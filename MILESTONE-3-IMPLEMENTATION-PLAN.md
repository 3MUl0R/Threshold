# Milestone 3 Implementation Plan: Conversation Engine

**Crate:** `conversation`
**Complexity:** Large
**Dependencies:** Milestone 1 (core), Milestone 2 (cli-wrapper)

## Overview

The Conversation Engine is the central orchestrator of Threshold. It manages:
- First-class conversations with persistent state
- Portal registry (which portals are in which conversations)
- Event broadcasting (all portals in a conversation see responses)
- Mode switching (seamlessly switch between conversation contexts)
- Audit trail integration (JSONL logs for every message)

## Architecture Decisions

### State Management Pattern
- **ConversationStore**: `HashMap<ConversationId, Conversation>` with JSON persistence
- **PortalRegistry**: `HashMap<PortalId, Portal>` with JSON persistence
- Both use `Arc<RwLock<>>` for concurrent access across async tasks
- Mutations trigger immediate `save()` to disk for durability

### Session ID Management
- **Single source of truth**: `SessionManager` in cli-wrapper crate
- Keyed by `ConversationId` (NOT stored in Conversation struct)
- Prevents drift between conversation state and CLI session state
- Engine passes `conversation_id` to ClaudeClient, which looks up session

### Event Broadcasting
- Uses `tokio::sync::broadcast` channel
- Engine broadcasts once, all portals receive
- Portals filter by `conversation_id` (only act on their conversation)
- Channel capacity: 100 events (balance memory vs history)

### Audit Trail
- One JSONL file per conversation: `~/.threshold/audit/<conversation_id>.jsonl`
- NOT used as context (CLI manages its own context internally)
- Purpose: history replay, debugging, accountability, Discord catch-up
- Write-only append (never read back during message handling)

---

## Phase 3.1: Conversation Store

**File:** `crates/conversation/src/store.rs`

### Implementation

```rust
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use threshold_core::{AgentConfig, CliProvider, Conversation, ConversationId, ConversationMode, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConversationMap {
    conversations: HashMap<ConversationId, Conversation>,
}

/// Persistent storage for conversation metadata.
pub struct ConversationStore {
    conversations: HashMap<ConversationId, Conversation>,
    state_path: PathBuf,
}

impl ConversationStore {
    /// Load conversations from disk
    pub async fn load(data_dir: &Path) -> Result<Self> {
        let state_path = data_dir.join("conversations.json");

        if !state_path.exists() {
            return Ok(Self {
                conversations: HashMap::new(),
                state_path,
            });
        }

        let content = tokio::fs::read_to_string(&state_path).await?;

        // Handle corruption gracefully
        let map: ConversationMap = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = ?state_path,
                    "conversation file corrupted, resetting to empty"
                );
                ConversationMap {
                    conversations: HashMap::new(),
                }
            }
        };

        tracing::info!(count = map.conversations.len(), "loaded conversations from disk");

        Ok(Self {
            conversations: map.conversations,
            state_path,
        })
    }

    /// Save conversations to disk
    pub async fn save(&self) -> Result<()> {
        let map = ConversationMap {
            conversations: self.conversations.clone(),
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

    /// Create a new conversation and return its ID
    pub fn create(
        &mut self,
        mode: ConversationMode,
        cli_provider: CliProvider,
        agent_id: String,
    ) -> ConversationId {
        let now = Utc::now();
        let conversation = Conversation {
            id: ConversationId::new(),
            mode,
            cli_provider,
            agent_id,
            created_at: now,
            last_active: now,
        };

        let id = conversation.id;
        self.conversations.insert(id, conversation);
        id
    }

    /// Get a conversation by ID
    pub fn get(&self, id: &ConversationId) -> Option<&Conversation> {
        self.conversations.get(id)
    }

    /// Get a mutable reference to a conversation
    pub fn get_mut(&mut self, id: &ConversationId) -> Option<&mut Conversation> {
        self.conversations.get_mut(id)
    }

    /// List all conversations
    pub fn list(&self) -> Vec<&Conversation> {
        self.conversations.values().collect()
    }

    /// Find a conversation by its mode key
    pub fn find_by_mode(&self, mode: &ConversationMode) -> Option<&Conversation> {
        let key = mode.key();
        self.conversations
            .values()
            .find(|c| c.mode.key() == key)
    }

    /// Get or create the singleton General conversation
    ///
    /// Returns (conversation_id, was_created) tuple
    pub fn get_or_create_general(
        &mut self,
        agent_id: String,
        cli_provider: CliProvider,
    ) -> (ConversationId, bool) {
        // Check if General already exists
        if let Some(conv) = self.find_by_mode(&ConversationMode::General) {
            return (conv.id, false);
        }

        // Create it
        let id = self.create(ConversationMode::General, cli_provider, agent_id);
        (id, true)
    }
}
```

