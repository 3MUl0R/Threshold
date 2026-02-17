//! Client for communicating with the Threshold daemon via Unix socket.
//!
//! The daemon exposes a Unix domain socket at `~/.threshold/threshold.sock` for
//! receiving commands from CLI subcommands (e.g., `threshold schedule`).
//! The actual socket protocol is implemented in Milestone 6.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Protocol version for daemon communication.
pub const PROTOCOL_VERSION: u32 = 1;

/// Request envelope sent from CLI to daemon (NDJSON framing).
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub version: u32,
    pub command: DaemonCommand,
}

/// Command payload within a daemon request.
#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonCommand {
    /// Create a scheduled conversation task
    ScheduleConversation {
        name: String,
        cron: String,
        prompt: String,
        model: Option<String>,
    },
    /// Create a scheduled script task
    ScheduleScript {
        name: String,
        cron: String,
        command: String,
        working_dir: Option<String>,
    },
    /// Create a scheduled monitoring task (script + AI analysis)
    ScheduleMonitor {
        name: String,
        cron: String,
        command: String,
        prompt_template: String,
        model: Option<String>,
    },
    /// List all scheduled tasks
    ScheduleList,
    /// Delete a scheduled task by ID
    ScheduleDelete { id: String },
    /// Toggle a scheduled task on/off
    ScheduleToggle { id: String, enabled: bool },
}

/// Response envelope sent from daemon to CLI.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub version: u32,
    pub status: ResponseStatus,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Status of a daemon response.
#[derive(Debug, Serialize, Deserialize)]
pub enum ResponseStatus {
    Ok,
    Error,
}

/// Client for sending commands to the Threshold daemon.
pub struct DaemonClient {
    /// Path to the daemon's Unix domain socket.
    #[allow(dead_code)]
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Create a new daemon client using the default socket path
    /// (`~/.threshold/threshold.sock`).
    pub fn new() -> Self {
        let socket_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".threshold")
            .join("threshold.sock");
        Self { socket_path }
    }

    /// Send a command to the running daemon and await the response.
    ///
    /// The daemon must be running (`threshold daemon`) for this to succeed.
    pub async fn send_command(
        &self,
        _command: &DaemonCommand,
    ) -> anyhow::Result<DaemonResponse> {
        // TODO(Milestone 6): Implement Unix socket communication
        // 1. Connect to self.socket_path via UnixStream
        // 2. Wrap command in DaemonRequest { version: PROTOCOL_VERSION, command }
        // 3. Serialize as NDJSON and send
        // 4. Read response line and deserialize DaemonResponse
        anyhow::bail!(
            "Daemon communication not yet implemented. \
             The daemon socket protocol is part of Milestone 6."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_request_serde_round_trip() {
        let req = DaemonRequest {
            version: PROTOCOL_VERSION,
            command: DaemonCommand::ScheduleConversation {
                name: "test".into(),
                cron: "0 * * * *".into(),
                prompt: "hello".into(),
                model: Some("sonnet".into()),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&restored).unwrap(), json);
    }

    #[test]
    fn daemon_command_list_serde() {
        let cmd = DaemonCommand::ScheduleList;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#""ScheduleList""#);
    }

    #[test]
    fn daemon_response_ok_serde_round_trip() {
        let resp = DaemonResponse {
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Ok,
            data: Some(serde_json::json!({"id": "abc-123"})),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&restored).unwrap(), json);
    }

    #[test]
    fn daemon_response_error_serde_round_trip() {
        let resp = DaemonResponse {
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Error,
            data: None,
            error: Some("Not found".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&restored).unwrap(), json);
    }
}
