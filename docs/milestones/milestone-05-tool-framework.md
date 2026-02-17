# Milestone 5 — Tool Framework

**Crate:** `tools`
**Complexity:** Large
**Dependencies:** Milestone 1 (core), Milestone 2 (cli-wrapper), Milestone 3 (conversation)

## What This Milestone Delivers

An extensible tool framework that gives Claude **capabilities it doesn't have
natively**, using a CLI-based integration architecture.

### The Core Realization

Claude CLI already has built-in tools for file editing, shell execution, web
search, and web fetching. **We don't need to duplicate those.** Instead, our
tool framework extends Claude with capabilities it cannot access natively:

- **Schedule management** (Milestone 6) — create/modify/delete cron jobs and heartbeats
- **Browser automation** (Milestone 8) — Playwright integration
- **Gmail** (Milestone 9) — read/send email
- **Image generation** (Milestone 10) — Google Imagen API

### Architecture: CLI Subcommands

All custom tools are implemented as `threshold` CLI subcommands. Claude invokes
them using its native shell execution capability — no MCP server, no text-based
tool call parsing, no interception pipeline.

```
Claude needs to check email
    → Claude runs: threshold gmail list --inbox "user@gmail.com"
    → stdout returns JSON with email summaries
    → Claude reads the output naturally

Claude needs to schedule a task
    → Claude runs: threshold schedule script --name "nightly tests" --cron "0 0 3 * * *" --command "cargo test"
    → threshold CLI communicates with running daemon via Unix socket
    → stdout confirms creation with next run time
```

This approach:
- Is **token-efficient** — Claude naturally understands CLI tools
- Is **self-documenting** — `threshold --help` and `threshold <cmd> --help`
- **Eliminates circular dependencies** — no tool→scheduler→tool import cycles
- Requires **no text parsing** of Claude's responses
- Requires **no interception pipeline** or MCP protocol
- Works with Claude's **native exec** capability

