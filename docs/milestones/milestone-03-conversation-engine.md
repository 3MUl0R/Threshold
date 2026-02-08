# Milestone 3 — Conversation Engine

**Crate:** `conversation`
**Complexity:** Large
**Dependencies:** Milestone 1 (core), Milestone 2 (cli-wrapper)

## What This Milestone Delivers

The `conversation` crate is the central orchestrator — the beating heart of
Threshold. It manages first-class conversations, a portal registry, mode
switching, event broadcasting, and audit trail integration.

At the end of this milestone, you can programmatically send messages through
conversations, switch modes, and have multiple portals receive responses
from the same conversation.

---

## Phase 3.1 — Conversation Store

Persistent storage for conversation metadata.

### `crates/conversation/src/store.rs`

```rust
pub struct ConversationStore {
    state_path: PathBuf,   // ~/.threshold/state/conversations.json
    conversations: HashMap<ConversationId, Conversation>,
}
```

### API

```rust
impl ConversationStore {
    pub async fn load(data_dir: &Path) -> Result<Self>;
    pub async fn save(&self) -> Result<()>;

    /// Create a new conversation with the given mode and agent.
    pub fn create(
        &mut self,
        mode: ConversationMode,
        provider: CliProvider,
        agent_id: &str,
    ) -> &Conversation;

    pub fn get(&self, id: &ConversationId) -> Option<&Conversation>;
    pub fn get_mut(&mut self, id: &ConversationId) -> Option<&mut Conversation>;
    pub fn list(&self) -> Vec<&Conversation>;

    /// Find a conversation by its mode key (e.g., "coding:myproject").
    pub fn find_by_mode(&self, mode: &ConversationMode) -> Option<&Conversation>;

    /// Get or create the singleton General conversation.
    pub fn get_or_create_general(&mut self, agent_id: &str) -> &Conversation;
}
```

### Design Notes

- The General conversation is a singleton — `get_or_create_general` always
  returns the same conversation
- `find_by_mode` uses `ConversationMode::key()` for stable lookup
  (e.g., "coding:myproject" always resolves to the same conversation)
- State is a JSON file on disk, loaded on startup and saved after mutations
- Conversations are never deleted — they accumulate (with future archival)

---

## Phase 3.2 — Portal Registry

Track which portals are connected and what conversation each is attached to.

### `crates/conversation/src/portals.rs`

```rust
pub struct PortalRegistry {
    state_path: PathBuf,    // ~/.threshold/state/portals.json
    portals: HashMap<PortalId, Portal>,
}
```

### API

```rust
impl PortalRegistry {
    pub async fn load(data_dir: &Path) -> Result<Self>;
    pub async fn save(&self) -> Result<()>;

    /// Register a new portal, attached to the given conversation.
    pub fn register(
        &mut self,
        portal_type: PortalType,
        conversation_id: ConversationId,
    ) -> Portal;

    /// Remove a portal entirely (e.g., Discord channel deleted).
    pub fn unregister(&mut self, portal_id: &PortalId);

    /// Move a portal from its current conversation to a new one.
    pub fn attach(
        &mut self,
        portal_id: &PortalId,
        conversation_id: ConversationId,
    ) -> Result<()>;

    /// Get which conversation a portal is currently in.
    pub fn get_conversation(&self, portal_id: &PortalId) -> Option<&ConversationId>;

    /// Get all portals attached to a conversation (for broadcasting).
    pub fn get_portals_for_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Vec<&Portal>;

    /// Find a portal by Discord channel (guild_id + channel_id).
    pub fn find_by_discord_channel(
        &self,
        guild_id: u64,
        channel_id: u64,
    ) -> Option<&Portal>;
}
```

### Design Notes

- Portals are the bridge between interfaces and conversations
- A portal can only be in ONE conversation at a time
- Attaching to a new conversation implicitly detaches from the old one
- `find_by_discord_channel` is a convenience for the Discord integration

