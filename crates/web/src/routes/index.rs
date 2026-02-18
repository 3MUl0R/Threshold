//! Dashboard routes: GET / and GET /status (JSON for htmx polling).

use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::error::WebError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/status", get(status_json))
}

/// GET / — Dashboard home page.
async fn dashboard(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, WebError> {
    let conversations = state.engine.list_conversations().await;
    let conversation_count = conversations.len();

    // Count recently active conversations (active within last hour)
    let one_hour_ago = chrono::Utc::now() - chrono::Duration::hours(1);
    let recently_active = conversations
        .iter()
        .filter(|c| c.last_active > one_hour_ago)
        .count();

    let schedule_count = match &state.scheduler_handle {
        Some(handle) => handle.list_tasks().await.map(|t| t.len()).unwrap_or(0),
        None => 0,
    };

    let scheduler_running = state.scheduler_handle.is_some();

    // Check for Discord portals as a proxy for "Discord connected"
    let portals = state.engine.portals();
    let portals_guard = portals.read().await;
    let discord_connected = conversations
        .iter()
        .any(|c| !portals_guard.get_portals_for_conversation(&c.id).is_empty());
    drop(portals_guard);

    // Read flash message
    let flash = read_flash(&headers);

    let tmpl = state
        .templates
        .get_template("index.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "dashboard",
            start_time => state.start_time.to_rfc3339(),
            conversation_count => conversation_count,
            recently_active => recently_active,
            schedule_count => schedule_count,
            scheduler_running => scheduler_running,
            discord_connected => discord_connected,
            flash => flash,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    // Clear flash cookie if one was read
    if flash.is_some() {
        let clear_cookie = format!(
            "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
            crate::flash::COOKIE_NAME
        );
        Ok(Response::builder()
            .header(header::SET_COOKIE, clear_cookie)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(rendered)
            .unwrap()
            .into_response())
    } else {
        Ok(Html(rendered).into_response())
    }
}

/// GET /status — JSON status for htmx polling.
async fn status_json(State(state): State<AppState>) -> impl IntoResponse {
    let conversations = state.engine.list_conversations().await;
    let conversation_count = conversations.len();

    let one_hour_ago = chrono::Utc::now() - chrono::Duration::hours(1);
    let recently_active = conversations
        .iter()
        .filter(|c| c.last_active > one_hour_ago)
        .count();

    let schedule_count = match &state.scheduler_handle {
        Some(handle) => handle.list_tasks().await.map(|t| t.len()).unwrap_or(0),
        None => 0,
    };

    let uptime = crate::helpers::time::format_duration_short(&state.start_time);

    axum::Json(serde_json::json!({
        "uptime": uptime,
        "conversation_count": conversation_count,
        "recently_active": recently_active,
        "schedule_count": schedule_count,
        "scheduler_running": state.scheduler_handle.is_some(),
    }))
}

/// Read and deserialize a flash message from cookies.
fn read_flash(headers: &axum::http::HeaderMap) -> Option<minijinja::Value> {
    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())?;
    let flash = crate::flash::read_flash(cookie_header)?;
    Some(minijinja::context! {
        level => flash.level,
        message => flash.message,
    })
}