Inspired by [Playwright's CLI guidance](https://github.com/anthropics/anthropic-cookbook):
CLI is more token-efficient for discrete actions. Claude understands `--help`
output and can discover capabilities on its own.

### What We Do NOT Build

| Component | Why Not |
|-----------|---------|
| `read`, `write`, `edit` tools | Claude has native file tools |
| `web_search`, `web_fetch` tools | Claude has native web tools |
| System prompt injection code module | Not needed — system prompt is static text |
| Tool call interception pipeline | Not needed — Claude calls CLI directly |
| MCP server | CLI subcommands are simpler and more token-efficient |

### How It All Connects

```
┌─────────────────────────────────────────────────────────┐
│                 Claude Conversation                       │
│                                                          │
│  System prompt includes:                                 │
│    "You have access to the `threshold` CLI..."           │
│                                                          │
│  Claude decides to check email:                          │
│    → exec("threshold gmail list --inbox user@gmail.com") │
│    → reads stdout, continues conversation                │
│                                                          │
│  Claude decides to schedule a task:                      │
│    → exec("threshold schedule script --name ...")        │
│    → daemon receives via Unix socket                     │
│    → stdout confirms, Claude continues                   │
└─────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────┐
│                 Scheduling System                         │
│                                                          │
│  Cron fires → What action?                               │
│                │                                         │
│                ├─ NewConversation → Claude CLI            │
│                ├─ ResumeConversation → ConversationEngine │
│                ├─ Script → shell exec (internal)         │
│                └─ ScriptThenConversation → exec → Claude │
└─────────────────────────────────────────────────────────┘
```

---

## Architecture: Unified Scheduling Model

A key architectural decision: **heartbeats and cron jobs are the same system
under the hood.** The heartbeat is simply a pre-configured scheduled task with:
- A dedicated instruction file (heartbeat.md)
- An associated conversation thread (always resumes)
- A skip-if-running guard
- Handoff notes for continuity between runs

The scheduling system (defined here, implemented in Milestone 6) supports
these action types:

```rust
// Defined in crates/core/src/types.rs — shared across all milestones
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScheduledAction {
    /// Launch a new Claude conversation with this prompt.
    /// Claude uses its native tools plus our custom CLI tools.
    NewConversation {
        prompt: String,
        model: Option<String>,
    },

    /// Resume an existing conversation thread.
    /// Maintains full conversation history and context.
    /// This is the action type used by heartbeats.
    ResumeConversation {
        conversation_id: ConversationId,
        prompt: String,
    },

    /// Run a script/command directly (no Claude involvement).
    /// For simple automation that doesn't need AI.
    Script {
        command: String,
        working_dir: Option<String>,
    },

    /// Run a script, then feed the output to Claude for analysis.
    /// Use {output} placeholder in prompt_template for script output.
    ScriptThenConversation {
        command: String,
        prompt_template: String,
        model: Option<String>,
    },
}
```

**This is the source of truth** for scheduled actions. Milestone 6 (unified
scheduler) implements the engine that executes them.

---

## What's Already Built (Phases 5.1-5.2)

The foundation is complete and solid:

### Tool Trait (`crates/tools/src/lib.rs`) ✅

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult>;
}
```

The Tool trait remains useful for the **scheduler's internal** tool execution
(e.g., running Script actions via ExecTool). It is NOT the integration layer
for Claude — that's the CLI.

### ToolContext (`crates/tools/src/context.rs`) ✅

```rust
pub struct ToolContext {
    pub conversation_id: Option<ConversationId>,
    pub portal_id: Option<PortalId>,
    pub agent_id: String,
    pub working_dir: PathBuf,
    pub profile: ToolProfile,
    pub permission_mode: ToolPermissionMode,
    pub cancellation: CancellationToken,
}
```

### ToolResult with truncation ✅

```rust
pub struct ToolResult {
    pub content: String,            // Max 100KB, truncated if larger
    pub artifacts: Vec<Artifact>,   // Files, images, etc.
    pub success: bool,
}
```

### ToolRegistry with audit logging ✅

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    audit: Arc<AuditTrail>,     // Uses core AuditTrail (mutex, 0600 perms, 64KB limit)
}
```

Execution pipeline: permission check → tokio::select! cancellation → execute →
truncate → audit log. All 24 foundation tests passing.

### Profile Enforcement via Extension Trait ✅

```rust
pub trait ToolProfileExt {
    fn allowed_tools(&self) -> Option<HashSet<&'static str>>;
    fn allows(&self, tool_name: &str) -> bool;
}
```

**Note:** Profile definitions need updating (see Phase 5.3 below).

### ExecTool (`crates/tools/src/builtin/exec.rs`) ✅

Shell command execution with concurrent stdout/stderr draining, timeout,
cancellation support, and relative path resolution. Used by the scheduler
for direct Script action execution. 6 tests passing.

---

## Phase 5.3 — Profile Rethink

Profiles now control what the **scheduler** can do internally. Claude's access
to CLI subcommands is governed by the system prompt and which commands are
available on the system.

### Updated Profile Definitions

| Profile | Scheduler Permissions | Use Case |
|---------|----------------------|----------|
| `Minimal` | None | Read-only agents |
| `Standard` | `exec` | Can run scripts via scheduler |
| `Full` | All internal tools | Full scheduler access |

```rust
impl ToolProfileExt for ToolProfile {
    fn allowed_tools(&self) -> Option<HashSet<&'static str>> {
        match self {
            Self::Minimal => Some(HashSet::new()),
            Self::Standard => Some(HashSet::from(["exec"])),
            Self::Full => None,  // All tools
        }
    }
    // allows() implementation unchanged
}
```

**Design decision:** Static profiles, not per-agent tool allowlists. Keeps the
system simple. If a use case arises for fine-grained tool access, we can add
configurable profiles later.

---

