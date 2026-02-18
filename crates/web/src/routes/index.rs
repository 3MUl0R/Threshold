//! Dashboard routes: GET / and GET /status (JSON for htmx polling).

use axum::extract::State;
use axum::response::{Html, IntoResponse};
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
async fn dashboard(State(state): State<AppState>) -> Result<impl IntoResponse, WebError> {
    let conversation_count = state.engine.list_conversations().await.len();

    let schedule_count = match &state.scheduler_handle {
        Some(handle) => handle.list_tasks().await.map(|t| t.len()).unwrap_or(0),
        None => 0,
    };

    let tmpl = state
        .templates
        .get_template("index.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "dashboard",
            start_time => state.start_time.to_rfc3339(),
            conversation_count => conversation_count,
            schedule_count => schedule_count,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    Ok(Html(rendered))
}

/// GET /status — JSON status for htmx polling.
async fn status_json(State(state): State<AppState>) -> impl IntoResponse {
    let conversation_count = state.engine.list_conversations().await.len();

    let schedule_count = match &state.scheduler_handle {
        Some(handle) => handle.list_tasks().await.map(|t| t.len()).unwrap_or(0),
        None => 0,
    };

    let uptime = crate::helpers::time::format_duration_short(&state.start_time);

    axum::Json(serde_json::json!({
        "uptime": uptime,
        "conversation_count": conversation_count,
        "schedule_count": schedule_count,
        "scheduler_running": state.scheduler_handle.is_some(),
    }))
}