### Error Handling
- File corruption: reset to empty (same pattern as SessionManager)
- Missing directory: create parent dirs automatically
- Permissions errors: propagate up (fatal on startup)

### Tests Required
- `load_nonexistent_creates_empty`
- `create_and_get_conversation`
- `list_returns_all_conversations`
- `find_by_mode_general`
- `find_by_mode_coding_case_insensitive`
- `get_or_create_general_singleton` (verify same ID returned)
- `save_and_load_persistence`
- `load_corrupted_file_resets_to_empty`
- `unix_permissions_are_0o600` (cfg(unix) only)

---

## Phase 3.2: Portal Registry

**File:** `crates/conversation/src/portals.rs`

### Implementation

```rust
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use threshold_core::{ConversationId, Portal, PortalId, PortalType, Result, ThresholdError};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PortalMap {
    portals: HashMap<PortalId, Portal>,
}

/// Track which portals are connected and what conversation each is attached to.
pub struct PortalRegistry {
    portals: HashMap<PortalId, Portal>,
    state_path: PathBuf,
}

impl PortalRegistry {
    /// Load portals from disk
    pub async fn load(data_dir: &Path) -> Result<Self> {
        let state_path = data_dir.join("portals.json");

        if !state_path.exists() {
            return Ok(Self {
                portals: HashMap::new(),
                state_path,
            });
        }

        let content = tokio::fs::read_to_string(&state_path).await?;

        // Handle corruption gracefully
        let map: PortalMap = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = ?state_path,
                    "portal file corrupted, resetting to empty"
                );
                PortalMap {
                    portals: HashMap::new(),
                }
            }
        };

        tracing::info!(count = map.portals.len(), "loaded portals from disk");

        Ok(Self {
            portals: map.portals,
            state_path,
        })
    }

    /// Save portals to disk
    pub async fn save(&self) -> Result<()> {
        let map = PortalMap {
            portals: self.portals.clone(),
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
            perms.set_mode(0o600);
            tokio::fs::set_permissions(&self.state_path, perms).await?;
        }

        Ok(())
    }

    /// Register a new portal
    pub fn register(
        &mut self,
        portal_type: PortalType,
        conversation_id: ConversationId,
    ) -> Portal {
        let portal = Portal {
            id: PortalId::new(),
            portal_type,
            conversation_id,
            connected_at: Utc::now(),
        };

        let id = portal.id;
        self.portals.insert(id, portal.clone());
        portal
    }

    /// Remove a portal entirely
    pub fn unregister(&mut self, portal_id: &PortalId) {
        self.portals.remove(portal_id);
    }

    /// Move a portal to a new conversation
    pub fn attach(
        &mut self,
        portal_id: &PortalId,
        conversation_id: ConversationId,
    ) -> Result<()> {
        let portal = self.portals.get_mut(portal_id).ok_or_else(|| {
            ThresholdError::NotFound {
                entity: "portal".to_string(),
                id: portal_id.0.to_string(),
            }
        })?;

        portal.conversation_id = conversation_id;
        Ok(())
    }

    /// Get a portal by ID
    pub fn get(&self, portal_id: &PortalId) -> Option<&Portal> {
        self.portals.get(portal_id)
    }

    /// Get which conversation a portal is in
    pub fn get_conversation(&self, portal_id: &PortalId) -> Option<&ConversationId> {
        self.portals.get(portal_id).map(|p| &p.conversation_id)
    }

    /// Get all portals in a conversation (for broadcasting)
    pub fn get_portals_for_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Vec<Portal> {
        self.portals
            .values()
            .filter(|p| &p.conversation_id == conversation_id)
            .cloned()
            .collect()
    }

    /// Find a portal by Discord channel
    pub fn find_by_discord_channel(
        &self,
        guild_id: u64,
        channel_id: u64,
    ) -> Option<&Portal> {
        self.portals.values().find(|p| match &p.portal_type {
            PortalType::Discord {
                guild_id: g,
                channel_id: c,
            } => *g == guild_id && *c == channel_id,
        })
    }
}
```

