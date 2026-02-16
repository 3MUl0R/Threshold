# Milestone 2 Implementation Plan: Claude CLI Wrapper

## Overview
Build the `cli-wrapper` crate to spawn Claude CLI as a subprocess, manage sessions, parse responses, and provide a clean Rust API for invoking Claude Code programmatically.

**Complexity:** Medium-High (subprocess management, session state, robust parsing)
**Dependencies:** Milestone 1 (core types, config, error handling)

---

## Implementation Strategy

### Approach
Implement phases **sequentially** with comprehensive testing at each stage. Each phase builds on the previous:
1. Process spawning → 2. Response parsing → 3. Session management → 4-7. Higher-level features

### Testing Philosophy
- **Unit tests**: For parsing logic, model resolution, arg building
- **Integration tests**: Require `claude` CLI installed, marked with `#[ignore]` for CI
- **Mock tests**: Use `echo` or fake commands to test error paths without requiring Claude

---

## Phase 2.1: CLI Subprocess Spawning

### Files
- `crates/cli-wrapper/src/process.rs`
- `crates/cli-wrapper/src/lib.rs` (module declaration)
- `crates/cli-wrapper/Cargo.toml` (new crate)

### Implementation: `process.rs`

```rust
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Configuration for CLI process spawning
#[derive(Debug, Clone)]
pub struct CliProcess {
    command: String,              // "claude"
    timeout_secs: u64,            // Default: 300
    env_clear: Vec<String>,       // ["ANTHROPIC_API_KEY", ...]
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
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout_secs: 300,  // 5 minutes default
            env_clear: vec![
                "ANTHROPIC_API_KEY".to_string(),
                "ANTHROPIC_API_KEY_OLD".to_string(),
            ],
        }
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Run the CLI with the given arguments
    pub async fn run(
        &self,
        args: &[String],
        working_dir: Option<&Path>,
        stdin_data: Option<&str>,
    ) -> crate::Result<CliOutput> {
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
        let mut child = cmd.spawn()
            .map_err(|e| {
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

        // Wait with timeout
        // IMPORTANT: Use select! to preserve child handle for kill on timeout
        let output = tokio::select! {
            result = child.wait_with_output() => {
                match result {
                    Ok(output) => output,
                    Err(e) => return Err(ThresholdError::Io(e)),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(self.timeout_secs)) => {
                // Timeout - kill the process
                let _ = child.kill().await;
                return Err(ThresholdError::CliTimeout {
                    timeout_ms: self.timeout_secs * 1000,
                });
            }
        };

        let duration = start.elapsed();
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        tracing::debug!(
            exit_code,
            duration_ms = duration.as_millis(),
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            "CLI process completed"
        );

        // Check for errors
        if exit_code != 0 {
            // Classify error type based on stderr patterns
            let stderr_lower = stderr.to_lowercase();
            if stderr_lower.contains("401") || stderr_lower.contains("unauthorized") {
                return Err(ThresholdError::CliError {
                    provider: "claude".to_string(),
                    code: exit_code,
                    stderr: "Authentication expired. Please re-authenticate.".to_string(),
                });
            } else if stderr_lower.contains("402") || stderr_lower.contains("payment") {
                return Err(ThresholdError::CliError {
                    provider: "claude".to_string(),
                    code: exit_code,
                    stderr: "Billing issue detected.".to_string(),
                });
            } else if stderr_lower.contains("429") || stderr_lower.contains("rate limit") {
                return Err(ThresholdError::CliError {
                    provider: "claude".to_string(),
                    code: exit_code,
                    stderr: "Rate limited. Please try again later.".to_string(),
                });
            } else {
                return Err(ThresholdError::CliError {
                    provider: "claude".to_string(),
                    code: exit_code,
                    stderr,
                });
            }
        }

        Ok(CliOutput {
            stdout,
            stderr,
            exit_code,
            duration,
        })
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
```

### Tests

```rust
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
        let result = process.run(
            &["hello".to_string(), "world".to_string()],
            None,
            None
        ).await;

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
            ThresholdError::CliTimeout { .. } => {},
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
```

### Cargo.toml

```toml
[package]
name = "threshold-cli-wrapper"
version.workspace = true
edition.workspace = true

[dependencies]
threshold-core = { path = "../core" }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4", "serde"] }
tracing = "0.1"

[dev-dependencies]
tokio-test = "0.4"
tempfile = "3"
```

---

## Phase 2.2: Response Parsing

### File: `crates/cli-wrapper/src/response.rs`

