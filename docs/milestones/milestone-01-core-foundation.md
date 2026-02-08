# Milestone 1 — Core Foundation

**Crate:** `core`
**Complexity:** Medium
**Dependencies:** None (this is the root)

## What This Milestone Delivers

The `core` crate is the bedrock everything else imports. It provides:

- Project-wide error types
- Core domain types (Conversation, Portal, Agent, Message, etc.)
- TOML configuration loading and validation
- OS keychain secrets management with env var fallback
- Append-only JSONL audit trail writer
- Structured logging infrastructure

No runnable binary yet — this is library-only. But the entire type system and
config pipeline are established.

---

## Phase 1.1 — Cargo Workspace Scaffold

Set up the Cargo workspace with the `core` crate and establish project-wide
conventions.

### Files to Create

**`Cargo.toml`** (workspace root):
```toml
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"
```

**`crates/core/Cargo.toml`**:
```toml
[package]
name = "threshold-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
keyring = { version = "3", features = ["apple-native", "windows-native", "sync-secret-service"] }
tokio = { version = "1", features = ["full"] }
dirs = "6"
```

**`.rustfmt.toml`**:
```toml
max_width = 100
use_field_init_shorthand = true
```

**`.gitignore`** additions:
```
/target
*.swp
*.swo
.env
```

### Crate Module Structure

```
crates/core/src/
  lib.rs          — re-exports all public modules
  error.rs        — ThresholdError enum
  types.rs        — core domain types
  config.rs       — configuration loading
  secrets.rs      — OS keychain wrapper
  audit.rs        — JSONL audit trail
  logging.rs      — tracing setup
```

---

## Phase 1.2 — Error Types

Define the project-wide error hierarchy using `thiserror`.

### `crates/core/src/error.rs`

```rust
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ThresholdError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Configuration file not found: {path}")]
    ConfigNotFound { path: PathBuf },

    #[error("Secret not found: {key}")]
    SecretNotFound { key: String },

    #[error("CLI error: {provider} exited with code {code}: {stderr}")]
    CliError {
        provider: String,
        code: i32,
        stderr: String,
    },

    #[error("CLI timeout after {timeout_ms}ms")]
    CliTimeout { timeout_ms: u64 },

    #[error("CLI not found: {command} — is it installed?")]
    CliNotFound { command: String },

    #[error("Discord error: {0}")]
    Discord(String),

    #[error("Tool error in '{tool}': {message}")]
    ToolError { tool: String, message: String },

    #[error("Tool not permitted: '{tool}' is not in the {profile:?} profile")]
    ToolNotPermitted { tool: String, profile: String },

    #[error("Conversation not found: {id}")]
    ConversationNotFound { id: uuid::Uuid },

    #[error("Portal not found: {id}")]
    PortalNotFound { id: uuid::Uuid },

    #[error("Audit trail write failed: {0}")]
    AuditWrite(#[source] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Keychain error: {0}")]
    Keychain(String),
}

pub type Result<T> = std::result::Result<T, ThresholdError>;
```

### Design Notes

- Each variant carries enough context to produce a useful error message
- `#[from]` conversions for common error types (`serde_json`, `toml`, `io`)
- `#[source]` on `AuditWrite` to chain the underlying IO error
- Other crates (cli-wrapper, discord, etc.) can wrap their domain errors into
  the appropriate variant

---

## Phase 1.3 — Core Domain Types

All foundational types that every crate will use.

### `crates/core/src/types.rs`

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ──── Conversations ────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl ConversationId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConversationMode {
    General,
    Coding { project: String },
    Research { topic: String },
}

