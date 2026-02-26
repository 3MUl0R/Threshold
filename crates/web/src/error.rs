//! Web error types → HTML error responses.

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

/// Errors returned from web handlers.
#[derive(Debug)]
pub enum WebError {
    NotFound(String),
    Internal(String),
    BadRequest(String),
    SchedulerNotRunning,
    DaemonDraining,
    CsrfMismatch,
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebError::NotFound(msg) => write!(f, "Not found: {msg}"),
            WebError::Internal(msg) => write!(f, "Internal error: {msg}"),
            WebError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            WebError::SchedulerNotRunning => write!(f, "Scheduler is not running"),
            WebError::DaemonDraining => {
                write!(f, "Threshold is restarting — please try again shortly")
            }
            WebError::CsrfMismatch => write!(f, "Invalid CSRF token"),
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            WebError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            WebError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            WebError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            WebError::SchedulerNotRunning => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Scheduler is not running".into(),
            ),
            WebError::DaemonDraining => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Threshold is restarting — please try again shortly".into(),
            ),
            WebError::CsrfMismatch => (StatusCode::FORBIDDEN, "Invalid CSRF token".into()),
        };

        let body = format!(
            r#"<!DOCTYPE html>
<html lang="en" data-theme="dark">
<head><meta charset="utf-8"><title>Error {code}</title>
<link rel="stylesheet" href="/static/pico.min.css">
<link rel="stylesheet" href="/static/style.css">
</head>
<body><main class="container">
<h1>Error {code}</h1>
<p>{message}</p>
<p><a href="/">Back to Dashboard</a></p>
</main></body></html>"#,
            code = status.as_u16(),
            message = html_escape(&message),
        );

        (status, Html(body)).into_response()
    }
}

impl From<anyhow::Error> for WebError {
    fn from(err: anyhow::Error) -> Self {
        WebError::Internal(err.to_string())
    }
}

impl From<threshold_core::ThresholdError> for WebError {
    fn from(err: threshold_core::ThresholdError) -> Self {
        WebError::Internal(err.to_string())
    }
}

/// Minimal HTML escaping for error messages.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
