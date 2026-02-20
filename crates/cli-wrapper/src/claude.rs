//! ClaudeClient - High-level API for Claude CLI interaction.
//!
//! This is the main entry point for other crates to interact with Claude.

use crate::locks::ConversationLockMap;
use crate::models::resolve_model_alias;
use crate::process::{CliProcess, StreamHandle};
use crate::response::ClaudeResponse;
use crate::session::SessionManager;
use crate::tracker::ProcessTracker;
use std::path::PathBuf;
use std::sync::Arc;
use threshold_core::{ConversationId, Result, RunId, ThresholdError};
use uuid::Uuid;

// Note: SessionManager is wrapped in Arc because we need to share it
// across async tasks and potentially clone it for background operations.
// SessionManager itself uses RwLock internally for concurrent access to
// the HashMap. This Arc<SessionManager> pattern is intentional.
pub struct ClaudeClient {
    process: CliProcess,
    sessions: Arc<SessionManager>,
    locks: Arc<ConversationLockMap>,
    tracker: Arc<ProcessTracker>,
    skip_permissions: bool,
}

impl ClaudeClient {
    /// Create a new Claude client
    ///
    /// # Arguments
    /// * `command` - CLI command name (usually "claude")
    /// * `state_dir` - Directory for session state persistence
    /// * `skip_permissions` - Whether to use --dangerously-skip-permissions
    /// * `timeout_secs` - Timeout for CLI invocations (0 = unlimited)
    /// * `sessions` - Shared session manager (also used by the deletion listener)
    /// * `locks` - Per-conversation lock map (also used by the deletion listener)
    pub async fn new(
        command: String,
        _state_dir: PathBuf,
        skip_permissions: bool,
        timeout_secs: u64,
        sessions: Arc<SessionManager>,
        locks: Arc<ConversationLockMap>,
        tracker: Arc<ProcessTracker>,
    ) -> Result<Self> {
        // Load existing sessions
        sessions.load().await?;

        Ok(Self {
            process: CliProcess::new(command).with_timeout(timeout_secs),
            sessions,
            locks,
            tracker,
            skip_permissions,
        })
    }

    /// Access the shared session manager
    pub fn sessions(&self) -> &Arc<SessionManager> {
        &self.sessions
    }

    /// Access the per-conversation lock map
    pub fn locks(&self) -> &Arc<ConversationLockMap> {
        &self.locks
    }

    /// Access the process tracker
    pub fn tracker(&self) -> &Arc<ProcessTracker> {
        &self.tracker
    }

    /// Send a message to Claude using streaming mode.
    ///
    /// **Important:** The caller must acquire the per-conversation lock before
    /// calling this method and hold it until the stream is fully consumed.
    /// This prevents overlapping runs in the same conversation.
    ///
    /// Session persistence is deferred to the caller (engine) — only after a
    /// successful `Result` event should the session be saved.
    ///
    /// The `run_id` is registered with the `ProcessTracker` so `/abort` can
    /// cancel the invocation.
    pub async fn send_message_streaming(
        &self,
        conversation_id: Uuid,
        run_id: RunId,
        message: &str,
        system_prompt: Option<&str>,
        model: Option<&str>,
    ) -> Result<StreamHandle> {
        // Check for existing session
        let existing_session = self.sessions.get(conversation_id).await;

        let args = if let Some(session_id) = existing_session {
            tracing::debug!(
                conversation_id = %conversation_id,
                session_id = %session_id,
                "resuming streaming CLI session"
            );
            self.build_resume_streaming_args(&session_id, message)
        } else {
            let session_id = Uuid::new_v4();
            let resolved_model = model
                .map(|m| resolve_model_alias(m).into_owned())
                .unwrap_or_else(|| "sonnet".to_string());

            tracing::debug!(
                conversation_id = %conversation_id,
                session_id = %session_id,
                model = %resolved_model,
                "starting new streaming CLI session"
            );

            self.build_new_session_streaming_args(
                session_id,
                message,
                system_prompt,
                &resolved_model,
            )
        };

        // Register with process tracker
        let abort_token = self
            .tracker
            .register(run_id, ConversationId(conversation_id))
            .await;

        // Spawn streaming process
        let handle = self
            .process
            .run_streaming(&args, None, None, abort_token)
            .await?;

        Ok(handle)
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
        // Per-conversation lock: different conversations run in parallel,
        // messages within the same conversation are serialized.
        let _guard = self.locks.lock(conversation_id).await;

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
                    let retry_args =
                        self.build_resume_args(&session_id.to_string(), message);
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

    fn build_new_session_streaming_args(
        &self,
        session_id: Uuid,
        message: &str,
        system_prompt: Option<&str>,
        model: &str,
    ) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            "--verbose".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
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

    fn build_resume_streaming_args(&self, session_id: &str, message: &str) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            "--verbose".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];

        if self.skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }

        args.push("--resume".to_string());
        args.push(session_id.to_string());

        args.push(message.to_string());
        args
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

    async fn test_client(dir: &std::path::Path, skip_perms: bool) -> ClaudeClient {
        let sessions = Arc::new(SessionManager::new(dir.join("cli-sessions.json")));
        let locks = Arc::new(ConversationLockMap::new());
        let tracker = Arc::new(ProcessTracker::new());
        ClaudeClient::new(
            "claude".to_string(),
            dir.to_path_buf(),
            skip_perms,
            300,
            sessions,
            locks,
            tracker,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn client_creation_loads_sessions() {
        let dir = tempdir().unwrap();
        let client = test_client(dir.path(), false).await;

        // Sessions loaded (count is 0 since no file exists yet)
        assert_eq!(client.sessions.count().await, 0);
    }

    #[tokio::test]
    async fn build_new_session_args_includes_all_flags() {
        let dir = tempdir().unwrap();
        let client = test_client(dir.path(), true).await;

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
        let client = test_client(dir.path(), false).await;

        let session_id = Uuid::new_v4();
        let args = client.build_new_session_args(session_id, "Hello", None, "sonnet");

        assert!(!args.contains(&"--append-system-prompt".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[tokio::test]
    async fn build_new_session_args_uses_decoupled_session_id() {
        let dir = tempdir().unwrap();
        let client = test_client(dir.path(), false).await;

        let session_id = Uuid::new_v4();
        let args = client.build_new_session_args(session_id, "Hello", None, "sonnet");

        // Session ID should be the generated UUID, not tied to any conversation ID
        assert!(args.contains(&session_id.to_string()));
        assert!(args.contains(&"--session-id".to_string()));
    }

    #[tokio::test]
    async fn build_resume_args_only_includes_resume_flag() {
        let dir = tempdir().unwrap();
        let client = test_client(dir.path(), false).await;

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
        let client = test_client(dir.path(), true).await;

        let args = client.build_resume_args("session-123", "Follow up");

        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[tokio::test]
    #[ignore] // Requires claude CLI installed
    async fn health_check_with_real_cli() {
        let dir = tempdir().unwrap();
        let client = test_client(dir.path(), false).await;

        let result = client.health_check().await;
        assert!(result.is_ok());
    }
}
