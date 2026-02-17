//! Task execution — handles all four `ScheduledAction` variants.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use uuid::Uuid;

use threshold_cli_wrapper::ClaudeClient;
use threshold_conversation::ConversationEngine;
use threshold_core::{ResultSender, ScheduledAction, ThresholdError};

use crate::task::{DeliveryTarget, ScheduledTask, TaskRunResult};

/// Truncate a string to at most `max_len` bytes, appending "..." if truncated.
///
/// Uses `char_indices()` to find a valid UTF-8 boundary, preventing panics
/// on multi-byte characters (e.g., em-dashes, smart quotes from Claude).
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let end = max_len.saturating_sub(3);
        let boundary = s
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= end)
            .last()
            .unwrap_or(0);
        format!("{}...", &s[..boundary])
    }
}

/// Execute a scheduled task and return the result.
///
/// Dispatches to the appropriate handler based on the task's `ScheduledAction`.
pub async fn execute_task(
    task: &ScheduledTask,
    claude: &Arc<ClaudeClient>,
    engine: &Arc<ConversationEngine>,
) -> TaskRunResult {
    let start = Instant::now();

    let result = match &task.action {
        ScheduledAction::NewConversation { prompt, model } => {
            exec_new_conversation(claude, prompt, model.as_deref()).await
        }
        ScheduledAction::ResumeConversation {
            conversation_id,
            prompt,
        } => exec_resume_conversation(engine, conversation_id, prompt).await,
        ScheduledAction::Script {
            command,
            working_dir,
        } => exec_script(command, working_dir.as_deref()).await,
        ScheduledAction::ScriptThenConversation {
            command,
            prompt_template,
            model,
        } => exec_script_then_conversation(claude, command, prompt_template, model.as_deref()).await,
    };

    let duration = start.elapsed();
    match result {
        Ok(output) => TaskRunResult {
            timestamp: Utc::now(),
            success: true,
            summary: truncate(&output, 2000),
            duration_ms: duration.as_millis() as u64,
        },
        Err(e) => TaskRunResult {
            timestamp: Utc::now(),
            success: false,
            summary: format!("Error: {}", e),
            duration_ms: duration.as_millis() as u64,
        },
    }
}

/// Deliver a task result to the configured delivery target.
pub async fn deliver_result(
    task: &ScheduledTask,
    result: &TaskRunResult,
    result_sender: &Option<Arc<dyn ResultSender>>,
) {
    // Conversation-attached tasks deliver via the portal system
    if task.conversation_id.is_some() {
        return;
    }

    let message = format!(
        "**Scheduled Task: {}**\n{}\n*Duration: {}ms*",
        task.name, result.summary, result.duration_ms,
    );

    match &task.delivery {
        DeliveryTarget::DiscordChannel { channel_id } => {
            if let Some(sender) = result_sender {
                if let Err(e) = sender.send_to_channel(*channel_id, &message).await {
                    tracing::error!("Failed to deliver result to channel {}: {}", channel_id, e);
                }
            }
        }
        DeliveryTarget::DiscordDm { user_id } => {
            if let Some(sender) = result_sender {
                if let Err(e) = sender.send_dm(*user_id, &message).await {
                    tracing::error!("Failed to deliver result as DM to {}: {}", user_id, e);
                }
            }
        }
        DeliveryTarget::AuditLogOnly => {
            // Nothing to do — execution is already logged
        }
    }
}

/// Launch a fresh Claude conversation with the given prompt.
async fn exec_new_conversation(
    claude: &Arc<ClaudeClient>,
    prompt: &str,
    model: Option<&str>,
) -> Result<String, ThresholdError> {
    let response = claude
        .send_message(Uuid::new_v4(), prompt, None, model)
        .await?;
    Ok(response.text)
}

/// Resume an existing conversation via the ConversationEngine.
async fn exec_resume_conversation(
    engine: &Arc<ConversationEngine>,
    conversation_id: &threshold_core::ConversationId,
    prompt: &str,
) -> Result<String, ThresholdError> {
    engine.send_to_conversation(conversation_id, prompt).await
}

/// Run a shell command directly (no Claude involved).
async fn exec_script(
    command: &str,
    working_dir: Option<&str>,
) -> Result<String, ThresholdError> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let output = cmd.output().await.map_err(|e| ThresholdError::IoError {
        path: command.to_string(),
        message: e.to_string(),
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        Ok(stdout.to_string())
    } else {
        let code = output.status.code().unwrap_or(-1);
        Err(ThresholdError::CliError {
            provider: "sh".into(),
            code,
            stderr: stderr.to_string(),
        })
    }
}

/// Run a script, then feed the output to Claude for analysis.
async fn exec_script_then_conversation(
    claude: &Arc<ClaudeClient>,
    command: &str,
    prompt_template: &str,
    model: Option<&str>,
) -> Result<String, ThresholdError> {
    // Step 1: Run the script
    let script_output = exec_script(command, None).await?;

    // Step 2: Build prompt with script output
    let prompt = prompt_template.replace("{output}", &script_output);

    // Step 3: Send to Claude
    exec_new_conversation(claude, &prompt, model).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(100);
        let result = truncate(&long, 20);
        assert!(result.len() <= 20);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_multibyte_utf8_does_not_panic() {
        // Em-dashes and smart quotes are multi-byte UTF-8, common in Claude responses
        let s = "Hello \u{2014} world \u{201C}test\u{201D} done";
        let result = truncate(s, 10);
        assert!(result.len() <= 13); // 10 chars of content + "..."
        assert!(result.ends_with("..."));
        // Verify it's valid UTF-8 (would panic if not)
        let _ = result.as_bytes();
    }

    #[tokio::test]
    async fn exec_script_echo_succeeds() {
        let result = exec_script("echo hello", None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "hello");
    }

    #[tokio::test]
    async fn exec_script_with_working_dir() {
        let result = exec_script("pwd", Some("/tmp")).await;
        assert!(result.is_ok());
        // macOS: /tmp is a symlink to /private/tmp
        let output = result.unwrap();
        assert!(
            output.trim() == "/tmp" || output.trim() == "/private/tmp",
            "unexpected pwd output: {}",
            output.trim()
        );
    }

    #[tokio::test]
    async fn exec_script_failure_returns_error() {
        let result = exec_script("exit 42", None).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ThresholdError::CliError { code, .. } => assert_eq!(code, 42),
            other => panic!("Expected CliError, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn exec_script_captures_stdout() {
        let result = exec_script("printf 'line1'; printf 'line2'", None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "line1line2");
    }

    #[test]
    fn script_then_conversation_substitutes_output() {
        // Test the template substitution logic directly
        let template = "Script output:\n{output}\n\nAnalyze this.";
        let output = "all tests passed";
        let result = template.replace("{output}", output);
        assert_eq!(result, "Script output:\nall tests passed\n\nAnalyze this.");
    }
}
