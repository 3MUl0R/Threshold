//! Client for communicating with the Threshold daemon via Unix socket.
//!
//! The daemon exposes a Unix domain socket at `~/.threshold/threshold.sock` for
//! receiving commands from CLI subcommands (e.g., `threshold schedule`).

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// Re-export protocol types from the scheduler crate for convenience.
pub use threshold_scheduler::daemon_api::{
    DaemonCommand, DaemonRequest, DaemonResponse, PROTOCOL_VERSION,
};

/// Client for sending commands to the Threshold daemon.
pub struct DaemonClient {
    /// Path to the daemon's Unix domain socket.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use threshold_scheduler::daemon_api::ResponseStatus;

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
}
