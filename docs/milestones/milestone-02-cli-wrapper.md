# Milestone 2 — Claude Code CLI Wrapper

**Crate:** `cli-wrapper`
**Complexity:** Medium
**Dependencies:** Milestone 1 (core types, config, errors)

## What This Milestone Delivers

The `cli-wrapper` crate spawns `claude` as a subprocess, manages sessions,
parses JSON responses, and enforces sequential execution. At the end of this
milestone, you can send a message to Claude via CLI and get a structured
response back, all from Rust.

---

## Phase 2.1 — CLI Subprocess Spawning

The fundamental subprocess management using `tokio::process::Command`.

### `crates/cli-wrapper/src/process.rs`

```rust
pub struct CliProcess {
    command: String,             // "claude"
    env_clear: Vec<String>,     // ["ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_OLD"]
    timeout: Duration,
}

pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration: Duration,
}
```

### Critical Implementation Details

1. **Clear `ANTHROPIC_API_KEY`** — Without this, the CLI detects the env var
   and uses it for direct API calls, which bypasses subscription billing and
   charges at API rates. Must also clear `ANTHROPIC_API_KEY_OLD`.

2. **Piped stdout/stderr** — Capture both via piped handles. The response JSON
   comes from stdout. Errors and diagnostics come from stderr.

3. **Timeout** — Wrap the child process wait in `tokio::time::timeout`. On
   timeout, kill the child process. CLI calls can be long-running for complex
   tool chains, so the default timeout should be generous (300 seconds).

4. **Working directory** — Configurable per invocation. Important for coding
   sessions where the CLI needs to operate in a project directory.

5. **Logging** — Log the full command with arguments (redact secrets) via
   `tracing::debug!` before spawning.

---

## Phase 2.2 — Response Parsing

Parse the JSON output from `claude -p --output-format json`.

### `crates/cli-wrapper/src/response.rs`

```rust
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
```

### Parsing Strategy

The CLI's JSON output isn't always in the same shape. The parser must be
resilient:

**Text extraction** (try in order):
1. `root.message` (string)
2. `root.content` (string)
3. `root.result` (string)
4. Root object itself as string (last resort)

**Session ID extraction** (try in order):
1. `root.session_id`
2. `root.sessionId`
3. `root.conversation_id`
4. `root.conversationId`

**Fallback**: If JSON parsing fails entirely but stdout is non-empty, return
the raw stdout as the text field. This handles edge cases where the CLI
outputs plain text instead of JSON.

---

## Phase 2.3 — Session Management

Track CLI session IDs and decide between new-session and resume-session
invocations.

### `crates/cli-wrapper/src/session.rs`

```rust
pub struct SessionManager {
    sessions: HashMap<Uuid, String>,  // conversation_id -> cli_session_id
    state_path: PathBuf,
}
```

### Decision Logic

```
IF conversation has an existing cli_session_id:
    → Resume mode
    → Args: --resume <session_id> (NO model, NO system prompt, NO --session-id)
    → Message is the only additional argument
ELSE:
    → New session mode
    → Args: --session-id <new_uuid> --model <model> --append-system-prompt <prompt>
    → Message is the last argument
```

### Critical: Resume Strips Everything

When resuming, the CLI receives ONLY:
- `-p`
- `--output-format json`
- `--dangerously-skip-permissions` (if configured)
- `--resume <session_id>`
- The user's message

**NOT included on resume:** `--model`, `--append-system-prompt`, `--session-id`

The CLI manages all context internally from its own session state. You cannot
change the model or system prompt mid-session.

### Persistence

The session map is saved to `~/.threshold/state/sessions.json` on every update
and loaded on startup. This ensures session continuity across server restarts.

---

## Phase 2.4 — Model Alias Resolution

### `crates/cli-wrapper/src/models.rs`

```rust
use std::borrow::Cow;

/// Resolve user-friendly model aliases to CLI model names.
/// Returns a Cow to avoid allocating for known aliases (which are
/// static strings) while still supporting pass-through of unknown models.
pub fn resolve_model_alias(input: &str) -> Cow<'static, str> {
    // Match against lowercase, but return static &str for known aliases
    let lower = input.to_lowercase();
    match lower.as_str() {
        "opus" | "opus-4" | "opus-4.5" | "opus-4.6" | "claude-opus" => Cow::Borrowed("opus"),
        "sonnet" | "sonnet-4" | "sonnet-4.1" | "sonnet-4.5" | "claude-sonnet" => Cow::Borrowed("sonnet"),
        "haiku" | "haiku-3.5" | "haiku-4.5" | "claude-haiku" => Cow::Borrowed("haiku"),
        _ => Cow::Owned(input.to_string()),  // Pass through unknown model strings
    }
}
```

Case-insensitive. Unknown model strings are passed through as-is (the CLI
will reject them if invalid). Uses `Cow<'static, str>` to avoid the lifetime
issue of returning a `&str` from a temporary `to_lowercase()` call.

---

## Phase 2.5 — Sequential Execution Queue

Prevent concurrent CLI executions for the same provider.

### `crates/cli-wrapper/src/queue.rs`

```rust
pub struct ExecutionQueue {
    lock: tokio::sync::Mutex<()>,
}

impl ExecutionQueue {
    pub fn new() -> Self { Self { lock: Mutex::new(()) } }

    pub async fn execute<F, T>(&self, f: F) -> T
    where
        F: Future<Output = T>,
    {
        let _guard = self.lock.lock().await;
        f.await
    }
}
```

### Why Sequential?