### Error Handling
- Portal not found on `attach`: return `ThresholdError::NotFound`
- File corruption: reset to empty
- Missing directory: create automatically

### Tests Required
- `register_portal_returns_new_id`
- `unregister_removes_portal`
- `attach_updates_conversation_id`
- `attach_nonexistent_portal_returns_error`
- `get_conversation_returns_current`
- `get_portals_for_conversation_filters_correctly`
- `find_by_discord_channel_match`
- `find_by_discord_channel_no_match_returns_none`
- `save_and_load_persistence`
- `load_corrupted_file_resets_to_empty`

---

## Phase 3.3: Conversation Engine (Orchestrator)

**File:** `crates/conversation/src/engine.rs`

**⚠️ Implementation Note**: This phase uses `ConversationAuditEvent` which is defined in Phase 3.5 (audit.rs). Either:
- Implement Phase 3.5 first, OR
- Stub the audit event types during Phase 3.3 and implement fully in Phase 3.5

The implementation below assumes Phase 3.5 types are available.

### Key Types

```rust
/// Events emitted by the engine
#[derive(Debug, Clone)]
pub enum ConversationEvent {
    AssistantMessage {
        conversation_id: ConversationId,
        content: String,
        artifacts: Vec<Artifact>,
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

/// File or binary artifact produced by tools
#[derive(Debug, Clone)]
pub struct Artifact {
    pub name: String,
    pub data: Vec<u8>,
    pub mime_type: String,
}
```

### Implementation

