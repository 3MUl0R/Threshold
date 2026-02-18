//! Log viewer routes: structured log file browser with filtering.

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::error::WebError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/logs", get(viewer))
        .route("/logs/entries", get(entries_partial))
}

/// Discover log files matching `threshold.log*` in data_dir/logs/.
fn discover_log_files(data_dir: &std::path::Path) -> Vec<String> {
    let logs_dir = data_dir.join("logs");
    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("threshold.log") {
                    files.push(name.to_string());
                }
            }
        }
    }

    files.sort();
    files
}

/// Validate a log filename: must be an exact match in discovered files.
/// Rejects any path traversal attempts.
fn validate_log_file(file: &str, files: &[String]) -> bool {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return false;
    }
    files.iter().any(|f| f == file)
}

/// GET /logs — log viewer page.
async fn viewer(State(state): State<AppState>) -> Result<impl IntoResponse, WebError> {
    let files = discover_log_files(&state.data_dir);

    let tmpl = state
        .templates
        .get_template("logs/viewer.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "logs",
            files => files,
            default_file => files.first(),
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    Ok(Html(rendered))
}

#[derive(Deserialize)]
struct LogQuery {
    file: Option<String>,
    #[serde(default)]
    level: String,
    #[serde(default)]
    search: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    100
}

/// GET /logs/entries — htmx partial with filtered log entries.
async fn entries_partial(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> Result<impl IntoResponse, WebError> {
    let files = discover_log_files(&state.data_dir);

    let file = query.file.unwrap_or_else(|| {
        files.first().cloned().unwrap_or_default()
    });

    if file.is_empty() {
        return Ok(Html("<p class=\"empty-state\">No log files found.</p>".to_string()));
    }

    // Security: validate file against discovered log files
    if !validate_log_file(&file, &files) {
        return Err(WebError::BadRequest(format!(
            "Invalid log file: {file}"
        )));
    }

    let log_path = state.data_dir.join("logs").join(&file);

    // Read log file lines
    let content = tokio::fs::read_to_string(&log_path)
        .await
        .unwrap_or_default();

    let level_filters: Vec<&str> = if query.level.is_empty() {
        vec![]
    } else {
        query.level.split(',').collect()
    };

    let search_lower = query.search.to_lowercase();

    // Parse and filter log entries (each line is JSON from tracing-subscriber)
    let mut entries: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| {
            let parsed: serde_json::Value = serde_json::from_str(line).ok()?;

            // Filter by level if specified
            if !level_filters.is_empty() {
                let entry_level = parsed
                    .get("level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                if !level_filters.iter().any(|l| l.eq_ignore_ascii_case(&entry_level)) {
                    return None;
                }
            }

            // Filter by search term
            if !search_lower.is_empty() {
                let line_lower = line.to_lowercase();
                if !line_lower.contains(&search_lower) {
                    return None;
                }
            }

            Some(parsed)
        })
        .collect();

    // Reverse to show newest first
    entries.reverse();

    let total = entries.len();
    let entries: Vec<serde_json::Value> = entries
        .into_iter()
        .skip(query.offset)
        .take(query.limit)
        .collect();

    let has_more = query.offset + entries.len() < total;
    let next_offset = query.offset + entries.len();

    let tmpl = state
        .templates
        .get_template("logs/entries_partial.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            entries => entries,
            has_more => has_more,
            next_offset => next_offset,
            total => total,
            file => &file,
            level => &query.level,
            search => &query.search,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    Ok(Html(rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_log_file_rejects_traversal() {
        let files = vec!["threshold.log".to_string()];
        assert!(!validate_log_file("../../../etc/passwd", &files));
        assert!(!validate_log_file("foo/bar", &files));
        assert!(!validate_log_file("..\\windows", &files));
    }

    #[test]
    fn validate_log_file_accepts_known() {
        let files = vec![
            "threshold.log".to_string(),
            "threshold.log.2024-01-15".to_string(),
        ];
        assert!(validate_log_file("threshold.log", &files));
        assert!(validate_log_file("threshold.log.2024-01-15", &files));
    }

    #[test]
    fn validate_log_file_rejects_unknown() {
        let files = vec!["threshold.log".to_string()];
        assert!(!validate_log_file("other.log", &files));
    }
}