**Goal:** Resilient parsing of Claude CLI JSON output.

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ClaudeResponse {
    pub text: String,
    pub session_id: Option<String>,
    pub usage: Option<Usage>,
    pub raw_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl ClaudeResponse {
    pub fn parse(stdout: &str) -> crate::Result<Self> {
        // Try JSON parsing first
        match serde_json::from_str::<serde_json::Value>(stdout) {
            Ok(json) => Self::from_json(json),
            Err(_) => {
                // Fallback: treat as plain text
                tracing::warn!("CLI output is not valid JSON, using raw text");
                Ok(Self {
                    text: stdout.to_string(),
                    session_id: None,
                    usage: None,
                    raw_json: None,
                })
            }
        }
    }

    fn from_json(json: serde_json::Value) -> crate::Result<Self> {
        // Extract text (try multiple fields)
        let text = Self::extract_text(&json)
            .unwrap_or_else(|| json.to_string());

        // Extract session ID (try multiple field names)
        let session_id = Self::extract_session_id(&json);

        // Extract usage if present
        let usage = json.get("usage")
            .and_then(|u| serde_json::from_value(u.clone()).ok());

        Ok(Self {
            text,
            session_id,
            usage,
            raw_json: Some(json),
        })
    }

    fn extract_text(json: &serde_json::Value) -> Option<String> {
        // Try in priority order
        json.get("message")
            .or_else(|| json.get("content"))
            .or_else(|| json.get("result"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    fn extract_session_id(json: &serde_json::Value) -> Option<String> {
        json.get("session_id")
            .or_else(|| json.get("sessionId"))
            .or_else(|| json.get("conversation_id"))
            .or_else(|| json.get("conversationId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}
```

### Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_message_field() {
        let json = r#"{"message": "Hello!", "session_id": "abc123"}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.text, "Hello!");
        assert_eq!(response.session_id, Some("abc123".to_string()));
    }

    #[test]
    fn parse_content_field_fallback() {
        let json = r#"{"content": "World", "sessionId": "xyz"}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.text, "World");
        assert_eq!(response.session_id, Some("xyz".to_string()));
    }

    #[test]
    fn parse_result_field_fallback() {
        let json = r#"{"result": "Test", "conversation_id": "789"}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.text, "Test");
        assert_eq!(response.session_id, Some("789".to_string()));
    }

    #[test]
    fn parse_malformed_json_uses_raw_text() {
        let text = "This is not JSON";
        let response = ClaudeResponse::parse(text).unwrap();

        assert_eq!(response.text, text);
        assert_eq!(response.session_id, None);
    }

    #[test]
    fn parse_with_usage() {
        let json = r#"{
            "message": "Hi",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50
            }
        }"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert!(response.usage.is_some());
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
    }
}
```

---

## Phase 2.3: Session Management

### File: `crates/cli-wrapper/src/session.rs`

**Goal:** Track CLI session IDs and decide new vs resume mode.

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMap {
    sessions: HashMap<Uuid, String>,
}

pub struct SessionManager {
    sessions: RwLock<HashMap<Uuid, String>>,
    state_path: PathBuf,
}

impl SessionManager {
    pub fn new(state_path: PathBuf) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            state_path,
        }
    }

    /// Load sessions from disk
    pub async fn load(&self) -> crate::Result<()> {
        if !self.state_path.exists() {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(&self.state_path).await?;

        // Handle corruption gracefully - reset to empty map on parse error
        let map: SessionMap = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = ?self.state_path,
                    "session file corrupted, resetting to empty"
                );
                SessionMap {
                    sessions: HashMap::new(),
                }
            }
        };

        let mut sessions = self.sessions.write().await;
        *sessions = map.sessions;

        tracing::info!(count = sessions.len(), "loaded CLI sessions from disk");
        Ok(())
    }

    /// Save sessions to disk
    pub async fn save(&self) -> crate::Result<()> {
        let sessions = self.sessions.read().await;
        let map = SessionMap {
            sessions: sessions.clone(),
        };

        // Create parent directory
        if let Some(parent) = self.state_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let json = serde_json::to_string_pretty(&map)?;
        tokio::fs::write(&self.state_path, json).await?;

        // Set restrictive permissions (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&self.state_path).await?.permissions();
            perms.set_mode(0o600); // rw------- (owner only)
            tokio::fs::set_permissions(&self.state_path, perms).await?;
        }

        Ok(())
    }

    /// Get the CLI session ID for a conversation
    pub async fn get(&self, conversation_id: Uuid) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions.get(&conversation_id).cloned()
    }

    /// Store a CLI session ID for a conversation
    pub async fn set(&self, conversation_id: Uuid, session_id: String) -> crate::Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.insert(conversation_id, session_id);
        drop(sessions);

        // Persist to disk
        self.save().await?;
        Ok(())
    }

    /// Remove a session (e.g., on error/reset)
    pub async fn remove(&self, conversation_id: Uuid) -> crate::Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.remove(&conversation_id);
        drop(sessions);

        self.save().await?;
        Ok(())
    }
}
```

---

## Phase 2.4: Model Alias Resolution

### File: `crates/cli-wrapper/src/models.rs`

```rust
use std::borrow::Cow;

