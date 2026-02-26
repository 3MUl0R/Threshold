//! Schedule routes: list, toggle, delete.

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
        .route("/schedules", get(list))
        .route("/schedules/{id}/toggle", post(toggle))
        .route("/schedules/{id}/delete", post(delete))
}

/// GET /schedules — list all scheduled tasks.
async fn list(State(state): State<AppState>) -> Result<impl IntoResponse, WebError> {
    let handle = state
        .scheduler_handle
        .as_ref()
        .ok_or(WebError::SchedulerNotRunning)?;

    let mut tasks = handle.list_tasks().await.map_err(|e| {
        WebError::Internal(format!("Failed to list tasks: {e}"))
    })?;

    // Sort by next_run ascending (soonest first), None at end
    tasks.sort_by(|a, b| {
        match (a.next_run, b.next_run) {
            (Some(a_next), Some(b_next)) => a_next.cmp(&b_next),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        }
    });

    let task_data: Vec<minijinja::Value> = tasks
        .iter()
        .map(|t| {
            let kind_label = match t.kind {
                threshold_scheduler::task::TaskKind::Cron => "Cron",
                threshold_scheduler::task::TaskKind::Heartbeat => "Heartbeat",
            };
            let kind_badge = match t.kind {
                threshold_scheduler::task::TaskKind::Cron => "cron",
                threshold_scheduler::task::TaskKind::Heartbeat => "heartbeat",
            };
            let action_summary = match &t.action {
                threshold_core::types::ScheduledAction::NewConversation { prompt, .. } => {
                    truncate_str(prompt, 60)
                }
                threshold_core::types::ScheduledAction::ResumeConversation { prompt, .. } => {
                    format!("Resume: {}", truncate_str(prompt, 50))
                }
                threshold_core::types::ScheduledAction::Script { command, .. } => {
                    format!("Script: {}", truncate_str(command, 50))
                }
                threshold_core::types::ScheduledAction::ScriptThenConversation {
                    command, ..
                } => {
                    format!("Script+Conv: {}", truncate_str(command, 40))
                }
            };
            let last_result_summary = t.last_result.as_ref().map(|r| {
                let status = if r.success { "OK" } else { "FAIL" };
                format!("{status}: {}", truncate_str(&r.summary, 40))
            });

            minijinja::context! {
                id => t.id.to_string(),
                name => &t.name,
                kind_label => kind_label,
                kind_badge => kind_badge,
                cron_expression => &t.cron_expression,
                action_summary => action_summary,
                enabled => t.enabled,
                next_run => t.next_run.map(|dt| dt.to_rfc3339()),
                last_run => t.last_run.map(|dt| dt.to_rfc3339()),
                last_result_summary => last_result_summary,
                conversation_id => t.conversation_id.map(|id| id.0.to_string()),
            }
        })
        .collect();

    let csrf_token = csrf::generate_token();

    let tmpl = state
        .templates
        .get_template("schedules/list.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "schedules",
            tasks => task_data,
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
struct CsrfForm {
    _csrf: String,
}

/// POST /schedules/{id}/toggle — toggle a task enabled/disabled.
async fn toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<CsrfForm>,
) -> Result<Response, WebError> {
    crate::helpers::check_not_draining(&state)?;
    validate_csrf(&headers, &form._csrf)?;

    let handle = state
        .scheduler_handle
        .as_ref()
        .ok_or(WebError::SchedulerNotRunning)?;

    let task_id = id
        .parse::<uuid::Uuid>()
        .map_err(|_| WebError::NotFound(format!("Invalid task ID: {id}")))?;

    // Get current state to determine new toggle value
    let tasks = handle.list_tasks().await.map_err(|e| {
        WebError::Internal(format!("Failed to list tasks: {e}"))
    })?;
    let task = tasks
        .iter()
        .find(|t| t.id == task_id)
        .ok_or_else(|| WebError::NotFound(format!("Task not found: {id}")))?;

    let new_enabled = !task.enabled;
    handle.toggle_task(task_id, new_enabled).await.map_err(|e| {
        WebError::Internal(format!("Failed to toggle task: {e}"))
    })?;

    let status = if new_enabled { "enabled" } else { "disabled" };
    let flash = FlashMessage::success(format!("Task '{}' {status}", task.name));
    let flash_cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=10",
        crate::flash::COOKIE_NAME,
        flash.to_cookie_value()
    );

    Ok((
        [(header::SET_COOKIE, flash_cookie)],
        Redirect::to("/schedules"),
    )
        .into_response())
}

/// POST /schedules/{id}/delete — remove a scheduled task.
async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<CsrfForm>,
) -> Result<Response, WebError> {
    crate::helpers::check_not_draining(&state)?;
    validate_csrf(&headers, &form._csrf)?;

    let handle = state
        .scheduler_handle
        .as_ref()
        .ok_or(WebError::SchedulerNotRunning)?;

    let task_id = id
        .parse::<uuid::Uuid>()
        .map_err(|_| WebError::NotFound(format!("Invalid task ID: {id}")))?;

    handle.remove_task(task_id).await.map_err(|e| {
        WebError::Internal(format!("Failed to remove task: {e}"))
    })?;

    let flash = FlashMessage::success("Task deleted");
    let flash_cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=10",
        crate::flash::COOKIE_NAME,
        flash.to_cookie_value()
    );

    Ok((
        [(header::SET_COOKIE, flash_cookie)],
        Redirect::to("/schedules"),
    )
        .into_response())
}

/// Validate CSRF token from cookie and form field.
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

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max)])
    }
}