impl ConversationMode {
    /// A stable string key for finding existing conversations by mode.
    pub fn key(&self) -> String {
        match self {
            Self::General => "general".to_string(),
            Self::Coding { project } => format!("coding:{}", project.to_lowercase()),
            Self::Research { topic } => format!("research:{}", topic.to_lowercase()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub mode: ConversationMode,
    pub cli_provider: CliProvider,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    // NOTE: cli_session_id is NOT stored here. The SessionManager in the
    // cli-wrapper crate is the single source of truth for CLI session IDs,
    // keyed by ConversationId. This avoids two sources of truth drifting.
}

// ──── CLI Providers ────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CliProvider {
    Claude { model: String },
    // Future: Codex { model: String, approval_mode: String },
}

// ──── Portals ────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortalId(pub Uuid);

impl PortalId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PortalType {
    Discord { guild_id: u64, channel_id: u64 },
    // Future:
    // Voice { device_id: String, room: String },
    // Web { session_token: String },
    // Phone { number: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portal {
    pub id: PortalId,
    pub portal_type: PortalType,
    pub conversation_id: ConversationId,
    pub connected_at: DateTime<Utc>,
}

// ──── Agents ────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub cli_provider: CliProvider,
    pub system_prompt: Option<String>,
    pub tool_profile: ToolProfile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolProfile {
    Minimal,
    Coding,
    Full,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolPermissionMode {
    FullAuto,
    ApproveDestructive,
    ApproveAll,
}

// ──── Messages ────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub conversation_id: ConversationId,
    pub role: MessageRole,
    pub content: String,
    pub portal_source: Option<PortalId>,
    pub timestamp: DateTime<Utc>,
}
```

### Design Notes

- `ConversationId` and `PortalId` are newtypes around `Uuid` for type safety
- `ConversationMode::key()` provides a stable lookup key so we can find existing
  conversations ("coding:myproject" always maps to the same conversation)
- `CliProvider` is an enum to support future providers (Codex) without
  restructuring
- `ToolProfile` and `ToolPermissionMode` are separate concepts: profile controls
  *which* tools are available, permission mode controls *how* destructive tools
  require confirmation

---

## Phase 1.4 — Configuration System

TOML configuration loading with `serde` deserialization and validation.

### `crates/core/src/config.rs`

```rust
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct ThresholdConfig {
    pub data_dir: Option<PathBuf>,
    pub log_level: Option<String>,       // "debug", "info", "warn", "error"

    pub cli: CliConfig,
    pub discord: Option<DiscordConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfigToml>,
    #[serde(default)]
    pub tools: ToolsConfig,
    pub heartbeat: Option<HeartbeatConfig>,
    pub scheduler: Option<SchedulerConfig>,
}

// ── CLI ──

#[derive(Debug, Deserialize)]
pub struct CliConfig {
    pub claude: ClaudeCliConfig,
    // Future: pub codex: Option<CodexCliConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeCliConfig {
    pub command: Option<String>,         // Default: "claude"
    pub model: Option<String>,           // Default: "sonnet"
    pub timeout_seconds: Option<u64>,    // Default: 300
    pub skip_permissions: Option<bool>,  // Default: false
    #[serde(default)]
    pub extra_flags: Vec<String>,
}

// ── Discord ──

#[derive(Debug, Deserialize, Clone)]
pub struct DiscordConfig {
    pub guild_id: u64,
    pub allowed_user_ids: Vec<u64>,
    // bot_token resolved from keychain, NEVER stored here
}

// ── Agents ──

#[derive(Debug, Deserialize)]
pub struct AgentConfigToml {
    pub id: String,
    pub name: String,
    pub cli_provider: String,            // "claude"
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub system_prompt_file: Option<String>, // Path to .md file
    pub tools: Option<String>,           // "minimal", "coding", "full"
}

// ── Tools ──

#[derive(Debug, Default, Deserialize)]
pub struct ToolsConfig {
    pub permission_mode: Option<String>, // "full-auto", "approve-destructive", "approve-all"
    pub browser: Option<BrowserToolConfig>,
    pub gmail: Option<GmailToolConfig>,
    pub image_gen: Option<ImageGenToolConfig>,
}

#[derive(Debug, Deserialize)]
pub struct BrowserToolConfig {
    pub enabled: bool,
    pub headless: Option<bool>,
    pub allowed_origins: Option<Vec<String>>,
    pub blocked_origins: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct GmailToolConfig {
    pub enabled: bool,
    pub inboxes: Option<Vec<String>>,
    pub allow_send: Option<bool>,        // Default: false
}

#[derive(Debug, Deserialize)]
pub struct ImageGenToolConfig {
    pub enabled: bool,
}

// ── Heartbeat ──

#[derive(Debug, Deserialize, Clone)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    pub interval_minutes: Option<u64>,   // Default: 30
    pub instructions_file: Option<String>, // Default: "heartbeat.md"
    pub notification_channel_id: Option<u64>, // Discord channel for reports
}

// ── Scheduler ──

#[derive(Debug, Deserialize, Clone)]
pub struct SchedulerConfig {
    pub enabled: bool,
}
```

### Configuration Loading

```rust
impl ThresholdConfig {
    /// Load config from the default path or THRESHOLD_CONFIG env var.
    pub fn load() -> crate::Result<Self> {
        let path = std::env::var("THRESHOLD_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Self::default_config_path());

        if !path.exists() {
            return Err(crate::ThresholdError::ConfigNotFound { path });
        }

        let content = std::fs::read_to_string(&path)?;
        let config: Self = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Default root: $HOME/.threshold
    ///
    /// We use a single unified directory across all platforms rather than
    /// splitting via ProjectDirs (which would put config in one place and
    /// data in another depending on the OS). This keeps things predictable
    /// and matches the layout described in all documentation.
    ///
    /// On Windows: %USERPROFILE%\.threshold\config.toml
    /// On Unix:    $HOME/.threshold/config.toml
    pub fn default_config_path() -> PathBuf {
        Self::default_root().join("config.toml")
    }

    /// Resolved data directory — same as root unless overridden.
    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(Self::default_root)
    }

    /// The root directory: $HOME/.threshold
    fn default_root() -> PathBuf {
        dirs::home_dir()
            .expect("Could not determine home directory")
            .join(".threshold")
    }

    /// Validate required fields and enum values.
    fn validate(&self) -> crate::Result<()> {
        // Must have at least one agent or we create a default
        // Discord config must have guild_id and at least one user
        // etc.
        Ok(())
    }
}
```

### Example Config File

```toml
# ~/.threshold/config.toml

log_level = "info"

[cli.claude]
model = "sonnet"
timeout_seconds = 300
skip_permissions = true

[discord]
guild_id = 123456789012345678
allowed_user_ids = [987654321098765432]

[tools]
permission_mode = "full-auto"

[tools.browser]
enabled = false
headless = true

[tools.gmail]
enabled = false

[tools.image_gen]
enabled = false

[heartbeat]
enabled = true
interval_minutes = 30

[scheduler]
enabled = true

[[agents]]
id = "default"
name = "Assistant"
cli_provider = "claude"
tools = "full"

[[agents]]
id = "coder"
name = "Code Assistant"
cli_provider = "claude"
model = "opus"
system_prompt = "You are a coding assistant. Focus on code quality and testing."
tools = "coding"
```

---

## Phase 1.5 — Secrets Store

Wrapper around the `keyring` crate for OS keychain access with env var fallback.

### `crates/core/src/secrets.rs`

```rust
pub struct SecretStore {
    service_name: String,
}

impl SecretStore {
    pub fn new() -> Self {
        Self { service_name: "threshold".to_string() }
    }

    /// Store a secret in the OS keychain.
    pub fn set(&self, key: &str, value: &str) -> crate::Result<()>;

    /// Retrieve a secret from the OS keychain.
    pub fn get(&self, key: &str) -> crate::Result<Option<String>>;

    /// Delete a secret from the OS keychain.
    pub fn delete(&self, key: &str) -> crate::Result<()>;

    /// Resolve a secret: keychain first, then env var fallback.
    /// Returns None if neither source has the value.
    pub fn resolve(&self, keychain_key: &str, env_var: &str) -> Option<String> {
        self.get(keychain_key).ok().flatten()
            .or_else(|| std::env::var(env_var).ok())
    }
}
```

### Well-Known Secret Keys

| Keychain Key | Env Var Fallback | Used By |
|--------------|-----------------|---------|
| `discord-bot-token` | `DISCORD_BOT_TOKEN` | Discord portal |
| `google-api-key` | `GOOGLE_API_KEY` | Gmail, NanoBanana |
| `github-token` | `GITHUB_TOKEN` | Git operations |
| `elevenlabs-api-key` | `ELEVENLABS_API_KEY` | Future: TTS |
| `groq-api-key` | `GROQ_API_KEY` | Future: cloud STT |

### Design Notes

- Secrets are NEVER stored in `config.toml`
- The `keyring` crate uses Windows Credential Manager, macOS Keychain, or
  Linux Secret Service depending on platform
- Env var fallback supports containerized deployments where keychain isn't available
- A CLI helper command (`threshold secret set <key>`) can be added later for
  convenient secret management

---

## Phase 1.6 — JSONL Audit Trail

Append-only JSONL writer used by every subsystem for logging.

### `crates/core/src/audit.rs`

```rust
use serde::Serialize;
use std::path::PathBuf;

pub struct AuditTrail {
    path: PathBuf,
}

impl AuditTrail {
    pub fn new(path: PathBuf) -> Self { Self { path } }

    /// Append a single entry. Opens in append mode, writes one JSON line, flushes.
    pub async fn append<T: Serialize>(
        &self,
        event_type: &str,
        data: &T,
    ) -> crate::Result<()>;

    /// Read the last N entries (for display/debugging).
    pub async fn read_tail(&self, n: usize) -> crate::Result<Vec<serde_json::Value>>;

    /// Read all entries.
    pub async fn read_all(&self) -> crate::Result<Vec<serde_json::Value>>;
}
```

### Entry Format

Every entry is a single JSON line:

```json
{"ts":"2026-02-08T14:30:00Z","type":"user_message","data":{...}}
```

The `ts` and `type` fields are always present. The `data` field is the
caller-provided payload serialized as JSON.

### File Locations

| Trail | Path | Purpose |
|-------|------|---------|
| Conversation | `~/.threshold/audit/conversations/{id}.jsonl` | Per-conversation messages |
| Tools | `~/.threshold/audit/tools.jsonl` | All tool invocations |
| System | `~/.threshold/audit/system.jsonl` | Startup, shutdown, errors |

### Design Notes

- Append-only is crash-safe: partial writes lose at most one entry
- Each write opens the file in append mode — no long-lived file handles
- Parent directories are created automatically if they don't exist
- Files can be rotated externally (logrotate, etc.) without coordination

---

## Phase 1.7 — Logging Infrastructure

Set up structured logging with `tracing`.

### `crates/core/src/logging.rs`

```rust
pub fn init_logging(log_level: &str, log_dir: &Path) -> crate::Result<()> {
    // Console layer: human-readable, colored
    // File layer: JSON format to {log_dir}/threshold.log
    // Filter: from config log_level ("debug", "info", "warn", "error")
}
```

### Span Fields

Key spans carry contextual fields:
- `conversation_id` — which conversation is active
- `portal_id` — which portal originated the request
- `agent_id` — which agent is handling this
- `tool` — which tool is being executed

These propagate through all log entries within a request lifecycle.

---

## Verification Checklist

At the end of Milestone 1, the following should pass:

- [ ] `cargo build` succeeds for the `threshold-core` crate
- [ ] `cargo clippy` passes with no warnings
- [ ] Unit test: config loads from a TOML string, validates, applies defaults
- [ ] Unit test: config rejects missing required fields
- [ ] Unit test: SecretStore set/get/resolve with mock (integration test for real keychain)
- [ ] Unit test: AuditTrail writes valid JSONL and reads back correctly
- [ ] Unit test: AuditTrail handles concurrent appends without corruption
- [ ] Unit test: all domain types serialize/deserialize round-trip correctly
- [ ] Unit test: ConversationMode::key() produces stable, predictable keys
- [ ] Integration test: load a sample config.toml from a temp directory
