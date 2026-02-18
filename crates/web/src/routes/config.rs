//! Config editor and credentials management routes.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::csrf;
use crate::error::WebError;
use crate::flash::FlashMessage;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/config", get(editor))
        .route("/config", post(save_config))
        .route("/config/credentials", get(credentials))
        .route("/config/credentials/{key}", post(set_credential))
        .route("/config/credentials/{key}/delete", post(delete_credential))
}

/// GET /config — structured config editor.
async fn editor(State(state): State<AppState>) -> Result<impl IntoResponse, WebError> {
    // Serialize current config to TOML for display
    let config_toml = toml::to_string_pretty(&*state.config)
        .map_err(|e| WebError::Internal(format!("Failed to serialize config: {e}")))?;

    let csrf_token = csrf::generate_token();

    let tmpl = state
        .templates
        .get_template("config/editor.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "config",
            config_toml => config_toml,
            config_path => state.config_path.display().to_string(),
            csrf_token => &csrf_token,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict",
        csrf::COOKIE_NAME, csrf_token
    );
    Ok(([(header::SET_COOKIE, cookie)], Html(rendered)))
}

#[derive(Deserialize)]
struct ConfigForm {
    _csrf: String,
    config_toml: String,
}

/// POST /config — save edited config (atomic write: tmp + rename).
async fn save_config(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<ConfigForm>,
) -> Result<Response, WebError> {
    validate_csrf(&headers, &form._csrf)?;

    // Parse the TOML to validate it
    let new_config: threshold_core::config::ThresholdConfig =
        toml::from_str(&form.config_toml).map_err(|e| {
            WebError::BadRequest(format!("Invalid TOML: {e}"))
        })?;

    // Validate the config
    new_config.validate().map_err(|e| {
        WebError::BadRequest(format!("Config validation failed: {e}"))
    })?;

    // Atomic write: write to tmp file, then rename
    let config_path = &state.config_path;
    let tmp_path = config_path.with_extension("toml.tmp");

    tokio::fs::write(&tmp_path, &form.config_toml)
        .await
        .map_err(|e| WebError::Internal(format!("Failed to write temp config: {e}")))?;

    tokio::fs::rename(&tmp_path, config_path)
        .await
        .map_err(|e| {
            // Clean up tmp file on failure
            let _ = std::fs::remove_file(&tmp_path);
            WebError::Internal(format!("Failed to save config: {e}"))
        })?;

    let flash = FlashMessage::success(
        "Config saved. Restart the daemon for changes to take effect.",
    );
    let flash_cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=10",
        crate::flash::COOKIE_NAME,
        flash.to_cookie_value()
    );

    Ok((
        [(header::SET_COOKIE, flash_cookie)],
        Redirect::to("/config"),
    )
        .into_response())
}

/// GET /config/credentials — credential status page.
async fn credentials(State(state): State<AppState>) -> Result<impl IntoResponse, WebError> {
    // Build a list of known credential keys and their status
    let mut cred_entries = Vec::new();

    // Always check these core credentials
    let core_keys = vec![
        ("discord-bot-token", "Discord Bot Token"),
        ("google-api-key", "Google API Key (Image Gen)"),
        ("gmail-oauth-client-id", "Gmail OAuth Client ID"),
        ("gmail-oauth-client-secret", "Gmail OAuth Client Secret"),
    ];

    for (key, label) in &core_keys {
        let configured = tokio::task::spawn_blocking({
            let store = state.secret_store.clone();
            let key = key.to_string();
            move || store.get(&key).ok().flatten().is_some()
        })
        .await
        .unwrap_or(false);

        cred_entries.push(minijinja::context! {
            key => key,
            label => label,
            configured => configured,
        });
    }

    // Add per-inbox Gmail refresh tokens
    if let Some(gmail_config) = state.config.tools.gmail.as_ref() {
        if let Some(inboxes) = &gmail_config.inboxes {
            for inbox in inboxes {
                let key = format!("gmail-oauth-refresh-token-{inbox}");
                let label = format!("Gmail Refresh Token ({inbox})");
                let configured = tokio::task::spawn_blocking({
                    let store = state.secret_store.clone();
                    let key = key.clone();
                    move || store.get(&key).ok().flatten().is_some()
                })
                .await
                .unwrap_or(false);

                cred_entries.push(minijinja::context! {
                    key => key,
                    label => label,
                    configured => configured,
                });
            }
        }
    }

    let csrf_token = csrf::generate_token();

    let tmpl = state
        .templates
        .get_template("config/credentials.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "config",
            credentials => cred_entries,
            csrf_token => &csrf_token,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict",
        csrf::COOKIE_NAME, csrf_token
    );
    Ok(([(header::SET_COOKIE, cookie)], Html(rendered)))
}

#[derive(Deserialize)]
struct CredentialForm {
    _csrf: String,
    value: String,
}

/// POST /config/credentials/{key} — set a credential value.
async fn set_credential(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<CredentialForm>,
) -> Result<Response, WebError> {
    validate_csrf(&headers, &form._csrf)?;

    // Validate the key is alphanumeric + dashes (no path traversal)
    if !key.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(WebError::BadRequest("Invalid credential key".into()));
    }

    if form.value.is_empty() {
        return Err(WebError::BadRequest("Credential value cannot be empty".into()));
    }

    tokio::task::spawn_blocking({
        let store = state.secret_store.clone();
        let key = key.clone();
        let value = form.value.clone();
        move || store.set(&key, &value)
    })
    .await
    .map_err(|e| WebError::Internal(format!("spawn_blocking error: {e}")))?
    .map_err(|e| WebError::Internal(format!("Failed to set credential: {e}")))?;

    let flash = FlashMessage::success(format!("Credential '{key}' updated"));
    let flash_cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=10",
        crate::flash::COOKIE_NAME,
        flash.to_cookie_value()
    );

    Ok((
        [(header::SET_COOKIE, flash_cookie)],
        Redirect::to("/config/credentials"),
    )
        .into_response())
}

/// POST /config/credentials/{key}/delete — delete a credential.
async fn delete_credential(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<CsrfOnlyForm>,
) -> Result<Response, WebError> {
    validate_csrf(&headers, &form._csrf)?;

    // Validate key format
    if !key.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(WebError::BadRequest("Invalid credential key".into()));
    }

    tokio::task::spawn_blocking({
        let store = state.secret_store.clone();
        let key = key.clone();
        move || store.delete(&key)
    })
    .await
    .map_err(|e| WebError::Internal(format!("spawn_blocking error: {e}")))?
    .map_err(|e| WebError::Internal(format!("Failed to delete credential: {e}")))?;

    let flash = FlashMessage::success(format!("Credential '{key}' deleted"));
    let flash_cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=10",
        crate::flash::COOKIE_NAME,
        flash.to_cookie_value()
    );

    Ok((
        [(header::SET_COOKIE, flash_cookie)],
        Redirect::to("/config/credentials"),
    )
        .into_response())
}

#[derive(Deserialize)]
struct CsrfOnlyForm {
    _csrf: String,
}

fn validate_csrf(headers: &axum::http::HeaderMap, form_token: &str) -> Result<(), WebError> {
    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::extract_cookie(cookie_header, csrf::COOKIE_NAME).unwrap_or_default();

    if !csrf::validate(&cookie_token, form_token) {
        return Err(WebError::CsrfMismatch);
    }
    Ok(())
}