## Phase 5.4 — CLI Binary Skeleton

The existing `threshold` binary in `crates/server/` gets a subcommand
structure using `clap`. The `daemon` subcommand replaces the current
direct startup. Each integration milestone adds its own subcommands.

> **Note:** The binary stays in `crates/server/` (package name `threshold`).
> No need for a separate `crates/cli/` — the CLI and daemon are the same
> binary, selected by subcommand. Code references in this doc to
> `crates/cli/src/` should be read as `crates/server/src/`.

### Command Tree

```
threshold
  daemon              Start the threshold daemon (scheduler, Discord bot, etc.)
  schedule            (Milestone 6)
    conversation      Schedule a new-conversation task
    script            Schedule a script task
    monitor           Schedule a script-then-conversation task
    list              List all scheduled tasks
    delete <id>       Delete a task
    toggle <id>       Enable/disable a task
  gmail               (Milestone 9)
    auth              Run OAuth setup
    list              List recent messages
    read <id>         Read a message
    search            Search messages
    send              Send an email
    reply <id>        Reply to a message
  browser             (Milestone 8)
    open [url]        Open browser session
    goto <url>        Navigate to URL
    click <ref>       Click element
    screenshot        Take screenshot
    close             Close session
  imagegen            (Milestone 10)
    generate          Generate an image from text prompt
```

### Implementation

```rust
// crates/cli/src/main.rs
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "threshold", about = "AI agent framework with scheduling and integrations")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the threshold daemon
    Daemon(DaemonArgs),
    /// Manage scheduled tasks
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommands,
    },
    // Future milestones add more variants:
    // Gmail(GmailArgs),
    // Browser(BrowserArgs),
    // Imagegen(ImagegenArgs),
}

#[derive(Subcommand)]
enum ScheduleCommands {
    /// Create a new-conversation task (launches fresh Claude session)
    Conversation {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// Create a script task (no Claude involved)
    Script {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long)]
        command: String,
        #[arg(long)]
        working_dir: Option<String>,
    },
    /// Create a script-then-conversation task (script output fed to Claude)
    Monitor {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long)]
        command: String,
        #[arg(long, default_value = "Script output:\n{output}\n\nAnalyze and report any issues.")]
        prompt_template: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// List all scheduled tasks
    List {
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Delete a scheduled task
    Delete {
        /// Task ID or name
        id: String,
    },
    /// Enable or disable a task
    Toggle {
        /// Task ID or name
        id: String,
        #[arg(long)]
        enabled: bool,
    },
}
```

### Daemon Communication

Commands that interact with the running threshold daemon (e.g., `schedule`)
use a Unix domain socket at `~/.threshold/threshold.sock`. The daemon exposes
a JSON-over-socket API.

Commands that call external APIs directly (e.g., `gmail list`, `browser
screenshot`, `imagegen generate`) don't need the daemon — they execute
independently.

```rust
// crates/cli/src/daemon_client.rs
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    pub fn new() -> Self {
        Self {
            socket_path: dirs::home_dir()
                .unwrap_or_default()
                .join(".threshold/threshold.sock"),
        }
    }

    pub async fn send_command(&self, command: &DaemonCommand) -> Result<DaemonResponse> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        // Serialize command, send, read response
        todo!("Implementation in Milestone 6")
    }
}
```

### Output Format

All CLI subcommands output **structured JSON** to stdout by default (for Claude
to parse), with an optional `--format table` for human readability:

```
$ threshold schedule list --format json
[
  {"id": "abc-123", "name": "Nightly Tests", "cron": "0 0 3 * * *", "enabled": true, "next_run": "2026-02-17T03:00:00Z"},
  {"id": "def-456", "name": "Email Check", "cron": "0 0 * * * *", "enabled": true, "next_run": "2026-02-16T15:00:00Z"}
]

$ threshold schedule list --format table
ID        Name            Cron            Enabled  Next Run
abc-123   Nightly Tests   0 0 3 * * *     yes      2026-02-17 03:00
def-456   Email Check     0 0 * * * *     yes      2026-02-16 15:00
```