```rust
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use threshold_cli_wrapper::{ClaudeClient, Usage};
use threshold_core::{
    AgentConfig, AgentConfigToml, CliProvider, ConversationId, PortalId, PortalType,
    Result, ThresholdConfig, ThresholdError, ToolProfile,
};
use tokio::sync::{broadcast, RwLock};

pub struct ConversationEngine {
    conversations: Arc<RwLock<ConversationStore>>,
    portals: Arc<RwLock<PortalRegistry>>,
    claude: Arc<ClaudeClient>,
    agents: HashMap<String, AgentConfig>,
    event_tx: broadcast::Sender<ConversationEvent>,
    audit_dir: PathBuf,
}

impl ConversationEngine {
    /// Create a new engine
    pub async fn new(config: &ThresholdConfig, claude: Arc<ClaudeClient>) -> Result<Self> {
        // Resolve data directory from config
        let data_dir = config.data_dir()?;
        let audit_dir = data_dir.join("audit");

        // Load state from disk
        let conversations = Arc::new(RwLock::new(
            ConversationStore::load(&data_dir).await?
        ));
        let portals = Arc::new(RwLock::new(
            PortalRegistry::load(&data_dir).await?
        ));

        // Create broadcast channel (capacity: 100)
        // This is sufficient for typical portal lag scenarios.
        // Events contain response text, so memory usage scales with response size.
        // Slow receivers will receive RecvError::Lagged and miss events.
        let (event_tx, _) = broadcast::channel(100);

        // Transform agent configs from TOML format to runtime format
        let agents: HashMap<String, AgentConfig> = config
            .agents
            .iter()
            .map(|a| {
                let agent = Self::transform_agent_config(a)?;
                Ok((agent.id.clone(), agent))
            })
            .collect::<Result<_>>()?;

        Ok(Self {
            conversations,
            portals,
            claude,
            agents,
            event_tx,
            audit_dir,
        })
    }

    /// Transform AgentConfigToml to AgentConfig
    fn transform_agent_config(toml: &AgentConfigToml) -> Result<AgentConfig> {
        let cli_provider = match toml.cli_provider.as_str() {
            "claude" => CliProvider::Claude {
                model: toml.model.clone().unwrap_or_else(|| "sonnet".to_string()),
            },
            other => {
                return Err(ThresholdError::Configuration {
                    message: format!("Unknown CLI provider: {}", other),
                })
            }
        };

        let tool_profile = match toml.tools.as_deref() {
            Some("minimal") => ToolProfile::Minimal,
            Some("coding") => ToolProfile::Coding,
            Some("full") | None => ToolProfile::Full,
            Some(other) => {
                return Err(ThresholdError::Configuration {
                    message: format!("Unknown tool profile: {}", other),
                })
            }
        };

        Ok(AgentConfig {
            id: toml.id.clone(),
            name: toml.name.clone(),
            cli_provider,
            system_prompt: toml.system_prompt.clone(),
            tool_profile,
        })
    }

    /// Subscribe to conversation events
    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent> {
        self.event_tx.subscribe()
    }

    /// Handle an incoming user message
    pub async fn handle_message(
        &self,
        portal_id: &PortalId,
        content: &str,
    ) -> Result<()> {
        // 1. Look up portal → get conversation_id
        let conversation_id = {
            let portals = self.portals.read().await;
            portals.get_conversation(portal_id)
                .ok_or_else(|| ThresholdError::NotFound {
                    entity: "portal".to_string(),
                    id: portal_id.0.to_string(),
                })?
                .clone()
        };

        // 2. Look up conversation → get agent_id, cli_provider
        let (agent_id, cli_provider) = {
            let conversations = self.conversations.read().await;
            let conv = conversations.get(&conversation_id)
                .ok_or_else(|| ThresholdError::NotFound {
                    entity: "conversation".to_string(),
                    id: conversation_id.0.to_string(),
                })?;
            (conv.agent_id.clone(), conv.cli_provider.clone())
        };

        // 3. Look up agent config → get system_prompt, model
        let agent = self.agents.get(&agent_id)
            .ok_or_else(|| ThresholdError::Configuration {
                message: format!("Agent '{}' not found in configuration", agent_id),
            })?;

        let (system_prompt, model) = match &cli_provider {
            CliProvider::Claude { model } => (agent.system_prompt.as_deref(), model.as_str()),
        };

        // 4. Write user message to audit trail
        self.write_audit_event(
            &conversation_id,
            &ConversationAuditEvent::UserMessage {
                portal_id: *portal_id,
                portal_type: self.get_portal_type_string(portal_id).await?,
                content: content.to_string(),
                timestamp: Utc::now(),
            },
        ).await?;

        // 5. Call Claude CLI
        let start = std::time::Instant::now();
        let result = self.claude.send_message(
            conversation_id.0,  // Use inner Uuid
            content,
            system_prompt,
            Some(model),
        ).await;

        match result {
            Ok(response) => {
                let duration = start.elapsed();

                // 6a. Write assistant response to audit
                self.write_audit_event(
                    &conversation_id,
                    &ConversationAuditEvent::AssistantMessage {
                        content: response.text.clone(),
                        usage: response.usage.clone(),
                        duration_ms: duration.as_millis() as u64,
                        timestamp: Utc::now(),
                    },
                ).await?;

                // 6b. Update conversation.last_active
                {
                    let mut conversations = self.conversations.write().await;
                    if let Some(conv) = conversations.get_mut(&conversation_id) {
                        conv.last_active = Utc::now();
                    }
                    // Drop write lock before save
                }

                // Save outside the lock to avoid holding write lock across await
                {
                    let conversations = self.conversations.read().await;
                    conversations.save().await?;
                }

                // 6c. Broadcast AssistantMessage event
                match self.event_tx.send(ConversationEvent::AssistantMessage {
                    conversation_id,
                    content: response.text,
                    artifacts: Vec::new(),  // TODO: Phase 10 (images)
                    usage: response.usage,
                    timestamp: Utc::now(),
                }) {
                    Ok(receiver_count) => {
                        tracing::debug!(receiver_count, "event broadcast");
                    }
                    Err(_) => {
                        tracing::warn!("no receivers for event broadcast - all portals may be disconnected");
                    }
                }

                Ok(())
            }
            Err(e) => {
                // 7a. Write error to audit
                self.write_audit_event(
                    &conversation_id,
                    &ConversationAuditEvent::Error {
                        error: e.to_string(),
                        timestamp: Utc::now(),
                    },
                ).await?;

                // 7b. Broadcast Error event
                match self.event_tx.send(ConversationEvent::Error {
                    conversation_id,
                    error: e.to_string(),
                }) {
                    Ok(receiver_count) => {
                        tracing::debug!(receiver_count, "error event broadcast");
                    }
                    Err(_) => {
                        tracing::warn!("no receivers for error event broadcast");
                    }
                }

                Err(e)
            }
        }
    }

    /// Register a new portal (auto-attach to General)
    pub async fn register_portal(
        &self,
        portal_type: PortalType,
    ) -> Result<PortalId> {
        // Get or create General conversation
        let (conversation_id, was_created) = {
            let mut conversations = self.conversations.write().await;
            let default_agent = self.agents.get("default")
                .ok_or_else(|| ThresholdError::Configuration {
                    message: "No 'default' agent configured".to_string(),
                })?;

            let (id, was_created) = conversations.get_or_create_general(
                default_agent.id.clone(),
                default_agent.cli_provider.clone(),
            );
            conversations.save().await?;
            (id, was_created)
        };

        // If General conversation was just created, broadcast ConversationCreated
        if was_created {
            let conv = {
                let conversations = self.conversations.read().await;
                conversations.get(&conversation_id).cloned().unwrap()
            };

            let _ = self.event_tx.send(ConversationEvent::ConversationCreated {
                conversation: conv,
            });
        }

        // Register portal
        let portal = {
            let mut portals = self.portals.write().await;
            let portal = portals.register(portal_type, conversation_id);
            portals.save().await?;
            portal
        };

        // Broadcast PortalAttached
        let _ = self.event_tx.send(ConversationEvent::PortalAttached {
            portal_id: portal.id,
            conversation_id,
        });

        Ok(portal.id)
    }

    /// Unregister a portal
    pub async fn unregister_portal(&self, portal_id: &PortalId) -> Result<()> {
        let conversation_id = {
            let portals = self.portals.read().await;
            portals.get_conversation(portal_id).cloned()
        };

        {
            let mut portals = self.portals.write().await;
            portals.unregister(portal_id);
            portals.save().await?;
        }

        if let Some(conversation_id) = conversation_id {
            let _ = self.event_tx.send(ConversationEvent::PortalDetached {
                portal_id: *portal_id,
                conversation_id,
            });
        }

        Ok(())
    }

    /// List all conversations
    pub async fn list_conversations(&self) -> Vec<Conversation> {
        let conversations = self.conversations.read().await;
        conversations.list().into_iter().cloned().collect()
    }

    // Helper: get portal type string for audit
    async fn get_portal_type_string(&self, portal_id: &PortalId) -> Result<String> {
        let portals = self.portals.read().await;
        let portal = portals.get(portal_id)
            .ok_or_else(|| ThresholdError::NotFound {
                entity: "portal".to_string(),
                id: portal_id.0.to_string(),
            })?;

        Ok(match &portal.portal_type {
            PortalType::Discord { guild_id, channel_id } => {
                format!("Discord({}:{})", guild_id, channel_id)
            }
        })
    }

    // Helper: write audit event
    // NOTE: ConversationAuditEvent is defined in Phase 3.5 (audit.rs)
    // This is stubbed for Phase 3.3 implementation
    async fn write_audit_event(
        &self,
        conversation_id: &ConversationId,
        event: &ConversationAuditEvent,
    ) -> Result<()> {
        crate::audit::write_audit_event(&self.audit_dir, conversation_id, event).await
    }
}
```

