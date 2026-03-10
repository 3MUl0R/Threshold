//! CLI subprocess spawning and execution.
//!
//! Handles spawning the Claude CLI as a subprocess with proper timeout,
//! environment variable clearing, and error classification.

use crate::stream::{self, StreamEvent};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use threshold_core::{Result, ThresholdError};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Configuration for CLI process spawning
#[derive(Debug, Clone)]
pub struct CliProcess {
    command: String,
    timeout_secs: u64,
    env_clear: Vec<String>,
}

/// Output from a CLI execution
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration: Duration,
}

/// Handle returned from `run_streaming()`.
pub struct StreamHandle {
    /// Receiver for parsed stream events.
    pub event_rx: tokio::sync::mpsc::Receiver<StreamEvent>,
    /// Child process PID (for diagnostics).
    pub pid: u32,
    /// Whether the stream ended because of a user-initiated abort.
    /// Set by the reader task when the abort token fires.
    pub was_aborted: Arc<std::sync::atomic::AtomicBool>,
}

impl CliProcess {
    /// Create a new CLI process with default settings
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout_secs: 300, // 5 minutes default
            env_clear: vec![
                "ANTHROPIC_API_KEY".to_string(),
                "ANTHROPIC_API_KEY_OLD".to_string(),
                // Prevent "cannot be launched inside another Claude Code
                // session" when the daemon itself was started from Claude Code.
                "CLAUDECODE".to_string(),
            ],
        }
    }

    /// Set a custom timeout in seconds
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Run the CLI with the given arguments
    ///
    /// # Arguments
    /// * `args` - Command-line arguments
    /// * `working_dir` - Optional working directory
    /// * `stdin_data` - Optional data to write to stdin
    pub async fn run(
        &self,
        args: &[String],
        working_dir: Option<&Path>,
        stdin_data: Option<&str>,
    ) -> Result<CliOutput> {
        let start = std::time::Instant::now();

        // Log the command (redact sensitive args)
        tracing::debug!(
            command = %self.command,
            args = ?self.redact_args(args),
            working_dir = ?working_dir,
            "spawning CLI process"
        );

        // Build command
        let mut cmd = Command::new(&self.command);
        cmd.args(args);

        // Clear sensitive env vars
        for key in &self.env_clear {
            cmd.env_remove(key);
        }

        // Set working directory
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        // Configure stdio
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        if stdin_data.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }
        // Kill child process when the Child handle is dropped (e.g. future
        // cancelled by tokio::time::timeout). Prevents orphaned processes.
        cmd.kill_on_drop(true);

        // Spawn
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ThresholdError::CliNotFound {
                    command: self.command.clone(),
                }
            } else {
                ThresholdError::Io(e)
            }
        })?;

        // Write stdin if provided
        if let Some(data) = stdin_data {
            use tokio::io::AsyncWriteExt;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(data.as_bytes()).await?;
                stdin.shutdown().await?;
            }
        }

        // Capture stdout/stderr handles before waiting
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Drain stdout/stderr concurrently with wait to avoid pipe buffer deadlock.
        // If the child produces more than ~64KB, it blocks writing to the pipe.
        // We must read while waiting, not after.
        use tokio::io::AsyncReadExt;
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::BufReader::new(stdout)
                .read_to_end(&mut buf)
                .await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::BufReader::new(stderr)
                .read_to_end(&mut buf)
                .await;
            buf
        });

        // Wait with timeout (0 = no timeout)
        if self.timeout_secs == 0 {
            let status = match child.wait().await {
                Ok(s) => s,
                Err(e) => {
                    stdout_task.abort();
                    stderr_task.abort();
                    return Err(ThresholdError::Io(e));
                }
            };
            let stdout_buf = stdout_task.await.unwrap_or_default();
            let stderr_buf = stderr_task.await.unwrap_or_default();
            self.build_output(start, status, &stdout_buf, &stderr_buf)
        } else {
            tokio::select! {
                result = child.wait() => {
                    let status = match result {
                        Ok(s) => s,
                        Err(e) => {
                            stdout_task.abort();
                            stderr_task.abort();
                            return Err(ThresholdError::Io(e));
                        }
                    };
                    let stdout_buf = stdout_task.await.unwrap_or_default();
                    let stderr_buf = stderr_task.await.unwrap_or_default();
                    self.build_output(start, status, &stdout_buf, &stderr_buf)
                }
                _ = tokio::time::sleep(Duration::from_secs(self.timeout_secs)) => {
                    let _ = child.kill().await;
                    stdout_task.abort();
                    stderr_task.abort();
                    Err(ThresholdError::CliTimeout {
                        timeout_ms: self.timeout_secs * 1000,
                    })
                }
            }
        }
    }

    /// Run the CLI in streaming mode with `--output-format stream-json`.
    ///
    /// Spawns the child process and a reader task that parses JSONL lines from
    /// stdout into `StreamEvent`s. Returns a `StreamHandle` containing:
    /// - An mpsc receiver for stream events
    /// - The child PID (for diagnostics)
    /// - An `was_aborted` flag for the caller to distinguish user abort from errors
    ///
    /// The reader monitors both the `abort_token` and `timeout_secs`; on
    /// cancellation or timeout it kills the child. After stdout EOF the task
    /// waits on the child exit status and sends a classified error event if
    /// the exit code is non-zero and no `Result` event was produced.
    pub async fn run_streaming(
        &self,
        args: &[String],
        working_dir: Option<&Path>,
        stdin_data: Option<&str>,
        abort_token: CancellationToken,
    ) -> Result<StreamHandle> {
        tracing::debug!(
            command = %self.command,
            args = ?self.redact_args(args),
            "spawning streaming CLI process"
        );

        let mut cmd = Command::new(&self.command);
        cmd.args(args);

        for key in &self.env_clear {
            cmd.env_remove(key);
        }
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        if stdin_data.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ThresholdError::CliNotFound {
                    command: self.command.clone(),
                }
            } else {
                ThresholdError::Io(e)
            }
        })?;

        if let Some(data) = stdin_data {
            use tokio::io::AsyncWriteExt;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(data.as_bytes()).await?;
                stdin.shutdown().await?;
            }
        }

        let pid = child.id().unwrap_or(0);
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Drain stderr in background (for error diagnostics)
        let stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            let _ = tokio::io::BufReader::new(stderr)
                .read_to_end(&mut buf)
                .await;
            String::from_utf8_lossy(&buf).to_string()
        });

        // Channel for parsed stream events
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);

        // Abort flag — set by the reader task when the abort token fires
        let was_aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let was_aborted_clone = was_aborted.clone();

        // Timeout (0 = no timeout)
        let timeout_secs = self.timeout_secs;

        // Reader task: read stdout line-by-line, parse, send events,
        // then wait on child exit and classify errors.
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            let mut saw_result = false;

            let deadline = if timeout_secs > 0 {
                Some(tokio::time::Instant::now() + Duration::from_secs(timeout_secs))
            } else {
                None
            };

            loop {
                tokio::select! {
                    line = reader.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                let events = stream::parse_stream_line(&line);
                                for event in events {
                                    if matches!(event, StreamEvent::Result { .. }) {
                                        saw_result = true;
                                    }
                                    if event_tx.send(event).await.is_err() {
                                        // Receiver dropped — kill child to avoid
                                        // pipe-buffer deadlock on wait().
                                        let _ = child.kill().await;
                                        let _ = child.wait().await;
                                        stderr_task.abort();
                                        return;
                                    }
                                }
                            }
                            Ok(None) => break, // EOF — process exited
                            Err(e) => {
                                let _ = event_tx.send(StreamEvent::Error {
                                    message: format!("stdout read error: {e}"),
                                }).await;
                                break;
                            }
                        }
                    }
                    _ = abort_token.cancelled() => {
                        was_aborted_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                        let _ = child.kill().await;
                        let _ = child.wait().await; // reap zombie
                        stderr_task.abort();
                        return; // don't send error event — caller checks was_aborted
                    }
                    _ = async {
                        match deadline {
                            Some(dl) => tokio::time::sleep_until(dl).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        let _ = event_tx.send(StreamEvent::Error {
                            message: format!("CLI timed out after {}s", timeout_secs),
                        }).await;
                        stderr_task.abort();
                        return;
                    }
                }
            }

            // Wait on child exit and classify errors if no Result event was seen
            let exit_status = child.wait().await;
            let stderr_output = stderr_task.await.unwrap_or_default();

            if !stderr_output.is_empty() {
                tracing::debug!(stderr = %stderr_output, "streaming CLI stderr");
            }

            if !saw_result {
                // Process exited without emitting a `result` event —
                // classify using exit code + stderr, same as non-streaming path.
                let exit_code = exit_status.ok().and_then(|s| s.code()).unwrap_or(-1);

                if exit_code != 0 || !stderr_output.is_empty() {
                    let stderr_lower = stderr_output.to_lowercase();
                    let message = if stderr_lower.contains("401")
                        || stderr_lower.contains("unauthorized")
                    {
                        "Authentication expired. Please re-authenticate.".to_string()
                    } else if stderr_lower.contains("402") || stderr_lower.contains("payment") {
                        "Billing issue detected.".to_string()
                    } else if stderr_lower.contains("429") || stderr_lower.contains("rate limit") {
                        "Rate limited. Please try again later.".to_string()
                    } else if stderr_output.is_empty() {
                        format!("CLI process exited with code {exit_code}")
                    } else {
                        stderr_output
                    };

                    let _ = event_tx.send(StreamEvent::Error { message }).await;
                }
            }
        });

        Ok(StreamHandle {
            event_rx,
            pid,
            was_aborted,
        })
    }

    fn build_output(
        &self,
        start: std::time::Instant,
        status: std::process::ExitStatus,
        stdout_buf: &[u8],
        stderr_buf: &[u8],
    ) -> Result<CliOutput> {
        let exit_code = status.code().unwrap_or(-1);
        let stdout_str = String::from_utf8_lossy(stdout_buf).to_string();
        let stderr_str = String::from_utf8_lossy(stderr_buf).to_string();
        let duration = start.elapsed();

        tracing::debug!(
            exit_code,
            duration_ms = duration.as_millis(),
            stdout_len = stdout_str.len(),
            stderr_len = stderr_str.len(),
            "CLI process completed"
        );

        if exit_code != 0 {
            return Err(self.classify_error(exit_code, &stderr_str));
        }

        Ok(CliOutput {
            stdout: stdout_str,
            stderr: stderr_str,
            exit_code,
            duration,
        })
    }

    fn classify_error(&self, exit_code: i32, stderr: &str) -> ThresholdError {
        let stderr_lower = stderr.to_lowercase();

        if stderr_lower.contains("401") || stderr_lower.contains("unauthorized") {
            ThresholdError::CliError {
                provider: "claude".to_string(),
                code: exit_code,
                stderr: "Authentication expired. Please re-authenticate.".to_string(),
            }
        } else if stderr_lower.contains("402") || stderr_lower.contains("payment") {
            ThresholdError::CliError {
                provider: "claude".to_string(),
                code: exit_code,
                stderr: "Billing issue detected.".to_string(),
            }
        } else if stderr_lower.contains("429") || stderr_lower.contains("rate limit") {
            ThresholdError::CliError {
                provider: "claude".to_string(),
                code: exit_code,
                stderr: "Rate limited. Please try again later.".to_string(),
            }
        } else {
            ThresholdError::CliError {
                provider: "claude".to_string(),
                code: exit_code,
                stderr: stderr.to_string(),
            }
        }
    }

    fn redact_args(&self, args: &[String]) -> Vec<String> {
        // Redact values after --append-system-prompt
        let mut redacted = Vec::new();
        let mut skip_next = false;

        for arg in args {
            if skip_next {
                redacted.push("<redacted>".to_string());
                skip_next = false;
            } else if arg == "--append-system-prompt" {
                redacted.push(arg.clone());
                skip_next = true;
            } else {
                redacted.push(arg.clone());
            }
        }

        redacted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cli_not_found_returns_error() {
        let process = CliProcess::new("nonexistent-command-xyz");
        let result = process.run(&[], None, None).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ThresholdError::CliNotFound { command } => {
                assert_eq!(command, "nonexistent-command-xyz");
            }
            _ => panic!("expected CliNotFound error"),
        }
    }

    #[tokio::test]
    async fn successful_command_returns_output() {
        let process = CliProcess::new("echo");
        let result = process
            .run(&["hello".to_string(), "world".to_string()], None, None)
            .await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("hello world"));
    }

    #[tokio::test]
    async fn timeout_kills_hanging_process() {
        let process = CliProcess::new("sleep").with_timeout(1);
        let result = process.run(&["10".to_string()], None, None).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ThresholdError::CliTimeout { .. } => {}
            e => panic!("expected CliTimeout, got {:?}", e),
        }
    }

    #[tokio::test]
    #[ignore] // Requires claude CLI installed
    async fn claude_version_check() {
        let process = CliProcess::new("claude");
        let result = process.run(&["--version".to_string()], None, None).await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("claude") || output.stdout.contains("version"));
    }

    #[tokio::test]
    async fn concurrent_drain_large_output() {
        // Produce >64KB of output to verify no pipe buffer deadlock.
        // `yes` outputs "y\n" endlessly; `head -c 100000` truncates to 100KB.
        let process = CliProcess::new("sh").with_timeout(10);
        let result = process
            .run(
                &["-c".to_string(), "yes | head -c 100000".to_string()],
                None,
                None,
            )
            .await;

        assert!(result.is_ok(), "large output should not deadlock");
        let output = result.unwrap();
        assert!(
            output.stdout.len() >= 100_000,
            "expected >=100KB stdout, got {} bytes",
            output.stdout.len()
        );
    }

    #[tokio::test]
    async fn no_timeout_when_zero() {
        // timeout_secs = 0 should not impose any timeout
        let process = CliProcess::new("sleep").with_timeout(0);
        let result = process.run(&["0.1".to_string()], None, None).await;
        assert!(result.is_ok(), "zero timeout should not fire");
    }

    #[tokio::test]
    async fn streaming_produces_events() {
        // Use sh -c to echo JSONL lines that simulate Claude CLI stream-json output.
        // The CLI emits: system → assistant (per turn) → result.
        let process = CliProcess::new("sh").with_timeout(10);
        let abort = CancellationToken::new();
        let jsonl = r#"printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}\n{"type":"result","subtype":"success","result":"Hello World","session_id":"abc","is_error":false}\n'"#;
        let handle = process
            .run_streaming(&["-c".to_string(), jsonl.to_string()], None, None, abort)
            .await
            .unwrap();

        let mut events = Vec::new();
        let mut rx = handle.event_rx;
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert!(
            events.len() >= 2,
            "expected at least 2 events, got {}",
            events.len()
        );
        // First event should be TextDelta (from assistant message content)
        assert!(matches!(&events[0], StreamEvent::TextDelta { text } if text == "Hello"));
        // Last event should be Result
        assert!(
            matches!(&events[events.len() - 1], StreamEvent::Result { text, .. } if text == "Hello World")
        );
    }

    #[tokio::test]
    async fn streaming_abort_kills_process() {
        // Start a long-running process, then abort it
        let process = CliProcess::new("sleep").with_timeout(0);
        let abort = CancellationToken::new();
        let handle = process
            .run_streaming(&["60".to_string()], None, None, abort.clone())
            .await
            .unwrap();

        let was_aborted = handle.was_aborted.clone();

        // Abort after a brief delay
        tokio::time::sleep(Duration::from_millis(50)).await;
        abort.cancel();

        // Drain remaining events
        let mut rx = handle.event_rx;
        while rx.recv().await.is_some() {}

        assert!(
            was_aborted.load(std::sync::atomic::Ordering::Relaxed),
            "expected was_aborted flag to be set"
        );
    }

    #[tokio::test]
    async fn streaming_channel_close_without_result() {
        // Process exits without emitting a result event
        let process = CliProcess::new("echo").with_timeout(10);
        let abort = CancellationToken::new();
        let handle = process
            .run_streaming(&["not-json".to_string()], None, None, abort)
            .await
            .unwrap();

        let mut rx = handle.event_rx;
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        // No events should be produced since "not-json" isn't valid JSONL
        assert!(events.is_empty(), "expected no events from non-JSON output");
    }

    #[tokio::test]
    async fn streaming_receiver_drop_kills_child() {
        // Drop the receiver while child is producing output — should not hang.
        let process = CliProcess::new("sh").with_timeout(10);
        let abort = CancellationToken::new();
        // Produce streaming JSONL output continuously (assistant events with text)
        let jsonl = r#"while true; do echo '{"type":"assistant","message":{"content":[{"type":"text","text":"x"}]}}'; done"#;
        let handle = process
            .run_streaming(&["-c".to_string(), jsonl.to_string()], None, None, abort)
            .await
            .unwrap();

        // Read one event then drop the receiver
        let mut rx = handle.event_rx;
        let first = rx.recv().await;
        assert!(first.is_some(), "should receive at least one event");
        drop(rx);

        // If the fix works, the reader task kills the child and returns.
        // If it doesn't, this test would hang (caught by the 10s timeout).
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[test]
    fn env_clear_includes_claudecode() {
        // CLAUDECODE must be stripped so child Claude CLI processes don't
        // refuse to start when the daemon was launched from Claude Code.
        let process = CliProcess::new("test");
        assert!(
            process.env_clear.contains(&"CLAUDECODE".to_string()),
            "env_clear must include CLAUDECODE"
        );
    }

    #[tokio::test]
    async fn claudecode_env_not_inherited_by_child() {
        // Set CLAUDECODE in our process, then verify the child doesn't see it.
        // SAFETY: single-threaded test setup.
        unsafe { std::env::set_var("CLAUDECODE", "1") };

        let process = CliProcess::new("env");
        let result = process.run(&[], None, None).await.unwrap();

        // Restore
        unsafe { std::env::remove_var("CLAUDECODE") };

        assert!(
            !result.stdout.contains("CLAUDECODE="),
            "child process should not inherit CLAUDECODE"
        );
    }

    #[test]
    fn redact_args_redacts_system_prompt() {
        let process = CliProcess::new("claude");
        let args = vec![
            "-p".to_string(),
            "--append-system-prompt".to_string(),
            "This is a secret prompt".to_string(),
            "User message".to_string(),
        ];

        let redacted = process.redact_args(&args);

        assert_eq!(redacted[0], "-p");
        assert_eq!(redacted[1], "--append-system-prompt");
        assert_eq!(redacted[2], "<redacted>");
        assert_eq!(redacted[3], "User message");
    }
}