The Claude CLI maintains local state (session files, etc.). Concurrent
executions can cause race conditions on that shared state. The queue ensures
one CLI process runs at a time per provider.

This is a known pattern from the OpenClaw reference implementation (see
`docs/03-claude-code-cli-wrapper.md`, "CLI_RUN_QUEUE").

---

## Phase 2.6 — ClaudeClient Facade

The high-level API that other crates use.

### `crates/cli-wrapper/src/claude.rs`

```rust
pub struct ClaudeClient {
    process: CliProcess,
    sessions: Arc<RwLock<SessionManager>>,
    queue: ExecutionQueue,
    config: ClaudeCliConfig,
}
```

### Primary API

```rust
impl ClaudeClient {
    /// Send a message. Auto-decides new vs resume based on session state.
    pub async fn send_message(
        &self,
        conversation_id: Uuid,
        message: &str,
        system_prompt: Option<&str>,
        model: Option<&str>,
    ) -> Result<ClaudeResponse>;

    /// Force a new session (ignores any existing session for this conversation).
    pub async fn new_session(
        &self,
        conversation_id: Uuid,
        message: &str,
        system_prompt: &str,
        model: &str,
    ) -> Result<ClaudeResponse>;

    /// Check that the CLI is installed and responsive.
    pub async fn health_check(&self) -> Result<()>;
}
```

### `send_message` Internal Flow

1. Acquire the execution queue lock
2. Check if a session exists for this `conversation_id`
3. **If yes** — build resume args:
   - `-p --output-format json`
   - `--dangerously-skip-permissions` (if configured)
   - `--resume <session_id>`
   - Message as final argument
4. **If no** — build new session args:
   - `-p --output-format json`
   - `--dangerously-skip-permissions` (if configured)
   - `--session-id <new_uuid>`
   - `--model <resolved_model>`
   - `--append-system-prompt <system_prompt>` (if provided)
   - Message as final argument
5. Spawn the CLI process (via `CliProcess::run`)
6. Parse the response (via `ClaudeResponse::parse`)
7. Store the session_id from the response in SessionManager
8. Persist the session map
9. Return the parsed response

### Input Handling

- **Normal messages**: passed as the final CLI argument
- **Long messages** (exceeding a configurable threshold, e.g., 5000 chars):
  passed via stdin instead, to avoid command-line length limits
- **Stdin mode**: write to the child process's stdin, then close the write end

---

## Phase 2.7 — Image Attachment Support

Handle image inputs for multimodal conversations.

### `crates/cli-wrapper/src/images.rs`

```rust
pub struct ImageAttachment {
    pub data: Vec<u8>,
    pub format: ImageFormat,
}

pub enum ImageFormat { Png, Jpeg, Gif, Webp }

impl ClaudeClient {
    pub async fn send_message_with_images(
        &self,
        conversation_id: Uuid,
        message: &str,
        images: Vec<ImageAttachment>,
        system_prompt: Option<&str>,
        model: Option<&str>,
    ) -> Result<ClaudeResponse>;
}
```

### Image Handling Flow

1. Create a temp directory
2. Write each image as `image-1.png`, `image-2.jpg`, etc.
3. Set file permissions to owner read/write only (0o600 on Unix)
4. Add `--image <path>` flags to the CLI args (one per image)
5. Run the CLI
6. **Cleanup**: delete the temp directory in a drop guard (even on error/panic)

---

## Error Classification

Non-zero exit codes from the CLI are classified for upstream handling:

| Pattern | Error Type | Meaning |
|---------|-----------|---------|
| Exit code + "401" in stderr | `CliError` (auth) | Authentication expired |
| Exit code + "402" in stderr | `CliError` (billing) | Billing issue |
| Exit code + "429" in stderr | `CliError` (rate_limit) | Rate limited |
| Timeout | `CliTimeout` | Process exceeded timeout |
| Command not found | `CliNotFound` | CLI not installed |
| Other non-zero | `CliError` (unknown) | Unclassified error |

These classifications enable the conversation engine to take appropriate
action (e.g., notify the user that they need to re-authenticate).

---

## Crate Module Structure

```
crates/cli-wrapper/src/
  lib.rs          — re-exports ClaudeClient as the primary API
  process.rs      — CliProcess subprocess spawning
  response.rs     — ClaudeResponse parsing
  session.rs      — SessionManager
  models.rs       — model alias resolution
  queue.rs        — ExecutionQueue
  claude.rs       — ClaudeClient facade
  images.rs       — image attachment handling
```

---

## Verification Checklist

- [ ] `cargo build` succeeds for the `threshold-cli-wrapper` crate
- [ ] Unit test: response parsing — standard JSON with `message` field
- [ ] Unit test: response parsing — JSON with `content` field (fallback)
- [ ] Unit test: response parsing — JSON with `result` field (fallback)
- [ ] Unit test: response parsing — malformed JSON falls back to raw text
- [ ] Unit test: session_id extraction from all four field names
- [ ] Unit test: model alias resolution (all aliases + case insensitivity)
- [ ] Unit test: argument building — new session includes all flags
- [ ] Unit test: argument building — resume only includes resume flag + message
- [ ] Unit test: argument building — `--dangerously-skip-permissions` conditional
- [ ] Integration test: `health_check()` succeeds (claude --version)
- [ ] Integration test: send a simple message, get a response
- [ ] Integration test: follow-up message uses `--resume`
- [ ] Integration test: verify `ANTHROPIC_API_KEY` is NOT in child env
- [ ] Integration test: timeout fires for a mock that hangs
