//! CLI subprocess spawning and execution.
//!
//! Handles spawning the Claude CLI as a subprocess with proper timeout,
//! environment variable clearing, and error classification.

use std::path::Path;
use std::time::Duration;
use threshold_core::{Result, ThresholdError};
use tokio::process::Command;

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

impl CliProcess {
    /// Create a new CLI process with default settings
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout_secs: 300, // 5 minutes default
            env_clear: vec![
                "ANTHROPIC_API_KEY".to_string(),
                "ANTHROPIC_API_KEY_OLD".to_string(),
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
            let _ = tokio::io::BufReader::new(stdout).read_to_end(&mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::BufReader::new(stderr).read_to_end(&mut buf).await;
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
                &[
                    "-c".to_string(),
                    "yes | head -c 100000".to_string(),
                ],
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