---

## Phase 3.3 — Conversation Engine (Orchestrator)

The main coordination layer that ties everything together.

### `crates/conversation/src/engine.rs`

```rust
/// Events emitted by the engine. Portals subscribe to these.
#[derive(Debug, Clone)]
pub enum ConversationEvent {
    AssistantMessage {
        conversation_id: ConversationId,
        content: String,
        artifacts: Vec<Artifact>,    // Images, files, etc. from tool results
        usage: Option<Usage>,
        timestamp: DateTime<Utc>,
    },
    Error {
        conversation_id: ConversationId,
        error: String,
    },
    ConversationCreated {
        conversation: Conversation,
    },
    PortalAttached {
        portal_id: PortalId,
        conversation_id: ConversationId,
    },
    PortalDetached {
        portal_id: PortalId,
        conversation_id: ConversationId,
    },
}

/// A file or binary artifact produced by a tool (image, PDF, etc.).
/// Defined here in the conversation crate so events can carry them;
/// re-exported from core so the tools crate uses the same type.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub name: String,
    pub data: Vec<u8>,
    pub mime_type: String,
}

pub struct ConversationEngine {
    conversations: Arc<RwLock<ConversationStore>>,
    portals: Arc<RwLock<PortalRegistry>>,
    claude: Arc<ClaudeClient>,
    agents: HashMap<String, AgentConfig>,
    event_tx: broadcast::Sender<ConversationEvent>,
    audit_dir: PathBuf,
}
```

### Primary API

```rust
impl ConversationEngine {
    pub fn new(config: &ThresholdConfig, claude: Arc<ClaudeClient>) -> Result<Self>;

    /// Subscribe to conversation events (portals call this).
    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent>;

    /// Handle an incoming user message from a portal.
    pub async fn handle_message(
        &self,
        portal_id: &PortalId,
        content: &str,
    ) -> Result<()>;

    /// Switch a portal to a different conversation mode.
    pub async fn switch_mode(
        &self,
        portal_id: &PortalId,
        mode: ConversationMode,
    ) -> Result<ConversationId>;

    /// List all active conversations.
    pub async fn list_conversations(&self) -> Vec<Conversation>;

    /// Attach a portal to a specific conversation by ID.
    pub async fn join_conversation(
        &self,
        portal_id: &PortalId,
        conversation_id: &ConversationId,
    ) -> Result<()>;

    /// Register a new portal (e.g., when a Discord channel first sends a message).
    pub async fn register_portal(
        &self,
        portal_type: PortalType,
    ) -> PortalId;

    /// Unregister a portal.
    pub async fn unregister_portal(&self, portal_id: &PortalId) -> Result<()>;

    /// Send a message directly to a conversation (for heartbeat, cron, etc.).
    pub async fn send_to_conversation(
        &self,
        conversation_id: &ConversationId,
        content: &str,
    ) -> Result<String>;
}
```

### `handle_message` Flow

This is the core message pipeline:

```
1. Look up portal → get current conversation_id
2. Look up conversation → get agent_id, cli_provider
3. Look up agent config → get system_prompt, model
4. Write user message to conversation audit trail
5. Call claude.send_message() with:
   - conversation_id (ClaudeClient uses this to look up session in
     SessionManager — the single source of truth for CLI session IDs)
   - user's message content
   - system_prompt (only used if this is a new session)
   - model (only used if this is a new session)
6. On success:
   a. Write assistant response to audit trail
   b. Update conversation.last_active
   c. Save conversation state
   d. Broadcast AssistantMessage event (with any artifacts)
7. On error:
   a. Write error to audit trail
   b. Broadcast Error event
```

### Broadcasting

The engine uses `tokio::sync::broadcast` to deliver events to all subscribers.
Each portal subscribes and filters events by conversation_id — it only acts
on events for the conversation it's currently attached to.

