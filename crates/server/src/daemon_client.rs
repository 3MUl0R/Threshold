//! Client for communicating with the Threshold daemon via Unix socket.
//!
//! The daemon exposes a Unix domain socket at `$DATA_DIR/threshold.sock` for
//! receiving commands from CLI subcommands (e.g., `threshold schedule`,
//! `threshold daemon status`).
//!
//! Some methods are forward-declared for Phase 16C (stop/restart commands).
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// Re-export protocol types from the scheduler crate for convenience.
#[allow(unused_imports)]
pub use threshold_scheduler::daemon_api::{
    DaemonCommand, DaemonRequest, DaemonResponse, ResponseStatus, PROTOCOL_VERSION,
};

/// Client for sending commands to the Threshold daemon.
pub struct DaemonClient {
    /// Path to the daemon's Unix domain socket.
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Create a new daemon client using the default socket path
    /// (`~/.threshold/threshold.sock`).
    ///
    /// Returns an error if the home directory cannot be determined.
    pub fn new() -> anyhow::Result<Self> {
        let socket_path = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
            .join(".threshold")
            .join("threshold.sock");
        Ok(Self { socket_path })
    }

    /// Create a daemon client with a specific socket path derived from the data dir.
    pub fn with_data_dir(data_dir: &std::path::Path) -> Self {
        Self {
            socket_path: data_dir.join("threshold.sock"),
        }
    }

    /// Send a command to the running daemon and await the response.
    ///
    /// The daemon must be running (`threshold daemon`) for this to succeed.
    /// Uses a 5-second connect timeout and 30-second response timeout.
    pub async fn send_command(
        &self,
        command: &DaemonCommand,
    ) -> anyhow::Result<DaemonResponse> {
        // Connect with timeout
        let stream = tokio::time::timeout(
            Duration::from_secs(5),
            UnixStream::connect(&self.socket_path),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Connection timeout — is the daemon running? \
                 Start it with: threshold daemon"
            )
        })?
        .map_err(|e| {
            anyhow::anyhow!(
                "Cannot connect to daemon at {}: {}. \
                 Start it with: threshold daemon",
                self.socket_path.display(),
                e
            )
        })?;

        let (reader, mut writer) = stream.into_split();

        // Build and send request
        let request = DaemonRequest {
            version: PROTOCOL_VERSION,
            command: command.clone(),
        };
        let mut request_json = serde_json::to_string(&request)?;
        request_json.push('\n');
        writer.write_all(request_json.as_bytes()).await?;
        writer.flush().await?;

        // Read response with timeout
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
            .await
            .map_err(|_| anyhow::anyhow!("Response timeout — daemon did not respond within 30s"))?
            .map_err(|e| anyhow::anyhow!("Failed to read response: {}", e))?;

        let response: DaemonResponse = serde_json::from_str(line.trim())?;
        Ok(response)
    }

    /// Send a Health check to the daemon.
    pub async fn send_health_check(&self) -> anyhow::Result<DaemonResponse> {
        self.send_command(&DaemonCommand::Health).await
    }

    /// Send a Drain command to the daemon.
    pub async fn send_drain(&self) -> anyhow::Result<DaemonResponse> {
        self.send_command(&DaemonCommand::Drain).await
    }

    /// Send an Undrain command to the daemon (rollback drain on failure).
    pub async fn send_undrain(&self) -> anyhow::Result<DaemonResponse> {
        self.send_command(&DaemonCommand::Undrain).await
    }
}

/// Resolve the data directory using the canonical chain:
/// 1. `explicit` (--data-dir CLI flag)
/// 2. `THRESHOLD_DATA_DIR` env var
/// 3. `THRESHOLD_CONFIG` env var → load config → data_dir field
/// 4. `~/.threshold` default
pub fn resolve_data_dir(explicit: Option<&str>) -> anyhow::Result<PathBuf> {
    // 1. Explicit --data-dir flag
    if let Some(dir) = explicit {
        return Ok(PathBuf::from(dir));
    }

    // 2. THRESHOLD_DATA_DIR env var
    if let Ok(dir) = std::env::var("THRESHOLD_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }

    // 3. THRESHOLD_CONFIG env var → load config → data_dir
    if let Ok(config_path) = std::env::var("THRESHOLD_CONFIG") {
        let config = threshold_core::config::ThresholdConfig::load_from(
            std::path::Path::new(&config_path),
        )?;
        return config
            .data_dir()
            .map_err(|e| anyhow::anyhow!("Failed to resolve data dir from config: {}", e));
    }

    // 4. Default: ~/.threshold
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".threshold"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn daemon_response_ok_serde_round_trip() {
        let resp = DaemonResponse {
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Ok,
            data: Some(serde_json::json!({"id": "abc-123"})),
            message: None,
            code: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, PROTOCOL_VERSION);
        assert_eq!(restored.status, ResponseStatus::Ok);
    }

    #[test]
    fn daemon_response_error_serde_round_trip() {
        let resp = DaemonResponse {
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Error,
            data: None,
            message: Some("Not found".into()),
            code: Some("not_found".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let restored: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status, ResponseStatus::Error);
        assert_eq!(restored.code.as_deref(), Some("not_found"));
    }

    #[test]
    fn client_with_data_dir() {
        let client = DaemonClient::with_data_dir(std::path::Path::new("/tmp/threshold"));
        assert_eq!(
            client.socket_path,
            PathBuf::from("/tmp/threshold/threshold.sock")
        );
    }

    #[test]
    fn health_command_serde() {
        let req = DaemonRequest {
            version: PROTOCOL_VERSION,
            command: DaemonCommand::Health,
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored.command, DaemonCommand::Health));
    }

    #[test]
    fn drain_command_serde() {
        let req = DaemonRequest {
            version: PROTOCOL_VERSION,
            command: DaemonCommand::Drain,
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored.command, DaemonCommand::Drain));
    }
}