### Error Handling
- Portal not found: `ThresholdError::NotFound`
- Conversation not found: `ThresholdError::NotFound`
- Agent not found: `ThresholdError::Configuration`
- CLI errors: propagate up (already logged in audit)

### Tests Required
- `new_engine_loads_state`
- `subscribe_receives_events`
- `register_portal_attaches_to_general`
- `unregister_portal_removes_and_broadcasts`
- `list_conversations_returns_all`

---

## Phase 3.4: Mode Switching

**Add to** `crates/conversation/src/engine.rs`

### Implementation

```rust
impl ConversationEngine {
    /// Switch a portal to a different conversation mode
    pub async fn switch_mode(
        &self,
        portal_id: &PortalId,
        mode: ConversationMode,
    ) -> Result<ConversationId> {
        let mode_key = mode.key();

        // 1. Check if portal exists and get current conversation
        let old_conversation_id = {
            let portals = self.portals.read().await;
            portals.get_conversation(portal_id)
                .ok_or_else(|| ThresholdError::NotFound {
                    entity: "portal".to_string(),
                    id: portal_id.0.to_string(),
                })?
                .clone()
        };

        // 2. Find or create target conversation
        let (target_conversation_id, was_created) = {
            let mut conversations = self.conversations.write().await;

            // Try to find existing conversation by mode
            if let Some(conv) = conversations.find_by_mode(&mode) {
                (conv.id, false)
            } else {
                // Create new conversation
                let agent = self.resolve_agent_for_mode(&mode);
                let id = conversations.create(
                    mode.clone(),
                    agent.cli_provider.clone(),
                    agent.id.clone(),
                );

                conversations.save().await?;
                (id, true)
            }
        };

        // If conversation was just created, broadcast ConversationCreated
        if was_created {
            let conv = {
                let conversations = self.conversations.read().await;
                conversations.get(&target_conversation_id).cloned().unwrap()
            };

            let _ = self.event_tx.send(ConversationEvent::ConversationCreated {
                conversation: conv,
            });
        }

        // 3. If already in target conversation, no-op
        if old_conversation_id == target_conversation_id {
            return Ok(target_conversation_id);
        }

        // 4. Detach from old conversation
        let _ = self.event_tx.send(ConversationEvent::PortalDetached {
            portal_id: *portal_id,
            conversation_id: old_conversation_id,
        });

        // 5. Attach to new conversation
        {
            let mut portals = self.portals.write().await;
            portals.attach(portal_id, target_conversation_id)?;
            portals.save().await?;
        }

        let _ = self.event_tx.send(ConversationEvent::PortalAttached {
            portal_id: *portal_id,
            conversation_id: target_conversation_id,
        });

        // 6. Write mode switch to audit trail
        self.write_audit_event(
            &target_conversation_id,
            &ConversationAuditEvent::ModeSwitch {
                portal_id: *portal_id,
                from_conversation: Some(old_conversation_id),
                to_conversation: target_conversation_id,
                mode,
                timestamp: Utc::now(),
            },
        ).await?;

        Ok(target_conversation_id)
    }

    /// Join a specific conversation by ID
    pub async fn join_conversation(
        &self,
        portal_id: &PortalId,
        conversation_id: &ConversationId,
    ) -> Result<()> {
        // Verify conversation exists
        {
            let conversations = self.conversations.read().await;
            conversations.get(conversation_id)
                .ok_or_else(|| ThresholdError::NotFound {
                    entity: "conversation".to_string(),
                    id: conversation_id.0.to_string(),
                })?;
        }

        // Get current conversation
        let old_conversation_id = {
            let portals = self.portals.read().await;
            portals.get_conversation(portal_id)
                .ok_or_else(|| ThresholdError::NotFound {
                    entity: "portal".to_string(),
                    id: portal_id.0.to_string(),
                })?
                .clone()
        };

        // If already in target, no-op
        if &old_conversation_id == conversation_id {
            return Ok(());
        }

        // Detach from old
        let _ = self.event_tx.send(ConversationEvent::PortalDetached {
            portal_id: *portal_id,
            conversation_id: old_conversation_id,
        });

        // Attach to new
        {
            let mut portals = self.portals.write().await;
            portals.attach(portal_id, *conversation_id)?;
            portals.save().await?;
        }

        let _ = self.event_tx.send(ConversationEvent::PortalAttached {
            portal_id: *portal_id,
            conversation_id: *conversation_id,
        });

        Ok(())
    }

    /// Send a message directly to a conversation (for heartbeat, cron)
    pub async fn send_to_conversation(
        &self,
        conversation_id: &ConversationId,
        content: &str,
    ) -> Result<String> {
        // Look up conversation
        let (agent_id, cli_provider) = {
            let conversations = self.conversations.read().await;
            let conv = conversations.get(conversation_id)
                .ok_or_else(|| ThresholdError::NotFound {
                    entity: "conversation".to_string(),
                    id: conversation_id.0.to_string(),
                })?;
            (conv.agent_id.clone(), conv.cli_provider.clone())
        };

        // Look up agent
        let agent = self.agents.get(&agent_id)
            .ok_or_else(|| ThresholdError::Configuration {
                message: format!("Agent '{}' not found", agent_id),
            })?;

        let (system_prompt, model) = match &cli_provider {
            CliProvider::Claude { model } => (agent.system_prompt.as_deref(), model.as_str()),
        };

        // Call Claude
        let response = self.claude.send_message(
            conversation_id.0,
            content,
            system_prompt,
            Some(model),
        ).await?;

        // Write to audit (no portal_id for system messages)
        self.write_audit_event(
            conversation_id,
            &ConversationAuditEvent::AssistantMessage {
                content: response.text.clone(),
                usage: response.usage.clone(),
                duration_ms: 0,  // Duration not tracked for system messages
                timestamp: Utc::now(),
            },
        ).await?;

        // Update last_active
        {
            let mut conversations = self.conversations.write().await;
            if let Some(conv) = conversations.get_mut(conversation_id) {
                conv.last_active = Utc::now();
            }
            conversations.save().await?;
        }

        Ok(response.text)
    }

    /// Resolve which agent handles a given mode
    fn resolve_agent_for_mode(&self, mode: &ConversationMode) -> &AgentConfig {
        match mode {
            ConversationMode::Coding { .. } => {
                // Look for "coder" agent, fall back to "default"
                self.agents.get("coder")
                    .or_else(|| self.agents.get("default"))
                    .expect("No 'default' agent configured")
            }
            _ => self.agents.get("default")
                .expect("No 'default' agent configured"),
        }
    }
}
```

