//! ClaudeClient - High-level API for Claude CLI interaction.
//!
//! This is the main entry point for other crates to interact with Claude.

use crate::models::resolve_model_alias;
use crate::process::CliProcess;
use crate::queue::ExecutionQueue;
use crate::response::ClaudeResponse;
use crate::session::SessionManager;
use std::path::PathBuf;
use std::sync::Arc;
use threshold_core::{Result, ThresholdError};
use uuid::Uuid;

// Note: SessionManager is wrapped in Arc because we need to share it
// across async tasks and potentially clone it for background operations.
// SessionManager itself uses RwLock internally for concurrent access to
// the HashMap. This Arc<SessionManager> pattern is intentional.
pub struct ClaudeClient {
    process: CliProcess,
    sessions: Arc<SessionManager>,
    queue: ExecutionQueue,
    skip_permissions: bool,
}

impl ClaudeClient {
    /// Create a new Claude client
    ///
    /// # Arguments
    /// * `command` - CLI command name (usually "claude")
    /// * `state_dir` - Directory for session state persistence
    /// * `skip_permissions` - Whether to use --dangerously-skip-permissions
    pub async fn new(command: String, state_dir: PathBuf, skip_permissions: bool) -> Result<Self> {
        let sessions = Arc::new(SessionManager::new(state_dir.join("cli-sessions.json")));

        // Load existing sessions
        sessions.load().await?;

        Ok(Self {
            process: CliProcess::new(command),
            sessions,
            queue: ExecutionQueue::new(),
            skip_permissions,
        })
    }

