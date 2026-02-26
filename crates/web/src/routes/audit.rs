//! Audit log browser routes: tabbed view of all audit JSONL files.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde::Deserialize;

use crate::error::WebError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/audit", get(browser))
        .route("/audit/{source}", get(tab_partial))
}

/// Discover audit JSONL files and return their stem names.
fn discover_audit_files(data_dir: &std::path::Path) -> Vec<String> {
    let audit_dir = data_dir.join("audit");
    let mut stems = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&audit_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    stems.push(stem.to_string());
                }
            }
        }
    }

    stems.sort();
    stems
}

/// Validate a source name: must be an exact match in the discovered stems.
/// Rejects any path traversal attempts.
fn validate_source(source: &str, stems: &[String]) -> bool {
    // Reject path separators and traversal
    if source.contains('/') || source.contains('\\') || source.contains("..") {
        return false;
    }
    stems.iter().any(|s| s == source)
}

/// Derive a human-friendly tab label from a JSONL file stem.
fn tab_label(stem: &str) -> String {
    match stem {
        "gmail" => "Gmail".to_string(),
        "imagegen" => "Image Gen".to_string(),
        _ => {
            // Try to parse as UUID — show truncated if so
            if uuid::Uuid::parse_str(stem).is_ok() {
                format!("Conv: {}...", &stem[..8])
            } else {
                stem.to_string()
            }
        }
    }
}

/// GET /audit — audit log browser with tabs.
async fn browser(State(state): State<AppState>) -> Result<impl IntoResponse, WebError> {
    let stems = discover_audit_files(&state.data_dir);

    let tabs: Vec<minijinja::Value> = stems
        .iter()
        .map(|s| {
            minijinja::context! {
                source => s,
                label => tab_label(s),
            }
        })
        .collect();

    let tmpl = state
        .templates
        .get_template("audit/browser.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            nav_active => "audit",
            tabs => tabs,
            default_source => stems.first(),
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    Ok(Html(rendered))
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

/// GET /audit/{source} — htmx partial for a specific audit source tab.
async fn tab_partial(
    State(state): State<AppState>,
    Path(source): Path<String>,
    Query(query): Query<AuditQuery>,
) -> Result<impl IntoResponse, WebError> {
    let stems = discover_audit_files(&state.data_dir);

    // Security: validate source against discovered files (path traversal prevention)
    if !validate_source(&source, &stems) {
        return Err(WebError::NotFound(format!(
            "Audit source not found: {source}"
        )));
    }

    let audit_path = state.data_dir.join("audit").join(format!("{source}.jsonl"));

    let (entries, total) =
        crate::helpers::jsonl::read_jsonl_page(&audit_path, query.offset, query.limit).await?;

    let has_more = query.offset + entries.len() < total;
    let next_offset = query.offset + entries.len();

    let tmpl = state
        .templates
        .get_template("audit/tab_partial.html")
        .map_err(|e| WebError::Internal(format!("Template error: {e}")))?;

    let rendered = tmpl
        .render(minijinja::context! {
            entries => entries,
            has_more => has_more,
            next_offset => next_offset,
            source => &source,
            total => total,
        })
        .map_err(|e| WebError::Internal(format!("Render error: {e}")))?;

    Ok(Html(rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_source_rejects_traversal() {
        let stems = vec!["gmail".to_string(), "abc123".to_string()];
        assert!(!validate_source("../etc/passwd", &stems));
        assert!(!validate_source("foo/bar", &stems));
        assert!(!validate_source("..%2F..", &stems));
        assert!(!validate_source("a\\b", &stems));
    }

    #[test]
    fn validate_source_accepts_known_stems() {
        let stems = vec!["gmail".to_string(), "abc123".to_string()];
        assert!(validate_source("gmail", &stems));
        assert!(validate_source("abc123", &stems));
    }

    #[test]
    fn validate_source_rejects_unknown() {
        let stems = vec!["gmail".to_string()];
        assert!(!validate_source("unknown", &stems));
    }

    #[test]
    fn tab_label_known_sources() {
        assert_eq!(tab_label("gmail"), "Gmail");
        assert_eq!(tab_label("imagegen"), "Image Gen");
    }

    #[test]
    fn tab_label_uuid_truncated() {
        assert_eq!(
            tab_label("550e8400-e29b-41d4-a716-446655440000"),
            "Conv: 550e8400..."
        );
    }

    #[test]
    fn tab_label_unknown_passthrough() {
        assert_eq!(tab_label("custom-source"), "custom-source");
    }
}