---

## Phase 5.5 — System Prompt Assembly

A small utility function assembles the tool availability section of the system
prompt based on the user's configuration. This is **not** a complex code module
— it's string concatenation based on which integrations are enabled.

```rust
// crates/tools/src/prompt.rs
pub fn build_tool_prompt(config: &ThresholdConfig) -> String {
    let mut sections = Vec::new();

    sections.push(
        "## Additional Tools\n\n\
         You have access to the `threshold` CLI for capabilities beyond your \
         native tools. Run `threshold --help` for a full list. Use your shell \
         execution capability to invoke these commands.\n"
    .to_string());

    if config.scheduler.as_ref().map_or(false, |s| s.enabled) {
        sections.push(
            "### Schedule Management\n\
             Create, list, and manage recurring tasks.\n\
             Run `threshold schedule --help` for full usage.\n\
             Example: `threshold schedule script --name \"nightly tests\" \
             --cron \"0 0 3 * * *\" --command \"cargo test\"`\n"
        .to_string());
    }

    if config.tools.gmail.as_ref().map_or(false, |g| g.enabled) {
        sections.push(
            "### Gmail\n\
             Read and send email.\n\
             Run `threshold gmail --help` for full usage.\n"
        .to_string());
    }

    if config.tools.browser.as_ref().map_or(false, |b| b.enabled) {
        sections.push(
            "### Browser Automation\n\
             Control a web browser via Playwright.\n\
             Run `threshold browser --help` for full usage.\n"
        .to_string());
    }

    if config.tools.image_gen.as_ref().map_or(false, |i| i.enabled) {
        sections.push(
            "### Image Generation\n\
             Generate images from text descriptions.\n\
             Run `threshold imagegen --help` for full usage.\n"
        .to_string());
    }

    sections.join("\n")
}
```

This prompt is prepended to the system prompt when launching Claude
conversations. The conversation engine calls `build_tool_prompt()` once at
setup time.

---

## Phase 5.6 — Path Resolution Utilities

Config paths use `~` for home directory and relative paths for `data_dir`.
A centralized utility resolves these consistently across the codebase.

```rust
// crates/core/src/paths.rs
use std::path::{Path, PathBuf};

/// Expand `~` to home directory and resolve relative paths against `data_dir`.
pub fn resolve_path(path: &str, data_dir: &Path) -> PathBuf {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        data_dir.join(p)
    }
}
```

Used by:
- `HeartbeatConfig.instruction_file` — resolved against `data_dir`
- `HeartbeatConfig.handoff_notes_path` — resolved against `data_dir`
- `SchedulerConfig.store_path` — resolved against `data_dir`
- Any future config path fields

---

## Phase 5.7 — Deprecation Cleanup

Remove tools that duplicate Claude's native capabilities:

- Delete `crates/tools/src/builtin/read.rs`
- Delete `crates/tools/src/builtin/write.rs`
- Delete `crates/tools/src/builtin/edit.rs`
- Update `crates/tools/src/builtin/mod.rs` to only export `ExecTool`

The `ExecTool` remains — it's used by the scheduler for direct `Script`
action execution (not through Claude).

---

## Phase 5.8 — Audit Logging ✅

Already implemented. Every tool invocation (via ToolRegistry) produces a JSONL
entry in `~/.threshold/audit/tools.jsonl`:

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

Uses `AuditTrail` from core with mutex serialization, Unix 0600 permissions,
and 64KB max entry size.

**Note:** CLI subcommand invocations (gmail, browser, imagegen) log their own
audit entries separately from the ToolRegistry pipeline. Each subcommand is
responsible for writing its audit entry before returning.

---

## Crate Module Structure

```
crates/tools/src/
  lib.rs              — Tool trait, ToolResult, Artifact, public API
  context.rs          — ToolContext definition
  profiles.rs         — ToolProfile extension trait (simplified)
  registry.rs         — ToolRegistry with audit integration
  prompt.rs           — System prompt assembly (Phase 5.5)
  builtin/
    mod.rs            — Built-in tool registration
    exec.rs           — Shell execution (for scheduler direct use)