    /// Send a message to Claude
    ///
    /// Automatically decides between new session and resume based on existing state.
    ///
    /// # Arguments
    /// * `conversation_id` - Unique ID for this conversation
    /// * `message` - User message
    /// * `system_prompt` - Optional system prompt (only used for new sessions)
    /// * `model` - Optional model name (only used for new sessions)
    pub async fn send_message(
        &self,
        conversation_id: Uuid,
        message: &str,
        system_prompt: Option<&str>,
        model: Option<&str>,
    ) -> Result<ClaudeResponse> {
        self.queue
            .execute(async {
                // Check for existing session
                let existing_session = self.sessions.get(conversation_id).await;

                let result = if let Some(session_id) = existing_session {
                    // Resume mode
                    tracing::debug!(
                        conversation_id = %conversation_id,
                        session_id = %session_id,
                        "resuming existing CLI session"
                    );
                    let args = self.build_resume_args(&session_id, message);
                    self.process.run(&args, None, None).await
                } else {
                    // New session mode — generate a fresh UUID (decoupled from conversation ID)
                    let session_id = Uuid::new_v4();
                    let resolved_model = model
                        .map(|m| resolve_model_alias(m).into_owned())
                        .unwrap_or_else(|| "sonnet".to_string());

                    tracing::debug!(
                        conversation_id = %conversation_id,
                        session_id = %session_id,
                        model = %resolved_model,
                        "starting new CLI session"
                    );

                    let args = self.build_new_session_args(
                        session_id,
                        message,
                        system_prompt,
                        &resolved_model,
                    );
                    let res = self.process.run(&args, None, None).await;

                    // If "already in use", fall back to resume
                    if let Err(ThresholdError::CliError { ref stderr, .. }) = res {
                        if stderr.contains("already in use") {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                session_id = %session_id,
                                "session ID collision, retrying with --resume"
                            );
                            let retry_args = self.build_resume_args(&session_id.to_string(), message);
                            self.process.run(&retry_args, None, None).await
                        } else {
                            res
                        }
                    } else {
                        res
                    }
                };

                // Execute CLI
                let output = result?;

                // Parse response
                let response = ClaudeResponse::parse(&output.stdout)?;

                // Store session ID if present
                if let Some(session_id) = &response.session_id {
                    tracing::debug!(
                        conversation_id = %conversation_id,
                        session_id = %session_id,
                        "stored CLI session ID"
                    );
                    self.sessions
                        .set(conversation_id, session_id.clone())
                        .await?;
                }

                Ok(response)
            })
            .await
    }

    /// Force a new session (ignores existing session)
    ///
    /// Useful for resetting a conversation or changing model/system prompt.
    pub async fn new_session(
        &self,
        conversation_id: Uuid,
        message: &str,
        system_prompt: &str,
        model: &str,
    ) -> Result<ClaudeResponse> {
        // Remove any existing session
        let _ = self.sessions.remove(conversation_id).await;

        // Send as new
        self.send_message(conversation_id, message, Some(system_prompt), Some(model))
            .await
    }

    /// Health check: verify CLI is installed and responsive
    ///
    /// Note: Requires Claude Code CLI version >= 0.98.0
    /// (The version with stable JSON output format)
    pub async fn health_check(&self) -> Result<()> {
        let output = self
            .process
            .run(&["--version".to_string()], None, None)
            .await?;

        if output.stdout.is_empty() && output.stderr.is_empty() {
            return Err(ThresholdError::CliError {
                provider: "claude".to_string(),
                code: 0,
                stderr: "CLI did not produce any output".to_string(),
            });
        }

        // TODO: Parse version and validate >= 0.98.0 for production use
        // For now, we just verify the CLI responds

        tracing::info!(
            version_output = %output.stdout.trim(),
            "Claude CLI health check passed"
        );

        Ok(())
    }

    fn build_new_session_args(
        &self,
        session_id: Uuid,
        message: &str,
        system_prompt: Option<&str>,
        model: &str,
    ) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ];

        if self.skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }

        args.push("--session-id".to_string());
        args.push(session_id.to_string());

        args.push("--model".to_string());
        args.push(model.to_string());

        if let Some(prompt) = system_prompt {
            args.push("--append-system-prompt".to_string());
            args.push(prompt.to_string());
        }

        args.push(message.to_string());

        args
    }

    fn build_resume_args(&self, session_id: &str, message: &str) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ];

        if self.skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }

        args.push("--resume".to_string());
        args.push(session_id.to_string());

        args.push(message.to_string());

        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn client_creation_loads_sessions() {
        let dir = tempdir().unwrap();

        let client = ClaudeClient::new("claude".to_string(), dir.path().to_path_buf(), false)
            .await
            .unwrap();

        // Sessions loaded (count is 0 since no file exists yet)
        assert_eq!(client.sessions.count().await, 0);
    }

    #[tokio::test]
    async fn build_new_session_args_includes_all_flags() {
        let dir = tempdir().unwrap();
        let client = ClaudeClient::new("claude".to_string(), dir.path().to_path_buf(), true)
            .await
            .unwrap();

        let session_id = Uuid::new_v4();
        let args =
            client.build_new_session_args(session_id, "Hello", Some("You are helpful"), "opus");

        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"json".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
        assert!(args.contains(&session_id.to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"opus".to_string()));
        assert!(args.contains(&"--append-system-prompt".to_string()));
        assert!(args.contains(&"You are helpful".to_string()));
        assert!(args.contains(&"Hello".to_string()));
    }

    #[tokio::test]
    async fn build_new_session_args_without_system_prompt() {
        let dir = tempdir().unwrap();
        let client = ClaudeClient::new("claude".to_string(), dir.path().to_path_buf(), false)
            .await
            .unwrap();

        let session_id = Uuid::new_v4();
        let args = client.build_new_session_args(session_id, "Hello", None, "sonnet");

        assert!(!args.contains(&"--append-system-prompt".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[tokio::test]
    async fn build_new_session_args_uses_decoupled_session_id() {
        let dir = tempdir().unwrap();
        let client = ClaudeClient::new("claude".to_string(), dir.path().to_path_buf(), false)
            .await
            .unwrap();

        let session_id = Uuid::new_v4();
        let args = client.build_new_session_args(session_id, "Hello", None, "sonnet");

        // Session ID should be the generated UUID, not tied to any conversation ID
        assert!(args.contains(&session_id.to_string()));
        assert!(args.contains(&"--session-id".to_string()));
    }

    #[tokio::test]
    async fn build_resume_args_only_includes_resume_flag() {
        let dir = tempdir().unwrap();
        let client = ClaudeClient::new("claude".to_string(), dir.path().to_path_buf(), false)
            .await
            .unwrap();

        let args = client.build_resume_args("session-123", "Follow up");

        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"session-123".to_string()));
        assert!(args.contains(&"Follow up".to_string()));

        // Should NOT include these
        assert!(!args.contains(&"--model".to_string()));
        assert!(!args.contains(&"--session-id".to_string()));
        assert!(!args.contains(&"--append-system-prompt".to_string()));
    }

    #[tokio::test]
    async fn build_resume_args_with_skip_permissions() {
        let dir = tempdir().unwrap();
        let client = ClaudeClient::new("claude".to_string(), dir.path().to_path_buf(), true)
            .await
            .unwrap();

        let args = client.build_resume_args("session-123", "Follow up");

        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[tokio::test]
    #[ignore] // Requires claude CLI installed
    async fn health_check_with_real_cli() {
        let dir = tempdir().unwrap();
        let client = ClaudeClient::new("claude".to_string(), dir.path().to_path_buf(), false)
            .await
            .unwrap();

        let result = client.health_check().await;
        assert!(result.is_ok());
    }
}