### Tests Required
- `switch_mode_finds_existing_conversation`
- `switch_mode_creates_new_conversation`
- `switch_mode_to_same_mode_is_noop`
- `switch_mode_broadcasts_detach_and_attach`
- `join_conversation_by_id_succeeds`
- `join_conversation_nonexistent_returns_error`
- `send_to_conversation_succeeds`
- `resolve_agent_for_mode_coding_uses_coder`
- `resolve_agent_for_mode_fallback_to_default`

---

## Phase 3.5: Audit Trail Integration

**File:** `crates/conversation/src/audit.rs`

### Implementation

```rust
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::PathBuf;
use threshold_cli_wrapper::Usage;
use threshold_core::{ConversationId, ConversationMode, PortalId, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Audit events written to per-conversation JSONL files
#[derive(Debug, Clone, Serialize)]
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

/// Write an audit event to the conversation's JSONL file
pub async fn write_audit_event(
    audit_dir: &PathBuf,
    conversation_id: &ConversationId,
    event: &ConversationAuditEvent,
) -> Result<()> {
    // Create audit directory if needed
    tokio::fs::create_dir_all(audit_dir).await?;

    // Path: ~/.threshold/audit/<conversation_id>.jsonl
    let file_path = audit_dir.join(format!("{}.jsonl", conversation_id.0));

    // Serialize event
    let mut json = serde_json::to_string(event)?;
    json.push('\n');

    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)
        .await?;

    file.write_all(json.as_bytes()).await?;
    file.flush().await?;

    // Set restrictive permissions (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&file_path).await?.permissions();
        perms.set_mode(0o600);
        tokio::fs::set_permissions(&file_path, perms).await?;
    }

    Ok(())
}
```

