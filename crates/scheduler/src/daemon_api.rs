//! Daemon API — Unix socket server for CLI-to-daemon communication.
//!
//! The daemon exposes a Unix domain socket at `~/.threshold/threshold.sock`.
//! Protocol: newline-delimited JSON (NDJSON) with request-response pattern.
//!
//! The API is decoupled from the scheduler — it always starts, even when the
//! scheduler is disabled. Scheduler commands return an error in that case.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use threshold_core::{DaemonState, HealthConfig};

use crate::engine::SchedulerHandle;
use crate::task::ScheduledTask;
use threshold_conversation::ConversationEngine;

/// Protocol version for daemon communication.
pub const PROTOCOL_VERSION: u32 = 1;

/// Request envelope sent from CLI to daemon (NDJSON framing).
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub version: u32,
    pub command: DaemonCommand,
}

/// Command payload within a daemon request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonCommand {
    /// Create a scheduled task.
    ScheduleCreate(ScheduledTask),
    /// List all scheduled tasks.
    ScheduleList,
    /// Delete a scheduled task by ID.
    ScheduleDelete { id: String },
    /// Toggle a scheduled task on/off.
    ScheduleToggle { id: String, enabled: bool },
    /// Query daemon health: PID, uptime, version, draining, active_work, task counts.
    Health,
    /// Enter drain mode: stop accepting new work, let in-flight work finish.
    Drain,
    /// Exit drain mode: resume accepting new work. Used for rollback if
    /// restart/stop fails after Drain but before SIGTERM.
    Undrain,
    /// List all active portals with their conversation assignments.
    PortalList,
}

/// Response envelope sent from daemon to CLI.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub version: u32,
    pub status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Status of a daemon response.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum ResponseStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
}

impl DaemonResponse {
    fn ok(data: serde_json::Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Ok,
            data: Some(data),
            message: None,
            code: None,
        }
    }

    fn ok_message(message: &str) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Ok,
            data: None,
            message: Some(message.to_string()),
            code: None,
        }
    }

    fn error(code: &str, message: &str) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Error,
            data: None,
            message: Some(message.to_string()),
            code: Some(code.to_string()),
        }
    }
}

/// Unix socket server for the daemon API.
///
/// Handles health checks, drain commands, and scheduler commands.
/// The scheduler is optional — when `None`, scheduler commands return an error
/// but health/drain commands still work.
pub struct DaemonApi {
    scheduler: Option<SchedulerHandle>,
    engine: Option<Arc<ConversationEngine>>,
    health_config: HealthConfig,
    daemon_state: Arc<DaemonState>,
    socket_path: PathBuf,
}

impl DaemonApi {
    /// Create a new daemon API server.
    pub fn new(
        scheduler: Option<SchedulerHandle>,
        engine: Option<Arc<ConversationEngine>>,
        health_config: HealthConfig,
        daemon_state: Arc<DaemonState>,
        socket_path: PathBuf,
    ) -> Self {
        Self {
            scheduler,
            engine,
            health_config,
            daemon_state,
            socket_path,
        }
    }

    /// Run the daemon API server.
    ///
    /// Binds to the Unix socket, handles stale sockets, and accepts connections
    /// until the cancellation token fires.
    pub async fn run(&self, cancel: CancellationToken) -> Result<(), anyhow::Error> {
        // Handle stale socket
        self.handle_stale_socket().await?;

        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!("Daemon API listening on {}", self.socket_path.display());

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let scheduler = self.scheduler.clone();
                            let engine = self.engine.clone();
                            let health_config = self.health_config.clone();
                            let daemon_state = self.daemon_state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_connection(
                                    stream, scheduler, engine, health_config, daemon_state,
                                ).await {
                                    tracing::debug!("Connection handler error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!("Failed to accept connection: {}", e);
                        }
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }

        // Clean up socket file
        tokio::fs::remove_file(&self.socket_path).await.ok();
        tracing::info!("Daemon API shut down, socket removed.");
        Ok(())
    }

