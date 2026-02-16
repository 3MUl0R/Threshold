//! Logging infrastructure with dual output: console and JSON file.
//!
//! # Architecture
//!
//! Logging uses `tracing` with two independent layers:
//! - **Console layer**: Pretty-formatted, colored output for interactive use
//! - **File layer**: JSON-formatted, non-blocking writes for production logs
//!
//! # Configuration
//!
//! Log level is configured via:
//! 1. `RUST_LOG` environment variable (takes precedence)
//! 2. Explicit `log_level` parameter (fallback)
//!
//! # Non-blocking I/O
//!
//! File writes use a background worker thread via `tracing_appender::non_blocking`.
//! The returned `WorkerGuard` MUST be kept alive for the program duration to
//! ensure buffered entries are flushed on shutdown.
//!
//! # Security
//!
//! On Unix systems, log files are created with permissions 0600 (owner read/write
//! only) to prevent unauthorized access to potentially sensitive log data.
//!
//! # Re-initialization
//!
//! Calling `init_logging` more than once will fail with `LoggingInit` error.
//! This is intentional - logging should be initialized once at startup.

use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter,
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
};

/// Initialize logging with console and file output.
///
/// # Returns
///
/// Returns a `WorkerGuard` that must be kept alive for the program duration.
/// When the guard is dropped, buffered log entries are flushed to disk.
///
/// # Errors
///
/// Returns `ThresholdError::LoggingInit` if:
/// - Log directory cannot be created
/// - Log file cannot be opened
/// - Invalid log level specified
/// - Logging is already initialized (can only init once)
///
/// # Example
///
/// ```no_run
/// use std::path::Path;
/// use threshold_core::logging::init_logging;
///
/// fn main() -> threshold_core::Result<()> {
///     let _guard = init_logging("info", Path::new("/var/log/threshold"))?;
///     // Keep _guard alive for entire program
///
///     tracing::info!("application started");
///     // ... rest of program ...
///
///     Ok(())
/// } // _guard dropped here, flushing buffered logs
/// ```
pub fn init_logging(log_level: &str, log_dir: &Path) -> crate::Result<WorkerGuard> {
    // Create log directory
    std::fs::create_dir_all(log_dir)
        .map_err(|e| crate::ThresholdError::LoggingInit(format!("create log dir: {}", e)))?;

    // Build env filter: RUST_LOG env var takes precedence, else use log_level
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(log_level))
        .map_err(|e| crate::ThresholdError::LoggingInit(format!("invalid log level: {}", e)))?;

    // Console layer: human-readable, colored
    let console_layer = tracing_subscriber::fmt::layer()
        .pretty()
        .with_filter(filter.clone());

    // File layer: JSON, non-blocking
    let log_file_path = log_dir.join("threshold.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
        .map_err(|e| crate::ThresholdError::LoggingInit(format!("open log file: {}", e)))?;

    // Set restrictive permissions (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = log_file
            .metadata()
            .map_err(|e| crate::ThresholdError::LoggingInit(format!("stat log file: {}", e)))?
            .permissions();
        perms.set_mode(0o600); // rw------- (owner only)
        std::fs::set_permissions(&log_file_path, perms)
            .map_err(|e| crate::ThresholdError::LoggingInit(format!("chmod log file: {}", e)))?;
    }

    let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_filter(filter);

    // Build and initialize subscriber
    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()
        .map_err(|e| crate::ThresholdError::LoggingInit(format!("subscriber init: {}", e)))?;

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn valid_log_level_initializes() {
        let dir = tempdir().unwrap();
        let result = init_logging("debug", dir.path());

        // First init should succeed
        assert!(result.is_ok());
    }

    #[test]
    fn invalid_log_level_returns_error() {
        // Test without actually calling try_init - just test filter parsing
        // Use clearly invalid syntax that will fail parsing
        let result = EnvFilter::try_new("invalid=[ syntax");
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn creates_log_directory() {
        let dir = tempdir().unwrap();
        let log_dir = dir.path().join("nested").join("logs");

        let _guard = init_logging("info", &log_dir).unwrap();

        assert!(log_dir.exists());
        assert!(log_dir.join("threshold.log").exists());
    }

    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn logging_writes_to_file() {
        let dir = tempdir().unwrap();
        let _guard = init_logging("debug", dir.path()).unwrap();

        tracing::info!("test message");

        // Guard drop flushes
        drop(_guard);

        let log_file = dir.path().join("threshold.log");
        assert!(log_file.exists());

        let content = std::fs::read_to_string(log_file).unwrap();
        assert!(content.contains("test message"));
    }

    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn second_init_fails() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();

        let _guard1 = init_logging("info", dir1.path()).unwrap();
        let result = init_logging("info", dir2.path());

        // Second init should fail
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::ThresholdError::LoggingInit(msg) => {
                assert!(msg.contains("subscriber init"));
            }
            _ => panic!("expected LoggingInit error"),
        }
    }

    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn rust_log_env_var_overrides() {
        let dir = tempdir().unwrap();

        // Set RUST_LOG to warn level
        // SAFETY: Test runs serially (#[serial]) so no data races
        unsafe {
            std::env::set_var("RUST_LOG", "warn");
        }

        let _guard = init_logging("debug", dir.path()).unwrap();

        // Verify subscriber was created (can't easily test the actual level
        // without complex introspection, but we verify it doesn't error)
        tracing::warn!("warn message");
        drop(_guard);

        let content = std::fs::read_to_string(dir.path().join("threshold.log")).unwrap();
        assert!(content.contains("warn message"));

        // Cleanup
        // SAFETY: Test runs serially (#[serial]) so no data races
        unsafe {
            std::env::remove_var("RUST_LOG");
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn log_file_created_with_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let _guard = init_logging("info", dir.path()).unwrap();

        let log_file = dir.path().join("threshold.log");
        let metadata = std::fs::metadata(&log_file).unwrap();
        let permissions = metadata.permissions();

        assert_eq!(permissions.mode() & 0o777, 0o600);
    }

    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn guard_drop_flushes_entries() {
        let dir = tempdir().unwrap();
        let guard = init_logging("trace", dir.path()).unwrap();

        // Write several log entries
        for i in 0..10 {
            tracing::trace!("entry {}", i);
        }

        // Drop guard to flush
        drop(guard);

        // Verify all entries were written
        let content = std::fs::read_to_string(dir.path().join("threshold.log")).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        // Should have 10 entries
        assert_eq!(lines.len(), 10);

        // Verify JSON format
        for line in lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("fields").is_some());
        }
    }

    #[test]
    #[serial]
    #[ignore] // Requires isolated execution - tracing subscriber can only init once
    fn nonexistent_parent_directory_created() {
        let dir = tempdir().unwrap();
        let deep_path = dir
            .path()
            .join("very")
            .join("deeply")
            .join("nested")
            .join("logs");

        let _guard = init_logging("info", &deep_path).unwrap();

        assert!(deep_path.exists());
        assert!(deep_path.join("threshold.log").exists());
    }
}
