//! Route construction — mounts all route groups and static file serving.

pub mod conversations;
pub mod index;
pub mod schedules;

use axum::Router;
use tower_http::services::ServeDir;

use crate::state::AppState;

/// Build the complete Axum router with all routes and static files.
pub fn build_router(state: AppState) -> Router {
    // Static file serving from embedded bytes
    let static_service = ServeDir::new(static_dir());

    Router::new()
        .merge(index::router())
        .merge(conversations::router())
        .merge(schedules::router())
        .nest_service("/static", static_service)
        .with_state(state)
}

/// Path to the static files directory.
///
/// In development, serves from the crate's static/ directory.
/// In production, this should be the path where static files are deployed.
fn static_dir() -> std::path::PathBuf {
    // Try crate-relative path first (for development)
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
    if crate_dir.exists() {
        return crate_dir;
    }

    // Fallback: current directory's static/
    std::path::PathBuf::from("static")
}