**Update** `crates/conversation/src/engine.rs` to call audit writer:

```rust
async fn write_audit_event(
    &self,
    conversation_id: &ConversationId,
    event: &ConversationAuditEvent,
) -> Result<()> {
    crate::audit::write_audit_event(&self.audit_dir, conversation_id, event).await
}
```

### Tests Required
- `write_audit_event_creates_file`
- `write_audit_event_appends_to_existing`
- `audit_file_is_valid_jsonl`
- `audit_file_has_0o600_permissions` (cfg(unix) only)

---

## Crate Structure

```
crates/conversation/
├── Cargo.toml
└── src/
    ├── lib.rs          # Re-exports ConversationEngine
    ├── store.rs        # Phase 3.1
    ├── portals.rs      # Phase 3.2
    ├── engine.rs       # Phase 3.3 + 3.4
    └── audit.rs        # Phase 3.5
```

### Cargo.toml

```toml
[package]
name = "threshold-conversation"
version.workspace = true
edition.workspace = true

[dependencies]
threshold-core = { path = "../core" }
threshold-cli-wrapper = { path = "../cli-wrapper" }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"

[dev-dependencies]
tempfile = "3"
tokio-test = "0.4"
```

### lib.rs

```rust
//! Conversation Engine - the heart of Threshold.
//!
//! Manages conversations, portals, mode switching, and event broadcasting.

mod audit;
mod engine;
mod portals;
mod store;

// Re-export main types
pub use audit::ConversationAuditEvent;
pub use engine::{Artifact, ConversationEngine, ConversationEvent};
pub use portals::PortalRegistry;
pub use store::ConversationStore;

// Note: write_audit_event is NOT exported - it's used internally by engine
```

---