/// Resolve user-friendly model aliases to CLI model names.
pub fn resolve_model_alias(input: &str) -> Cow<'static, str> {
    let lower = input.to_lowercase();
    match lower.as_str() {
        "opus" | "opus-4" | "opus-4.5" | "opus-4.6" | "claude-opus" => {
            Cow::Borrowed("opus")
        }
        "sonnet" | "sonnet-4" | "sonnet-4.1" | "sonnet-4.5" | "claude-sonnet" => {
            Cow::Borrowed("sonnet")
        }
        "haiku" | "haiku-3.5" | "haiku-4.5" | "claude-haiku" => {
            Cow::Borrowed("haiku")
        }
        _ => Cow::Owned(input.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_opus_aliases() {
        assert_eq!(resolve_model_alias("opus"), "opus");
        assert_eq!(resolve_model_alias("OPUS"), "opus");
        assert_eq!(resolve_model_alias("opus-4.6"), "opus");
        assert_eq!(resolve_model_alias("claude-opus"), "opus");
    }

    #[test]
    fn resolve_sonnet_aliases() {
        assert_eq!(resolve_model_alias("sonnet"), "sonnet");
        assert_eq!(resolve_model_alias("Sonnet-4.5"), "sonnet");
    }

    #[test]
    fn unknown_model_passes_through() {
        assert_eq!(resolve_model_alias("gpt-4"), "gpt-4");
        assert_eq!(resolve_model_alias("custom-model"), "custom-model");
    }
}
```

---

## Phase 2.5: Sequential Execution Queue

### File: `crates/cli-wrapper/src/queue.rs`

```rust
use std::future::Future;
use tokio::sync::Mutex;

pub struct ExecutionQueue {
    lock: Mutex<()>,
}

impl ExecutionQueue {
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
        }
    }

    pub async fn execute<F, T>(&self, f: F) -> T
    where
        F: Future<Output = T>,
    {
        let _guard = self.lock.lock().await;
        f.await
    }
}