This is how "all portals in the same conversation see the same response" works.
When the AI responds, the engine broadcasts once, and every portal picks it up.

---

## Phase 3.4 — Mode Switching

Users switch conversations via commands like `/coding myproject`.

### Logic

```
switch_mode(portal_id, mode):
    1. Compute mode.key()  (e.g., "coding:myproject")
    2. Search conversations for one with matching key
    3. If found:
        → Reuse existing conversation
    4. If not found:
        → Resolve which agent handles this mode
        → Create new conversation with that agent
        → Broadcast ConversationCreated event
    5. Detach portal from current conversation
        → Broadcast PortalDetached event
    6. Attach portal to target conversation
        → Broadcast PortalAttached event
    7. Save portal state
    8. Return the conversation_id
```

### Agent Resolution

Different modes can map to different agents based on configuration:

```rust
fn resolve_agent_for_mode(&self, mode: &ConversationMode) -> &AgentConfig {
    match mode {
        ConversationMode::Coding { .. } => {
            // Look for an agent with id "coder", fall back to "default"
            self.agents.get("coder")
                .unwrap_or_else(|| self.agents.get("default").unwrap())
        }
        _ => self.agents.get("default").unwrap(),
    }
}
```

This is configurable — if no "coder" agent is defined, coding mode uses the
default agent.

---

## Phase 3.5 — Audit Trail Integration

Every message in and out is written to a per-conversation JSONL file.

### Audit Event Types

```rust
#[derive(Serialize)]
#[serde(tag = "type")]
pub enum ConversationAuditEvent {
    UserMessage {
        portal_id: PortalId,
        portal_type: String,
        content: String,
        timestamp: DateTime<Utc>,
    },
    AssistantMessage {
        content: String,
        usage: Option<Usage>,
        duration_ms: u64,
        timestamp: DateTime<Utc>,
    },
    ModeSwitch {
        portal_id: PortalId,
        from_conversation: Option<ConversationId>,
        to_conversation: ConversationId,
        mode: ConversationMode,
        timestamp: DateTime<Utc>,
    },
    SessionCreated {
        cli_session_id: String,
        model: String,
        agent_id: String,
        timestamp: DateTime<Utc>,
    },
    Error {
        error: String,
        timestamp: DateTime<Utc>,
    },
}
```

### Purpose

This audit trail is our record, NOT the CLI's. The CLI manages its own
context internally. Our audit trail is for:

- Displaying conversation history in Discord (catch-up messages)
- Cross-portal message replay ("what did we talk about?")
- Debugging and accountability
- **Never** fed back to the CLI as context

---

## Crate Module Structure

```
crates/conversation/src/
  lib.rs          — re-exports ConversationEngine as primary API
  store.rs        — ConversationStore
  portals.rs      — PortalRegistry
  engine.rs       — ConversationEngine orchestrator
  audit.rs        — conversation-specific audit event types
```

---

## Verification Checklist

- [ ] Unit test: ConversationStore create, get, list, find_by_mode
- [ ] Unit test: General conversation auto-creation (singleton)
- [ ] Unit test: ConversationMode::key() stability (same input = same key)
- [ ] Unit test: PortalRegistry register, attach, detach, lookup
- [ ] Unit test: find_by_discord_channel returns correct portal
- [ ] Unit test: mode switching — finds existing conversation
- [ ] Unit test: mode switching — creates new conversation when none exists
- [ ] Unit test: agent resolution — coding mode uses "coder" if defined
- [ ] Unit test: agent resolution — falls back to "default"
- [ ] Integration test: create engine, register portal, send message, verify
  audit trail written
- [ ] Integration test: two portals in same conversation both receive
  AssistantMessage events via broadcast
- [ ] Integration test: switch modes, verify old conversation persists
- [ ] Integration test: persistence — save state, reload, verify conversations
  and portals restored
- [ ] Integration test (with Claude CLI): full round-trip message through engine