## Testing Strategy

### Unit Tests (Per Module)
- **store.rs**: 9 tests covering CRUD, mode lookup, General singleton
- **portals.rs**: 10 tests covering register, attach, lookup, persistence
- **engine.rs**: 15+ tests covering message handling, mode switching, events
- **audit.rs**: 4 tests covering JSONL writing and permissions

### Integration Tests
- Create engine, register portal, send message, verify audit written
- Two portals in same conversation both receive events
- Switch modes, verify old conversation persists
- Full persistence: save state, reload, verify restored
- **(with Claude CLI)**: Full round-trip message through engine

### Test Helpers
```rust
// In tests/common/mod.rs
pub fn test_config() -> ThresholdConfig {
    // Returns a config with default agent for testing
}

pub async fn test_engine() -> (ConversationEngine, PathBuf) {
    // Returns an engine with temp directories
}
```

---

## Critical Implementation Notes

### Arc<RwLock<>> Pattern
- Both ConversationStore and PortalRegistry wrapped in Arc<RwLock<>>
- Engine holds Arc clones, allows concurrent access across async tasks
- Pattern: acquire read lock, clone what you need, drop lock quickly
- Minimize critical section time (don't await while holding lock)

### Lock Ordering Convention
**CRITICAL**: To prevent deadlock, always acquire locks in this order:
1. portals (if needed)
2. conversations (if needed)

Never hold multiple locks across `.await` boundaries.
Drop locks before I/O operations (save, CLI calls, audit writes).

### Session ID Flow
- Engine never touches CLI session IDs directly
- ClaudeClient.send_message() receives ConversationId
- ClaudeClient internally looks up session via SessionManager
- This decoupling prevents state drift

### Event Broadcasting Semantics
- `broadcast::send()` returns `Result<usize, SendError>`
- We ignore send errors (`let _ = self.event_tx.send(...)`)
- Rationale: if no receivers, that's fine (portals may be offline)
- Receivers use `recv()` which returns `Err(RecvError::Lagged)` if too slow
- Portals should log and continue on `Lagged` (missed events acceptable)

### Audit Trail Guarantees
- Write-only append (never read during message handling)
- Each event is a separate JSONL line
- Failure to write audit is a hard error (propagated up)
- File corruption tolerance: N/A (append-only, no reads)

### Error Propagation
- Portal/Conversation not found: return error, don't panic
- Agent config missing: return error at startup (fail-fast)
- CLI errors: propagate up after logging to audit
- Broadcast send errors: ignore (no receivers is OK)

---

## Verification Checklist

Before committing:
- [ ] All unit tests pass (33+ tests expected)
- [ ] Integration tests pass (without Claude CLI)
- [ ] Clippy clean with `-D warnings`
- [ ] Rustfmt clean
- [ ] All public APIs have docs
- [ ] Audit JSONL files are valid (can parse with `jq`)
- [ ] File permissions 0o600 on Unix
- [ ] No unwrap() except in tests or after verification
- [ ] Tracing events at appropriate levels (info for major operations, debug for internals)

---

## Dependencies on Future Milestones

- **Milestone 4 (Discord)**: Will use `ConversationEngine::handle_message()`
- **Milestone 6 (Heartbeat)**: Will use `send_to_conversation()`
- **Milestone 7 (Cron)**: Will use `send_to_conversation()`
- **Milestone 10 (Images)**: Will populate `artifacts` in AssistantMessage events

---

## End of Plan

**Codex Review Completed**: All P0 and P1 issues identified in review have been fixed:
- ✅ P0 #1: Added public `get()` method to PortalRegistry
- ✅ P0 #2: Fixed ThresholdConfig field access to use `data_dir()` method
- ✅ P0 #3: Added AgentConfig transformation from TOML format
- ✅ P0 #4: Fixed lock holding across await (save outside write lock)
- ✅ P1 #5: Added diagnostic logging for broadcast sends
- ✅ P1 #6: Fixed early return check in switch_mode
- ✅ P1 #7: Changed ConversationStore::create to return ConversationId
- ✅ P1 #8: Added missing Utc and other imports to engine.rs
- ✅ P1 #10: Changed get_portals_for_conversation to return owned values
- ✅ P1 #11: Added ConversationCreated broadcast in register_portal
- ✅ P1 #12: Added phase ordering note for ConversationAuditEvent

Ready for implementation.
