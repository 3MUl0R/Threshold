//! Daemon health poller.
//!
//! Runs on a tokio runtime in a background thread, periodically connecting
//! to the daemon's Unix socket to check health status. State changes are
//! forwarded to the tray event loop via a channel.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::icons::TrayState;

/// Poll interval when daemon is running (responsive updates).
const POLL_RUNNING: Duration = Duration::from_secs(5);
/// Poll interval when daemon is stopped (reduce noise).
const POLL_STOPPED: Duration = Duration::from_secs(15);

/// Start the daemon health poller on the tokio runtime.
///
/// Returns a watch receiver that emits `TrayState` updates.
pub fn start_poller(data_dir: PathBuf) -> watch::Receiver<TrayState> {
    let (tx, rx) = watch::channel(TrayState::Stopped);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for poller");

        rt.block_on(async move {
            poll_loop(&data_dir, &tx).await;
        });
    });

    rx
}

async fn poll_loop(data_dir: &Path, tx: &watch::Sender<TrayState>) {
    let socket_path = data_dir.join("threshold.sock");

    loop {
        let state = check_health(&socket_path).await;
        let _ = tx.send(state);

        let interval = match state {
            TrayState::Running | TrayState::Draining => POLL_RUNNING,
            TrayState::Stopped | TrayState::Error => POLL_STOPPED,
        };

        tokio::time::sleep(interval).await;
    }
}

/// Connect to the daemon socket and check health.
async fn check_health(socket_path: &Path) -> TrayState {
    if !socket_path.exists() {
        return TrayState::Stopped;
    }

    // Also check PID file — if it doesn't exist, daemon is stopped
    let pid_path = socket_path.parent().unwrap_or(Path::new(".")).join("threshold.pid");
    if !pid_path.exists() {
        return TrayState::Stopped;
    }

    match try_health_check(socket_path).await {
        Ok(state) => state,
        Err(_) => {
            // Socket exists but can't connect — could be stale or daemon is starting
            TrayState::Error
        }
    }
}

async fn try_health_check(socket_path: &Path) -> anyhow::Result<TrayState> {
    let stream = tokio::net::UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send Health command using the versioned DaemonRequest envelope (NDJSON protocol)
    let cmd = serde_json::json!({"version": 1, "command": "Health"});
    let mut line = serde_json::to_string(&cmd)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;

    // Read response
    let mut response_line = String::new();
    let read_result = tokio::time::timeout(
        Duration::from_secs(5),
        reader.read_line(&mut response_line),
    )
    .await;

    match read_result {
        Ok(Ok(0)) | Err(_) => return Ok(TrayState::Error),
        Ok(Err(_)) => return Ok(TrayState::Error),
        Ok(Ok(_)) => {}
    }

    let resp: serde_json::Value = serde_json::from_str(&response_line)?;

    // Check response status — non-"ok" means the daemon reported an error
    let status = resp
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("error");

    if status != "ok" {
        return Ok(TrayState::Error);
    }

    // Check draining status
    let draining = resp
        .get("data")
        .and_then(|d| d.get("draining"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if draining {
        Ok(TrayState::Draining)
    } else {
        Ok(TrayState::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_check_returns_stopped_for_missing_socket() {
        let state = check_health(Path::new("/nonexistent/threshold.sock")).await;
        assert_eq!(state, TrayState::Stopped);
    }
}