impl Default for ExecutionQueue {
    fn default() -> Self {
        Self::new()
    }
}
```

---

## Phase 2.6: ClaudeClient Facade

### File: `crates/cli-wrapper/src/claude.rs`

**This is the high-level API.**

```rust
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
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
    pub async fn new(
        command: String,
        state_dir: PathBuf,
        skip_permissions: bool,
    ) -> crate::Result<Self> {
        let sessions = Arc::new(SessionManager::new(
            state_dir.join("cli-sessions.json")
        ));

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
    pub async fn send_message(
        &self,
        conversation_id: Uuid,
        message: &str,
        system_prompt: Option<&str>,
        model: Option<&str>,
    ) -> crate::Result<ClaudeResponse> {
        self.queue.execute(async {
            // Check for existing session
            let existing_session = self.sessions.get(conversation_id).await;

            let args = if let Some(session_id) = existing_session {
                // Resume mode
                self.build_resume_args(&session_id, message)
            } else {
                // New session mode
                let resolved_model = model
                    .map(|m| resolve_model_alias(m).into_owned())
                    .unwrap_or_else(|| "sonnet".to_string());

                self.build_new_session_args(
                    conversation_id,
                    message,
                    system_prompt,
                    &resolved_model,
                )
            };

            // Execute CLI
            let output = self.process.run(&args, None, None).await?;

            // Parse response
            let response = ClaudeResponse::parse(&output.stdout)?;

            // Store session ID if present
            if let Some(session_id) = &response.session_id {
                self.sessions.set(conversation_id, session_id.clone()).await?;
            }

            Ok(response)
        }).await
    }

    /// Force a new session (ignores existing session)
    pub async fn new_session(
        &self,
        conversation_id: Uuid,
        message: &str,
        system_prompt: &str,
        model: &str,
    ) -> crate::Result<ClaudeResponse> {
        // Remove any existing session
        let _ = self.sessions.remove(conversation_id).await;

        // Send as new
        self.send_message(conversation_id, message, Some(system_prompt), Some(model)).await
    }

    /// Health check: verify CLI is installed and responsive
    ///
    /// Note: Requires Claude Code CLI version >= 0.98.0
    /// (The version with stable JSON output format)
    pub async fn health_check(&self) -> crate::Result<()> {
        let output = self.process.run(&["--version".to_string()], None, None).await?;

        if output.stdout.is_empty() && output.stderr.is_empty() {
            return Err(ThresholdError::CliError {
                provider: "claude".to_string(),
                code: 0,
                stderr: "CLI did not produce any output".to_string(),
            });
        }

        // TODO: Parse version and validate >= 0.98.0 for production use
        // For now, we just verify the CLI responds

        Ok(())
    }

    fn build_new_session_args(
        &self,
        conversation_id: Uuid,
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
        args.push(conversation_id.to_string());

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
```

---

## Phase 2.7: Image Attachments (Optional for MVP)

**Note:** This phase can be deferred if not needed immediately. The core functionality (text messages) is in Phase 2.6.

For now, we'll implement a stub that returns an error:

```rust
// In claude.rs
pub async fn send_message_with_images(
    &self,
    _conversation_id: Uuid,
    _message: &str,
    _images: Vec<ImageAttachment>,
    _system_prompt: Option<&str>,
    _model: Option<&str>,
) -> crate::Result<ClaudeResponse> {
    Err(ThresholdError::Config("Image attachments not yet implemented".to_string()))
}
```

---

## Testing Strategy

### Unit Tests (Run Always)
- Response parsing with various JSON shapes
- Model alias resolution
- Argument building (new vs resume)
- Error classification

### Integration Tests (Require Claude CLI)
Mark with `#[ignore]` to skip in CI without `claude`:

```rust
#[tokio::test]
#[ignore]
async fn integration_send_simple_message() {
    let client = ClaudeClient::new(
        "claude".to_string(),
        PathBuf::from("/tmp/threshold-test"),
        true,  // skip permissions for test
    ).await.unwrap();

    let response = client.send_message(
        Uuid::new_v4(),
        "Say 'Hello' and nothing else",
        None,
        Some("haiku"),
    ).await.unwrap();

    assert!(response.text.to_lowercase().contains("hello"));
}
```

---

## Implementation Order

1. **Create `cli-wrapper` crate** with Cargo.toml
2. **Phase 2.1**: Process spawning + basic tests
3. **Phase 2.2**: Response parsing + tests
4. **Phase 2.4**: Model resolution (simple, can do early)
5. **Phase 2.3**: Session management + tests
6. **Phase 2.5**: Execution queue (simple)
7. **Phase 2.6**: ClaudeClient facade (ties everything together)
8. **Verify**: Run all tests, clippy, fmt
9. **Integration test**: Send a real message to Claude (requires auth)
10. **Phase 2.7**: (Deferred) Image attachments

---

## Verification Checklist

- [ ] Crate builds: `cargo build -p threshold-cli-wrapper`
- [ ] Unit tests pass: `cargo test -p threshold-cli-wrapper`
- [ ] Clippy clean: `cargo clippy -p threshold-cli-wrapper -- -D warnings`
- [ ] Formatted: `cargo fmt -p threshold-cli-wrapper -- --check`
- [ ] Integration test with real Claude CLI (manual, requires auth)
- [ ] Session persistence works (check `cli-sessions.json`)
- [ ] Second message uses `--resume` (verify via logs)
- [ ] `ANTHROPIC_API_KEY` is NOT in child env (verify no API billing)
- [ ] Timeout handling works (mock with `sleep`)

---

## Open Questions / Decisions Needed

1. **State directory location**: Use `~/.threshold/state/` or config-driven?
   - **Decision**: Use `config.data_dir().join("state")` for consistency

2. **Default model**: sonnet or haiku for new sessions?
   - **Decision**: sonnet (more capable, mentioned in milestone doc)

3. **Timeout**: 300 seconds sufficient?
   - **Decision**: Yes, configurable via `CliProcess::with_timeout()`

4. **Large message handling**: When to use stdin vs args?
   - **Decision**: Defer to Phase 2.6 implementation, add if needed

---

## Risks & Mitigations

| Risk | Mitigation | Status |
|------|-----------|--------|
| CLI version incompatibility | Document min version (0.98.0), add TODO for version parsing | ✅ Documented |
| Session file corruption | Catch parse errors on load, reset to empty with warning | ✅ Fixed |
| Concurrent access to session file (single process) | Use RwLock, serialize all writes | ✅ Implemented |
| Concurrent access from multiple Threshold processes | **Accepted limitation** - document that only one instance should run | ⚠️ Documented |
| API key leak via env | Explicitly clear in process.rs, add verification test | ✅ Implemented |
| Timeout too short for large tasks | Make configurable, default 300s (conservative) | ✅ Implemented |
| Timeout bug (child handle consumed) | Use tokio::select! to preserve handle | ✅ Fixed |
| Large messages exceed command-line length | **Accepted limitation** - will add stdin if needed | ⚠️ Deferred |
| Session file permissions leak data | Set 0o600 on Unix | ✅ Fixed |

---

## Success Criteria

✅ Can send a message to Claude and get a response back
✅ Second message to same conversation uses `--resume`
✅ Sessions persist across restarts
✅ Errors are properly classified (auth, billing, timeout, etc.)
✅ No ANTHROPIC_API_KEY in child process env
✅ All tests pass (unit + integration)
✅ Clippy clean, formatted
✅ Ready for integration with Discord bot (Milestone 3)
