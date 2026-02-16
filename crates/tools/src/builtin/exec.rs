//! Shell command execution tool

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::Stdio;
use threshold_core::{Result, ThresholdError};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::{Tool, ToolContext, ToolResult};

/// ExecTool - executes shell commands with timeout and output capture
pub struct ExecTool;

#[derive(Debug, Deserialize)]
struct ExecParams {
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    120 // 2 minutes
}

#[derive(Debug, Serialize)]
struct ExecOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command with timeout and output capture"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional working directory (defaults to context working_dir)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120)",
                    "default": 120,
                    "minimum": 1,
                    "maximum": 300
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let params: ExecParams = serde_json::from_value(params)?;

        // Determine working directory
        let working_dir = if let Some(dir) = params.working_dir {
            let path = std::path::PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                // Resolve relative paths against ctx.working_dir
                ctx.working_dir.join(path)
            }
        } else {
            ctx.working_dir.clone()
        };

        // Start command with stdout/stderr capture
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&params.command)
            .current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ThresholdError::ToolError {
                tool: "exec".to_string(),
                message: format!("Failed to spawn command: {}", e),
            })?;

        // Capture stdout and stderr
        let stdout = child.stdout.take().ok_or_else(|| ThresholdError::ToolError {
            tool: "exec".to_string(),
            message: "Failed to capture stdout".to_string(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ThresholdError::ToolError {
            tool: "exec".to_string(),
            message: "Failed to capture stderr".to_string(),
        })?;

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        // Read output concurrently to avoid deadlock on large output
        let stdout_task = tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                lines.push(line);
            }
            lines
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                lines.push(line);
            }
            lines
        });

        // Wait for command with timeout
        let timeout = tokio::time::Duration::from_secs(params.timeout_secs);
        let result = tokio::select! {
            result = child.wait() => Some(result),
            _ = tokio::time::sleep(timeout) => {
                // Timeout - kill the process
                child.kill().await.ok();
                None
            }
            _ = ctx.cancellation.cancelled() => {
                // Cancellation - kill the process
                child.kill().await.ok();
                return Err(ThresholdError::SchedulerShutdown);
            }
        };

        // Wait for output tasks to complete
        let stdout_lines = stdout_task.await.map_err(|e| ThresholdError::ToolError {
            tool: "exec".to_string(),
            message: format!("Failed to read stdout: {}", e),
        })?;
        let stderr_lines = stderr_task.await.map_err(|e| ThresholdError::ToolError {
            tool: "exec".to_string(),
            message: format!("Failed to read stderr: {}", e),
        })?;

        match result {
            Some(exit_result) => {
                let exit_status = exit_result.map_err(|e| ThresholdError::ToolError {
                    tool: "exec".to_string(),
                    message: format!("Failed to wait for command: {}", e),
                })?;

                let output = ExecOutput {
                    stdout: stdout_lines.join("\n"),
                    stderr: stderr_lines.join("\n"),
                    exit_code: exit_status.code().unwrap_or(-1),
                    timed_out: false,
                };

                Ok(ToolResult::success(serde_json::to_string_pretty(&output)?))
            }
            None => {
                // Timeout occurred
                let output = ExecOutput {
                    stdout: stdout_lines.join("\n"),
                    stderr: stderr_lines.join("\n"),
                    exit_code: -1,
                    timed_out: true,
                };
                Ok(ToolResult::failure(serde_json::to_string_pretty(&output)?))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn exec_tool_name_is_exec() {
        let tool = ExecTool;
        assert_eq!(tool.name(), "exec");
    }

    #[tokio::test]
    async fn exec_tool_executes_simple_command() {
        let tool = ExecTool;
        let ctx = ToolContext::new("test-agent");
        let params = json!({
            "command": "echo hello"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn exec_tool_captures_stderr() {
        let tool = ExecTool;
        let ctx = ToolContext::new("test-agent");
        let params = json!({
            "command": "echo error >&2"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.content.contains("error"));
    }

    #[tokio::test]
    async fn exec_tool_captures_exit_code() {
        let tool = ExecTool;
        let ctx = ToolContext::new("test-agent");
        let params = json!({
            "command": "exit 42"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.content.contains("\"exit_code\": 42"));
    }

    #[tokio::test]
    async fn exec_tool_respects_working_dir() {
        let tool = ExecTool;
        let ctx = ToolContext::new("test-agent");
        let params = json!({
            "command": "pwd",
            "working_dir": "/tmp"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.content.contains("/tmp"));
    }

    #[tokio::test]
    async fn exec_tool_times_out_long_commands() {
        let tool = ExecTool;
        let ctx = ToolContext::new("test-agent");
        let params = json!({
            "command": "sleep 10",
            "timeout_secs": 1
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.content.contains("\"timed_out\": true"));
    }
}