crates/core/src/
  paths.rs            — Centralized path resolution (tilde expansion) [Phase 5.6 — Pending]

crates/server/src/    — threshold CLI binary (daemon + subcommands)
  main.rs             — Entrypoint with clap subcommands
  daemon_client.rs    — Unix socket client for daemon communication
  schedule.rs         — Schedule management commands (impl in Milestone 6)
  output.rs           — JSON/table output formatting utilities
```

---

## Implementation Status

| Phase | Description | Status |
|-------|-------------|--------|
| 5.1 | Tool Trait and Registry | ✅ Complete (24 tests) |
| 5.2 | Profile Enforcement (original) | ✅ Complete |
| 5.3 | Profile Rethink (Coding→Standard) | ✅ Complete |
| 5.4 | CLI Binary Skeleton | Pending |
| 5.5 | System Prompt Assembly | Pending |
| 5.6 | Path Resolution Utilities | Pending |
| 5.7 | Deprecation Cleanup | Pending |
| 5.8 | Audit Logging | ✅ Complete |

### Code to retain
- `lib.rs` — Tool trait, ToolResult, Artifact, MAX_RESULT_SIZE
- `context.rs` — ToolContext with builder pattern
- `profiles.rs` — ToolProfileExt trait (update allowed tool sets)
- `registry.rs` — ToolRegistry with Arc<AuditTrail>
- `builtin/exec.rs` — ExecTool (used by scheduler for Script actions)

### Code to deprecate
- `builtin/read.rs` — Claude has native file reading
- `builtin/write.rs` — Claude has native file writing
- `builtin/edit.rs` — Claude has native file editing

---

## Verification Checklist

### Foundation (Complete)
- [x] `cargo build` succeeds for the `threshold-tools` crate
- [x] Unit test: ToolRegistry registers tools and lists them
- [x] Unit test: profile enforcement blocks unpermitted tools
- [x] Unit test: profile enforcement allows permitted tools
- [x] Unit test: Full profile allows everything
- [x] Unit test: result size guard truncates content > 100KB
- [x] Unit test: ExecTool runs `echo hello` and returns stdout
- [x] Unit test: ExecTool respects timeout
- [x] Unit test: ExecTool handles large output without deadlock
- [x] Unit test: audit trail entries written correctly

### Profile Rethink (Phase 5.3) ✅
- [x] Unit test: Minimal profile allows no tools
- [x] Unit test: Standard profile allows exec only
- [x] Unit test: Full profile allows everything
- [x] Config validation accepts both "standard" and "coding" (backwards compat)

### CLI Binary (Phase 5.4)
- [ ] `threshold --help` shows available subcommands
- [ ] `threshold schedule --help` shows schedule commands
- [ ] CLI builds as standalone binary
- [ ] JSON output format is parseable
- [ ] Table output format is human-readable
- [ ] Daemon client connects to Unix socket

### System Prompt (Phase 5.5)
- [ ] Unit test: build_tool_prompt produces expected sections for enabled tools
- [ ] Unit test: disabled tools are excluded from prompt
- [ ] Unit test: empty config produces minimal prompt

### Path Resolution (Phase 5.6)
- [ ] Unit test: `~` expansion to home directory
- [ ] Unit test: relative paths resolved against data_dir
- [ ] Unit test: absolute paths pass through unchanged

### Deprecation (Phase 5.7)
- [ ] read.rs, write.rs, edit.rs removed
- [ ] builtin/mod.rs only exports ExecTool
- [ ] All remaining tests pass
- [ ] `cargo build` succeeds after cleanup
