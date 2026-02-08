# Milestone 5 — Tool Framework

**Crate:** `tools`
**Complexity:** Large
**Dependencies:** Milestone 1 (core)

## What This Milestone Delivers

The trait-based tool system with built-in implementations for shell execution,
file operations, and web access. Tool profiles control which tools are available
per agent. All tool invocations are audit-logged.

### Relationship to CLI-Internal Tools

The Claude CLI has its own built-in tools (file editing, shell exec, etc.) that
it manages internally during conversations. Our tool framework serves a
**different and complementary** purpose:

1. **Heartbeat system** — the heartbeat needs to execute actions autonomously
2. **Cron scheduler** — scheduled tasks need to run commands, fetch URLs, etc.
3. **Integration tools** — capabilities outside the CLI's scope (Gmail, image
   generation, browser automation) that produce results flowing back through
   the conversation engine as `ConversationEvent::AssistantMessage` with
   artifacts (see Milestone 3)
4. **System prompt injection** — we inject our tool schemas into the CLI's
   system prompt so the AI knows what additional capabilities exist and can
   request them

When the AI (via CLI) requests one of our tools, the conversation engine
intercepts the request, executes the tool via `ToolRegistry`, and delivers the
result — including any artifacts — back through the conversation event system
to all attached portals.

---

## Phase 5.1 — Tool Trait and Registry

### `crates/tools/src/lib.rs`

```rust
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name (e.g., "exec", "read", "gmail").
    fn name(&self) -> &str;

    /// Human-readable description for the AI.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn schema(&self) -> Value;

    /// Execute the tool with the given parameters.
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult>;
}
```

### ToolContext

```rust
pub struct ToolContext {
    pub conversation_id: Option<ConversationId>,
    pub portal_id: Option<PortalId>,
    pub agent_id: String,
    pub working_dir: PathBuf,
    pub profile: ToolProfile,
    pub permission_mode: ToolPermissionMode,
    pub cancellation: tokio_util::sync::CancellationToken,
}
```

### ToolResult

```rust
pub struct ToolResult {
    pub content: String,            // Text output (max 100KB, truncated if larger)
    pub artifacts: Vec<Artifact>,   // Files, images, etc.
    pub success: bool,
}

pub struct Artifact {
    pub name: String,               // Filename
    pub data: Vec<u8>,              // Raw bytes
    pub mime_type: String,          // MIME type
}

const MAX_RESULT_SIZE: usize = 100 * 1024; // 100KB
```

### ToolRegistry

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    audit: AuditTrail,
}

impl ToolRegistry {
    pub fn new(config: &ToolsConfig, audit_path: PathBuf) -> Self;

    /// Register a tool.
    pub fn register(&mut self, tool: Arc<dyn Tool>);

    /// Execute a tool by name. Handles:
    /// 1. Permission check (is tool in the active profile?)
    /// 2. Audit log entry (before execution)
    /// 3. Execution with cancellation support
    /// 4. Result size guard (truncate if > 100KB)
    /// 5. Audit log completion (duration, success/failure)
    pub async fn execute(
        &self,
        tool_name: &str,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult>;

    /// Get tools available for a given profile.
    pub fn tools_for_profile(&self, profile: &ToolProfile) -> Vec<&dyn Tool>;

    /// Get tool schemas as JSON (for system prompt injection).
    pub fn schemas_for_profile(&self, profile: &ToolProfile) -> Vec<Value>;

    /// List all registered tool names.
    pub fn list(&self) -> Vec<&str>;
}
```

---

## Phase 5.2 — Tool Profile Enforcement

### Profile Definitions

| Profile | Tools |
|---------|-------|
| `Minimal` | `web_search`, `web_fetch`, `read` |
| `Coding` | Minimal + `write`, `edit`, `exec` |
| `Full` | All registered tools |

```rust
impl ToolProfile {
    pub fn allowed_tools(&self) -> Option<HashSet<&'static str>> {
        match self {
            Self::Minimal => Some(hashset!["web_search", "web_fetch", "read"]),
            Self::Coding => Some(hashset![
                "web_search", "web_fetch", "read", "write", "edit", "exec"
            ]),
            Self::Full => None,  // None means "allow all"
        }
    }