    /// Handle a stale socket file.
    ///
    /// If the socket file already exists:
    /// - Try to connect to it — if succeeds, another daemon is running → error
    /// - If connection fails → stale socket → delete it
    async fn handle_stale_socket(&self) -> Result<(), anyhow::Error> {
        if !self.socket_path.exists() {
            // Create parent directory if needed
            if let Some(parent) = self.socket_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            return Ok(());
        }

        // Try to connect — if it works, another daemon is already running
        match UnixStream::connect(&self.socket_path).await {
            Ok(_) => {
                anyhow::bail!(
                    "Another daemon is already running (socket {} is active)",
                    self.socket_path.display()
                );
            }
            Err(_) => {
                // Stale socket — remove it
                tracing::info!("Removing stale socket: {}", self.socket_path.display());
                tokio::fs::remove_file(&self.socket_path).await?;
            }
        }

        Ok(())
    }

    /// Handle a single client connection.
    ///
    /// Reads one NDJSON request line, dispatches the command, and writes
    /// one NDJSON response line.
    async fn handle_connection(
        stream: UnixStream,
        scheduler: Option<SchedulerHandle>,
        engine: Option<Arc<ConversationEngine>>,
        health_config: HealthConfig,
        daemon_state: Arc<DaemonState>,
    ) -> Result<(), anyhow::Error> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        reader.read_line(&mut line).await?;

        let response = match serde_json::from_str::<DaemonRequest>(line.trim()) {
            Ok(request) => {
                if request.version != PROTOCOL_VERSION {
                    DaemonResponse::error(
                        "version_mismatch",
                        &format!(
                            "Unsupported protocol version {}. Expected {}.",
                            request.version, PROTOCOL_VERSION
                        ),
                    )
                } else {
                    Self::dispatch_command(
                        request.command,
                        &scheduler,
                        &engine,
                        &health_config,
                        &daemon_state,
                    )
                    .await
                }
            }
            Err(e) => DaemonResponse::error("invalid_input", &format!("Invalid request: {}", e)),
        };

