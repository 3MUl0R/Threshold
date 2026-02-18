//! Shared application state for the web server.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use threshold_conversation::ConversationEngine;
use threshold_core::config::ThresholdConfig;
use threshold_core::SecretStore;
use threshold_scheduler::SchedulerHandle;
use tokio_util::sync::CancellationToken;

/// Shared state injected into all Axum handlers via `State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<ConversationEngine>,
    pub scheduler_handle: Option<SchedulerHandle>,
    pub secret_store: Arc<SecretStore>,
    pub config: Arc<ThresholdConfig>,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub cancel: CancellationToken,
    pub start_time: DateTime<Utc>,
    pub templates: Arc<minijinja::Environment<'static>>,
}