    pub fn allows(&self, tool_name: &str) -> bool {
        match self.allowed_tools() {
            Some(set) => set.contains(tool_name),
            None => true,
        }
    }
}
```

The `execute` method on `ToolRegistry` checks the profile before running:

```rust
if !ctx.profile.allows(tool_name) {
    return Err(ThresholdError::ToolNotPermitted {
        tool: tool_name.to_string(),
        profile: format!("{:?}", ctx.profile),
    });
}
```

---

## Phase 5.3 — Shell Exec Tool

### `crates/tools/src/builtin/exec.rs`

```rust
pub struct ExecTool;

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str { "exec" }

    fn description(&self) -> &str {
        "Execute a shell command and return its output."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 30000)"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Working directory for the command"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let command = params["command"].as_str()
            .ok_or_else(|| ThresholdError::ToolError {
                tool: "exec".into(),
                message: "Missing 'command' parameter".into(),
            })?;
        let timeout_ms = params["timeout_ms"].as_u64().unwrap_or(30_000);
        let cwd = params["working_dir"].as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        // Platform-aware shell
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        let output = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            Command::new(shell).arg(flag).arg(command).current_dir(&cwd).output()
        ).await
        .map_err(|_| ThresholdError::ToolError {
            tool: "exec".into(),
            message: format!("Command timed out after {}ms", timeout_ms),
        })??;

        Ok(ToolResult {
            content: format!(
                "exit code: {}\n\nstdout:\n{}\n\nstderr:\n{}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ),
            artifacts: vec![],
            success: output.status.success(),
        })
    }
}
```

---

## Phase 5.4 — File Tools (Read, Write, Edit)

### ReadTool

```rust
pub struct ReadTool;
// Params: { file_path: string, offset?: number, limit?: number }
// Returns file contents as text, with optional line range
// Fails if file doesn't exist
```

### WriteTool

```rust
pub struct WriteTool;
// Params: { file_path: string, content: string }
// Creates/overwrites file with the given content
// Creates parent directories if they don't exist
```

### EditTool

```rust
pub struct EditTool;
// Params: { file_path: string, old_text: string, new_text: string }
// Reads the file, replaces old_text with new_text, writes back
// Fails if old_text is not found in the file (prevents silent no-ops)
// Fails if old_text appears more than once (ambiguous edit)
```

---

## Phase 5.5 — Web Tools (Search, Fetch)

### WebSearchTool

```rust
pub struct WebSearchTool;
// Params: { query: string, max_results?: number }
// Implementation options:
//   - SearXNG (self-hosted meta search)
//   - DuckDuckGo HTML scraping
//   - Google Custom Search API (if configured)
// Returns list of results with title, URL, snippet
```

### WebFetchTool

```rust
pub struct WebFetchTool;
// Params: { url: string, extract_text?: bool }
// Fetches URL via reqwest with timeout (30s)
// If extract_text: strip HTML tags, return text content
// Truncate at 100KB
// User-Agent header to avoid blocks
```

---

## Phase 5.6 — Tool Audit Logging

Every tool invocation produces a JSONL entry in `~/.threshold/audit/tools.jsonl`:

```json
{
  "ts": "2026-02-08T14:30:00Z",
  "tool": "exec",
  "params": {"command": "ls -la"},
  "agent": "default",
  "conversation": "abc-123",
  "portal": "discord-456",
  "duration_ms": 45,
  "success": true,
  "result_size": 1234
}
```

### What's Logged

- Tool name and parameters (full params, not redacted)
- Agent ID, conversation ID, portal ID (context)
- Execution duration
- Success/failure
- Result size in bytes

### What's NOT Logged

- The full result content (could be huge)
- Image/artifact data (binary)

---

## Crate Module Structure

```
crates/tools/src/
  lib.rs              — Tool trait, ToolRegistry, ToolResult, public API
  profiles.rs         — ToolProfile enforcement
  context.rs          — ToolContext definition
  builtin/
    mod.rs            — register all built-in tools
    exec.rs           — shell execution
    file_read.rs      — file reading
    file_write.rs     — file writing
    file_edit.rs      — file editing (search-and-replace)
    web_search.rs     — web search
    web_fetch.rs      — URL fetching
```

---

## Verification Checklist

- [ ] `cargo build` succeeds for the `threshold-tools` crate
- [ ] Unit test: ToolRegistry registers tools and lists them
- [ ] Unit test: profile enforcement — Minimal blocks `exec`
- [ ] Unit test: profile enforcement — Coding allows `exec`
- [ ] Unit test: profile enforcement — Full allows everything
- [ ] Unit test: result size guard truncates content > 100KB
- [ ] Unit test: ExecTool runs `echo hello` and returns stdout
- [ ] Unit test: ExecTool respects timeout
- [ ] Unit test: ReadTool reads a file
- [ ] Unit test: ReadTool fails on missing file
- [ ] Unit test: WriteTool creates a file
- [ ] Unit test: EditTool replaces text
- [ ] Unit test: EditTool fails if search text not found
- [ ] Unit test: WebFetchTool fetches a URL (with mock HTTP server)
- [ ] Integration test: register all tools, execute each, verify audit log
- [ ] Integration test: audit trail entries have correct fields
