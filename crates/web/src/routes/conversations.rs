//! Conversation routes: list, detail, audit trail, delete.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::csrf;
use crate::error::WebError;
use crate::flash::FlashMessage;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/conversations", get(list))
        .route("/conversations/{id}", get(detail))
        .route("/conversations/{id}/audit", get(audit_partial))
        .route("/conversations/{id}/delete", post(delete))
}

/// GET /conversations — list all conversations.
async fn list(State(state): State<AppState>) -> Result<impl IntoResponse, WebError> {
    let mut conversations = state.engine.list_conversations().await;
    // Sort by last_active descending (most recent first)
    conversations.sort_by(|a, b| b.last_active.cmp(&a.last_active));

    let portals = state.engine.portals();
    let portals_guard = portals.read().await;

    // Build template data
    let conv_data: Vec<minijinja::Value> = conversations
        .iter()
        .map(|c| {
            let portal_count = portals_guard.get_portals_for_conversation(&c.id).len();
            let mode_label = match &c.mode {
                threshold_core::types::ConversationMode::General => "General".to_string(),
                threshold_core::types::ConversationMode::Coding { project } => {
                    format!("Coding: {project}")
                }
                threshold_core::types::ConversationMode::Research { topic } => {
                    format!("Research: {topic}")
                }
            };
            let mode_badge = match &c.mode {
                threshold_core::types::ConversationMode::General => "general",
                threshold_core::types::ConversationMode::Coding { .. } => "coding",
                threshold_core::types::ConversationMode::Research { .. } => "research",
            };
            minijinja::context! {
                id => c.id.0.to_string(),
                mode_label => mode_label,
                mode_badge => mode_badge,
                agent_id => &c.agent_id,
                created_at => c.created_at.to_rfc3339(),
                last_active => c.last_active.to_rfc3339(),
                portal_count => portal_count,
                is_general => matches!(c.mode, threshold_core::types::ConversationMode::General),
            }
        })
        .collect();

    drop(portals_guard);

    let tmpl = state
        .templates
        .get_template("conversations/list.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "conversations",
            conversations => conv_data,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    Ok(Html(rendered))
}

/// GET /conversations/{id} — conversation detail page.
async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, WebError> {
    let conv_id = id
        .parse::<uuid::Uuid>()
        .map(threshold_core::types::ConversationId)
        .map_err(|_| WebError::NotFound(format!("Invalid conversation ID: {id}")))?;

    let conversations = state.engine.list_conversations().await;
    let conversation = conversations
        .iter()
        .find(|c| c.id == conv_id)
        .ok_or_else(|| WebError::NotFound(format!("Conversation not found: {id}")))?;

    let portals = state.engine.portals();
    let portals_guard = portals.read().await;
    let conv_portals = portals_guard.get_portals_for_conversation(&conv_id);

    let portal_data: Vec<minijinja::Value> = conv_portals
        .iter()
        .map(|p| {
            let portal_info = match &p.portal_type {
                threshold_core::types::PortalType::Discord {
                    guild_id,
                    channel_id,
                } => format!("Discord (guild: {guild_id}, channel: {channel_id})"),
            };
            minijinja::context! {
                id => p.id.0.to_string(),
                portal_type => portal_info,
                connected_at => p.connected_at.to_rfc3339(),
            }
        })
        .collect();

    drop(portals_guard);

    // Read memory.md preview if it exists
    let memory_path = state
        .engine
        .data_dir()
        .join("conversations")
        .join(conv_id.0.to_string())
        .join("memory.md");
    let memory_preview = tokio::fs::read_to_string(&memory_path)
        .await
        .ok()
        .map(|content| {
            // Truncate to first ~500 chars for preview
            if content.len() > 500 {
                format!("{}...", &content[..content.floor_char_boundary(500)])
            } else {
                content
            }
        });

    let mode_label = match &conversation.mode {
        threshold_core::types::ConversationMode::General => "General".to_string(),
        threshold_core::types::ConversationMode::Coding { project } => {
            format!("Coding: {project}")
        }
        threshold_core::types::ConversationMode::Research { topic } => {
            format!("Research: {topic}")
        }
    };
    let mode_badge = match &conversation.mode {
        threshold_core::types::ConversationMode::General => "general",
        threshold_core::types::ConversationMode::Coding { .. } => "coding",
        threshold_core::types::ConversationMode::Research { .. } => "research",
    };

    let csrf_token = csrf::generate_token();

    let tmpl = state
        .templates
        .get_template("conversations/detail.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "conversations",
            id => conv_id.0.to_string(),
            mode_label => mode_label,
            mode_badge => mode_badge,
            agent_id => &conversation.agent_id,
            created_at => conversation.created_at.to_rfc3339(),
            last_active => conversation.last_active.to_rfc3339(),
            portals => portal_data,
            memory_preview => memory_preview,
            is_general => matches!(conversation.mode, threshold_core::types::ConversationMode::General),
            csrf_token => &csrf_token,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    // Set CSRF cookie
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict",
        csrf::COOKIE_NAME,
        csrf_token
    );
    Ok(([(header::SET_COOKIE, cookie)], Html(rendered)))
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    25
}

/// GET /conversations/{id}/audit — htmx partial for audit entries.
async fn audit_partial(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AuditQuery>,
) -> Result<impl IntoResponse, WebError> {
    let conv_id = id
        .parse::<uuid::Uuid>()
        .map_err(|_| WebError::NotFound(format!("Invalid conversation ID: {id}")))?;

    // Read audit entries from JSONL file
    let audit_path = state
        .data_dir
        .join("audit")
        .join(format!("{conv_id}.jsonl"));

    let (entries, total) =
        crate::helpers::jsonl::read_jsonl_page(&audit_path, query.offset, query.limit).await?;

    let has_more = query.offset + entries.len() < total;
    let next_offset = query.offset + entries.len();

    let tmpl = state
        .templates
        .get_template("conversations/audit_partial.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            entries => entries,
            has_more => has_more,
            next_offset => next_offset,
            conversation_id => id,
            total => total,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    Ok(Html(rendered))
}

#[derive(Deserialize)]
struct DeleteForm {
    _csrf: String,
}

/// POST /conversations/{id}/delete — delete a conversation.
async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<DeleteForm>,
) -> Result<Response, WebError> {
    // Drain check
    crate::helpers::check_not_draining(&state)?;

    // Validate CSRF
    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::extract_cookie(cookie_header, csrf::COOKIE_NAME).unwrap_or_default();

    if !csrf::validate(&cookie_token, &form._csrf) {
        return Err(WebError::CsrfMismatch);
    }

    let conv_id = id
        .parse::<uuid::Uuid>()
        .map(threshold_core::types::ConversationId)
        .map_err(|_| WebError::NotFound(format!("Invalid conversation ID: {id}")))?;

    state
        .engine
        .delete_conversation(&conv_id)
        .await
        .map_err(|e| WebError::BadRequest(e.to_string()))?;

    // Set flash message and redirect
    let flash = FlashMessage::success(format!("Conversation {id} deleted"));
    let flash_cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=10",
        crate::flash::COOKIE_NAME,
        flash.to_cookie_value()
    );

    Ok((
        [(header::SET_COOKIE, flash_cookie)],
        Redirect::to("/conversations"),
    )
        .into_response())
}