        let mut response_json = serde_json::to_string(&response)?;
        response_json.push('\n');
        writer.write_all(response_json.as_bytes()).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Dispatch a command and build a response.
    async fn dispatch_command(
        command: DaemonCommand,
        scheduler: &Option<SchedulerHandle>,
        engine: &Option<Arc<ConversationEngine>>,
        health_config: &HealthConfig,
        daemon_state: &Arc<DaemonState>,
    ) -> DaemonResponse {
        match command {
            DaemonCommand::Health => {
                let uptime = Utc::now()
                    .signed_duration_since(health_config.started_at)
                    .num_seconds()
                    .max(0) as u64;

                // Compute scheduler task counts dynamically
                let (task_count, enabled_count) = if let Some(sched) = scheduler {
                    match sched.list_tasks().await {
                        Ok(tasks) => {
                            let total = tasks.len();
                            let enabled = tasks.iter().filter(|t| t.enabled).count();
                            (
                                Some(serde_json::Value::from(total)),
                                Some(serde_json::Value::from(enabled)),
                            )
                        }
                        Err(_) => (
                            Some(serde_json::Value::from(0)),
                            Some(serde_json::Value::from(0)),
                        ),
                    }
                } else {
                    (None, None)
                };

                DaemonResponse::ok(serde_json::json!({
                    "pid": health_config.pid,
                    "uptime_secs": uptime,
                    "version": health_config.version,
                    "draining": daemon_state.is_draining(),
                    "active_work": daemon_state.active_work(),
                    "scheduler_task_count": task_count,
                    "scheduler_enabled_count": enabled_count,
                }))
            }

            DaemonCommand::Drain => {
                daemon_state.set_draining(true);
                DaemonResponse::ok(serde_json::json!({
                    "draining": true,
                    "active_work": daemon_state.active_work(),
                }))
            }

            DaemonCommand::Undrain => {
                daemon_state.set_draining(false);
                DaemonResponse::ok(serde_json::json!({
                    "draining": false,
                    "active_work": daemon_state.active_work(),
                }))
            }

            // --- Scheduler commands: require scheduler to be enabled ---
            DaemonCommand::ScheduleCreate(task) => {
                let Some(sched) = scheduler else {
                    return DaemonResponse::error(
                        "scheduler_disabled",
                        "Scheduler is not enabled in this configuration",
                    );
                };
                let id = task.id;
                match sched.add_task(task).await {
                    Ok(()) => DaemonResponse::ok(serde_json::json!({ "id": id.to_string() })),
                    Err(e) => DaemonResponse::error("internal", &e.to_string()),
                }
            }
            DaemonCommand::ScheduleList => {
                let Some(sched) = scheduler else {
                    return DaemonResponse::error(
                        "scheduler_disabled",
                        "Scheduler is not enabled in this configuration",
                    );
                };
                match sched.list_tasks().await {
                    Ok(tasks) => {
                        let json = serde_json::to_value(&tasks).unwrap_or_default();
                        DaemonResponse::ok(json)
                    }
                    Err(e) => DaemonResponse::error("scheduler_shutdown", &e.to_string()),
                }
            }
            DaemonCommand::ScheduleDelete { id } => {
                let Some(sched) = scheduler else {
                    return DaemonResponse::error(
                        "scheduler_disabled",
                        "Scheduler is not enabled in this configuration",
                    );
                };
                let uuid = match Uuid::parse_str(&id) {
                    Ok(uuid) => uuid,
                    Err(_) => {
                        return DaemonResponse::error(
                            "invalid_input",
                            &format!("Invalid task ID: {}", id),
                        );
                    }
                };
                match sched.remove_task(uuid).await {
                    Ok(()) => DaemonResponse::ok_message(&format!("Task {} deleted", id)),
                    Err(e) => DaemonResponse::error("not_found", &e.to_string()),
                }
            }
            DaemonCommand::ScheduleToggle { id, enabled } => {
                let Some(sched) = scheduler else {
                    return DaemonResponse::error(
                        "scheduler_disabled",
                        "Scheduler is not enabled in this configuration",
                    );
                };
                let uuid = match Uuid::parse_str(&id) {
                    Ok(uuid) => uuid,
                    Err(_) => {
                        return DaemonResponse::error(
                            "invalid_input",
                            &format!("Invalid task ID: {}", id),
                        );
                    }
                };
                match sched.toggle_task(uuid, enabled).await {
                    Ok(()) => {
                        let state = if enabled { "enabled" } else { "disabled" };
                        DaemonResponse::ok_message(&format!("Task {} {}", id, state))
                    }
                    Err(e) => DaemonResponse::error("not_found", &e.to_string()),
                }
            }

            // --- Portal commands: require engine ---
            DaemonCommand::PortalList => {
                let Some(eng) = engine else {
                    return DaemonResponse::error(
                        "engine_unavailable",
                        "Conversation engine is not available",
                    );
                };
                let portals = eng.list_portals().await;
                let json: Vec<serde_json::Value> = portals
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "portal_id": p.portal_id.0.to_string(),
                            "portal_type": format!("{:?}", p.portal_type),
                            "conversation_id": p.conversation_id.0.to_string(),
                            "conversation_mode": p.conversation_mode.as_ref().map(|m| format!("{:?}", m)),
                            "is_primary": p.is_primary,
                            "connected_at": p.connected_at.to_rfc3339(),
                        })
                    })
                    .collect();
                DaemonResponse::ok(serde_json::json!(json))
            }
        }
    }

    /// Get the default socket path.
    pub fn default_socket_path(data_dir: &Path) -> PathBuf {
        data_dir.join("threshold.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_command_schedule_list_serde() {
        let cmd = DaemonCommand::ScheduleList;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#""ScheduleList""#);
    }

    #[test]
    fn daemon_command_schedule_delete_serde() {
        let cmd = DaemonCommand::ScheduleDelete {
            id: "abc-123".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let restored: DaemonCommand = serde_json::from_str(&json).unwrap();
        match restored {
            DaemonCommand::ScheduleDelete { id } => assert_eq!(id, "abc-123"),
            _ => panic!("Expected ScheduleDelete"),
        }
    }

    #[test]
    fn daemon_command_health_serde() {
        let cmd = DaemonCommand::Health;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#""Health""#);
        let restored: DaemonCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored, DaemonCommand::Health));
    }

    #[test]
    fn daemon_command_drain_serde() {
        let cmd = DaemonCommand::Drain;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#""Drain""#);
    }

    #[test]
    fn daemon_command_undrain_serde() {
        let cmd = DaemonCommand::Undrain;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#""Undrain""#);
    }

    #[test]
    fn daemon_request_serde_round_trip() {
        let req = DaemonRequest {
            version: PROTOCOL_VERSION,
            command: DaemonCommand::ScheduleList,
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, PROTOCOL_VERSION);
    }

    #[test]
    fn daemon_response_ok_serde() {
        let resp = DaemonResponse::ok(serde_json::json!({"id": "abc"}));
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status, ResponseStatus::Ok);
        assert!(restored.data.is_some());
        assert!(restored.message.is_none());
        assert!(restored.code.is_none());
    }

    #[test]
    fn daemon_response_error_serde() {
        let resp = DaemonResponse::error("not_found", "Task not found");
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status, ResponseStatus::Error);
        assert_eq!(restored.code.as_deref(), Some("not_found"));
        assert_eq!(restored.message.as_deref(), Some("Task not found"));
        assert!(restored.data.is_none());
    }

    #[test]
    fn daemon_response_ok_message_serde() {
        let resp = DaemonResponse::ok_message("Task deleted");
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status, ResponseStatus::Ok);
        assert_eq!(restored.message.as_deref(), Some("Task deleted"));
    }

    #[test]
    fn default_socket_path() {
        let path = DaemonApi::default_socket_path(Path::new("/home/user/.threshold"));
        assert_eq!(
            path.to_str().unwrap(),
            "/home/user/.threshold/threshold.sock"
        );
    }

    #[tokio::test]
    async fn health_without_scheduler() {
        let health_config = HealthConfig {
            pid: 12345,
            started_at: Utc::now(),
            version: "0.1.0".into(),
        };
        let daemon_state = Arc::new(DaemonState::new());

        // Health should work without scheduler
        let resp = DaemonApi::dispatch_command(
            DaemonCommand::Health,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        let data = resp.data.unwrap();
        assert_eq!(data["pid"], 12345);
        assert_eq!(data["version"], "0.1.0");
        assert!(data["scheduler_task_count"].is_null());
        assert!(data["scheduler_enabled_count"].is_null());

        // ScheduleList should fail without scheduler
        let resp = DaemonApi::dispatch_command(
            DaemonCommand::ScheduleList,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.code.as_deref(), Some("scheduler_disabled"));
    }

    #[tokio::test]
    async fn drain_sets_draining_flag() {
        let health_config = HealthConfig {
            pid: 1,
            started_at: Utc::now(),
            version: "0.1.0".into(),
        };
        let daemon_state = Arc::new(DaemonState::new());
        assert!(!daemon_state.is_draining());

        let resp = DaemonApi::dispatch_command(
            DaemonCommand::Drain,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert!(daemon_state.is_draining());
        let data = resp.data.unwrap();
        assert_eq!(data["draining"], true);
    }

    #[tokio::test]
    async fn health_reflects_draining() {
        let health_config = HealthConfig {
            pid: 1,
            started_at: Utc::now(),
            version: "0.1.0".into(),
        };
        let daemon_state = Arc::new(DaemonState::new());

        // Set draining via Drain command
        DaemonApi::dispatch_command(
            DaemonCommand::Drain,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;

        // Health should show draining: true
        let resp = DaemonApi::dispatch_command(
            DaemonCommand::Health,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;
        let data = resp.data.unwrap();
        assert_eq!(data["draining"], true);
    }

    #[tokio::test]
    async fn undrain_restores_normal() {
        let health_config = HealthConfig {
            pid: 1,
            started_at: Utc::now(),
            version: "0.1.0".into(),
        };
        let daemon_state = Arc::new(DaemonState::new());

        // Drain
        DaemonApi::dispatch_command(
            DaemonCommand::Drain,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;
        assert!(daemon_state.is_draining());

        // Undrain
        let resp = DaemonApi::dispatch_command(
            DaemonCommand::Undrain,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert!(!daemon_state.is_draining());

        // Health should confirm not draining
        let resp = DaemonApi::dispatch_command(
            DaemonCommand::Health,
            &None,
            &None,
            &health_config,
            &daemon_state,
        )
        .await;
        let data = resp.data.unwrap();
        assert_eq!(data["draining"], false);
    }
}
