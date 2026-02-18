//! Conversation Engine - the main orchestrator for Threshold.
//!
//! Handles message routing, event broadcasting, and mode switching.

use crate::audit::ConversationAuditEvent;
use crate::portals::PortalRegistry;
use crate::store::ConversationStore;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use threshold_cli_wrapper::ClaudeClient;
use threshold_cli_wrapper::response::Usage;
use threshold_core::config::{AgentConfigToml, ThresholdConfig};
use threshold_core::{
    AgentConfig, CliProvider, Conversation, ConversationId, ConversationMode, PortalId, PortalType,
    Result, ThresholdError, ToolProfile,
};
use tokio::sync::{RwLock, broadcast};

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

// LOCK ORDERING CONVENTION:
// To prevent deadlock, always acquire locks in this order:
// 1. portals (if needed)
// 2. conversations (if needed)
//
// Never hold multiple locks across .await boundaries.
// Drop locks before I/O operations (save, CLI calls, audit writes).

pub struct ConversationEngine {
    conversations: Arc<RwLock<ConversationStore>>,
    portals: Arc<RwLock<PortalRegistry>>,
    claude: Arc<ClaudeClient>,
    agents: HashMap<String, AgentConfig>,
    event_tx: broadcast::Sender<ConversationEvent>,
    audit_dir: PathBuf,
    data_dir: PathBuf,
    /// Tool-availability section to prepend to system prompts (from build_tool_prompt).
    tool_prompt: Option<String>,
}

impl ConversationEngine {
    /// Create a new engine.
    ///
    /// `tool_prompt` is an optional tool-availability section (from `build_tool_prompt()`)
    /// that gets prepended to each agent's system prompt when launching Claude sessions.
    pub async fn new(
        config: &ThresholdConfig,
        claude: Arc<ClaudeClient>,
        tool_prompt: Option<String>,
    ) -> Result<Self> {
        // Resolve data directory from config
        let data_dir = config.data_dir()?;
        let audit_dir = data_dir.join("audit");

        // Load state from disk
        let conversations = Arc::new(RwLock::new(ConversationStore::load(&data_dir).await?));
        let portals = Arc::new(RwLock::new(PortalRegistry::load(&data_dir).await?));

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
            data_dir,
            tool_prompt,
        })
    }

    /// Transform AgentConfigToml to AgentConfig
    fn transform_agent_config(toml: &AgentConfigToml) -> Result<AgentConfig> {
        let cli_provider = match toml.cli_provider.as_str() {
            "claude" => CliProvider::Claude {
                model: toml.model.clone().unwrap_or_else(|| "sonnet".to_string()),
            },
            other => {
                return Err(ThresholdError::Config(format!(
                    "Unknown CLI provider: {}",
                    other
                )));
            }
        };

        let tool_profile = match toml.tools.as_deref() {
            Some("minimal") => ToolProfile::Minimal,
            Some("standard") | Some("coding") => ToolProfile::Standard,
            Some("full") | None => ToolProfile::Full,
            Some(other) => {
                return Err(ThresholdError::Config(format!(
                    "Unknown tool profile: {}",
                    other
                )));
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

    /// Build the effective system prompt for an agent, prepending tool instructions
    /// if a tool_prompt was provided at engine construction time.
    fn effective_system_prompt(&self, agent: &AgentConfig) -> Option<String> {
        match (&self.tool_prompt, &agent.system_prompt) {
            (Some(tool), Some(agent_prompt)) => Some(format!("{}\n\n{}", tool, agent_prompt)),
            (Some(tool), None) => Some(tool.clone()),
            (None, Some(agent_prompt)) => Some(agent_prompt.clone()),
            (None, None) => None,
        }
    }

    /// Build the memory prompt supplement for a conversation.
    ///
    /// Reads the conversation's `memory.md` file and formats it as a system
    /// prompt supplement. Returns `None` if the file doesn't exist or is empty.
    /// Truncates at 4KB with a notice if the file is larger.
    fn build_memory_prompt(
        conversation_id: &ConversationId,
        data_dir: &Path,
    ) -> Option<String> {
        let memory_path = data_dir
            .join("conversations")
            .join(conversation_id.0.to_string())
            .join("memory.md");

        let memory_contents = match std::fs::read_to_string(&memory_path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(
                    conversation_id = %conversation_id.0,
                    error = %e,
                    path = %memory_path.display(),
                    "failed to read memory.md — memory will not be injected"
                );
                return None;
            }
        };
        if memory_contents.is_empty() {
            return None;
        }

        // Truncate if over 4KB (UTF-8 safe — find nearest char boundary)
        let (contents, truncated) = if memory_contents.len() > 4096 {
            let boundary = memory_contents.floor_char_boundary(4096);
            (&memory_contents[..boundary], true)
        } else {
            (memory_contents.as_str(), false)
        };

        let mut prompt = format!(
            "### Conversation Memory\n\
             Your persistent memory file is at: {path}\n\
             This file persists across session resets. Update it when you make important \
             decisions, complete milestones, or need to preserve information across sessions.\n\n\
             Current contents:\n{contents}",
            path = memory_path.display(),
            contents = contents,
        );

        if truncated {
            prompt.push_str(&format!(
                "\n\n[Memory truncated — full file at {}. Read it directly for complete context.]",
                memory_path.display()
            ));
        }

        Some(prompt)
    }

    /// Combine a base system prompt with a memory prompt supplement.
    fn combine_prompts(base: Option<&str>, memory: Option<&str>) -> Option<String> {
        match (base, memory) {
            (Some(b), Some(m)) => Some(format!("{}\n\n{}", b, m)),
            (Some(b), None) => Some(b.to_string()),
            (None, Some(m)) => Some(m.to_string()),
            (None, None) => None,
        }
    }

    /// Subscribe to conversation events
    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent> {
        self.event_tx.subscribe()
    }

    /// Handle an incoming user message
    pub async fn handle_message(&self, portal_id: &PortalId, content: &str) -> Result<()> {
        // 1. Look up portal → get conversation_id
        let conversation_id = {
            let portals = self.portals.read().await;
            *portals
                .get_conversation(portal_id)
                .ok_or(ThresholdError::PortalNotFound { id: portal_id.0 })?
        };

        // 2. Look up conversation → get agent_id, cli_provider
        let (agent_id, cli_provider) = {
            let conversations = self.conversations.read().await;
            let conv = conversations.get(&conversation_id).ok_or(
                ThresholdError::ConversationNotFound {
                    id: conversation_id.0,
                },
            )?;
            (conv.agent_id.clone(), conv.cli_provider.clone())
        };

        // 3. Look up agent config → get system_prompt, model
        let agent = self.agents.get(&agent_id).ok_or_else(|| {
            ThresholdError::Config(format!("Agent '{}' not found in configuration", agent_id))
        })?;

        let effective_prompt = self.effective_system_prompt(agent);
        let memory_prompt = Self::build_memory_prompt(&conversation_id, &self.data_dir);
        let combined_prompt = Self::combine_prompts(
            effective_prompt.as_deref(),
            memory_prompt.as_deref(),
        );
        let (system_prompt, model) = match &cli_provider {
            CliProvider::Claude { model } => (combined_prompt.as_deref(), model.as_str()),
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
        )
        .await?;

        // 5. Call Claude CLI
        let start = std::time::Instant::now();
        let result = self
            .claude
            .send_message(
                conversation_id.0, // Use inner Uuid
                content,
                system_prompt,
                Some(model),
            )
            .await;

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
                )
                .await?;

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
                    artifacts: Vec::new(), // TODO: Phase 10 (images)
                    usage: response.usage,
                    timestamp: Utc::now(),
                }) {
                    Ok(receiver_count) => {
                        tracing::debug!(receiver_count, "event broadcast");
                    }
                    Err(_) => {
                        tracing::warn!(
                            "no receivers for event broadcast - all portals may be disconnected"
                        );
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
                )
                .await?;

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
    pub async fn register_portal(&self, portal_type: PortalType) -> Result<PortalId> {
        // Get or create General conversation
        let (conversation_id, was_created) = {
            let mut conversations = self.conversations.write().await;
            let default_agent = self.agents.get("default").ok_or_else(|| {
                ThresholdError::Config("No 'default' agent configured".to_string())
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

            let _ = self
                .event_tx
                .send(ConversationEvent::ConversationCreated { conversation: conv });
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

    /// Save all state to disk (conversations and portals)
    pub async fn save_state(&self) -> Result<()> {
        // Save conversations
        {
            let conversations = self.conversations.read().await;
            conversations.save().await?;
        }

        // Save portals
        {
            let portals = self.portals.read().await;
            portals.save().await?;
        }

        Ok(())
    }

    /// Get access to portals (for portal listener management)
    pub fn portals(&self) -> Arc<RwLock<PortalRegistry>> {
        self.portals.clone()
    }

    /// Switch a portal to a different conversation mode
    pub async fn switch_mode(
        &self,
        portal_id: &PortalId,
        mode: ConversationMode,
    ) -> Result<ConversationId> {
        // 1. Check if portal exists and get current conversation
        let old_conversation_id = {
            let portals = self.portals.read().await;
            *portals
                .get_conversation(portal_id)
                .ok_or(ThresholdError::PortalNotFound { id: portal_id.0 })?
        };

        // 2. Find or create target conversation
        let (target_conversation_id, was_created) = {
            let mut conversations = self.conversations.write().await;

            // Try to find existing conversation by mode
            if let Some(conv) = conversations.find_by_mode(&mode) {
                let id = conv.id;
                // Backfill directory for pre-M12 conversations
                conversations.ensure_conversation_dir(&id, &mode);
                (id, false)
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

            let _ = self
                .event_tx
                .send(ConversationEvent::ConversationCreated { conversation: conv });
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
        )
        .await?;

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
            conversations
                .get(conversation_id)
                .ok_or(ThresholdError::ConversationNotFound {
                    id: conversation_id.0,
                })?;
        }

        // Get current conversation
        let old_conversation_id = {
            let portals = self.portals.read().await;
            *portals
                .get_conversation(portal_id)
                .ok_or(ThresholdError::PortalNotFound { id: portal_id.0 })?
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
            let conv =
                conversations
                    .get(conversation_id)
                    .ok_or(ThresholdError::ConversationNotFound {
                        id: conversation_id.0,
                    })?;
            (conv.agent_id.clone(), conv.cli_provider.clone())
        };

        // Look up agent
        let agent = self
            .agents
            .get(&agent_id)
            .ok_or_else(|| ThresholdError::Config(format!("Agent '{}' not found", agent_id)))?;

        let effective_prompt = self.effective_system_prompt(agent);
        let memory_prompt = Self::build_memory_prompt(conversation_id, &self.data_dir);
        let combined_prompt = Self::combine_prompts(
            effective_prompt.as_deref(),
            memory_prompt.as_deref(),
        );
        let (system_prompt, model) = match &cli_provider {
            CliProvider::Claude { model } => (combined_prompt.as_deref(), model.as_str()),
        };

        // Call Claude
        let response = self
            .claude
            .send_message(conversation_id.0, content, system_prompt, Some(model))
            .await?;

        // Write to audit (no portal_id for system messages)
        self.write_audit_event(
            conversation_id,
            &ConversationAuditEvent::AssistantMessage {
                content: response.text.clone(),
                usage: response.usage.clone(),
                duration_ms: 0, // Duration not tracked for system messages
                timestamp: Utc::now(),
            },
        )
        .await?;

        // Broadcast AssistantMessage event so portal listeners receive it
        // This is critical for heartbeat/cron messages to reach portals (Milestone 7)
        match self.event_tx.send(ConversationEvent::AssistantMessage {
            conversation_id: *conversation_id,
            content: response.text.clone(),
            artifacts: Vec::new(),
            usage: response.usage.clone(),
            timestamp: Utc::now(),
        }) {
            Ok(receiver_count) => {
                tracing::debug!(
                    receiver_count,
                    "Broadcasted system message to {} receivers",
                    receiver_count
                );
            }
            Err(_) => {
                tracing::warn!("No receivers for system message (all portals may be disconnected)");
            }
        }

        // Update last_active
        {
            let mut conversations = self.conversations.write().await;
            if let Some(conv) = conversations.get_mut(conversation_id) {
                conv.last_active = Utc::now();
            }
        }

        {
            let conversations = self.conversations.read().await;
            conversations.save().await?;
        }

        Ok(response.text)
    }

    /// Resolve which agent handles a given mode
    fn resolve_agent_for_mode(&self, mode: &ConversationMode) -> &AgentConfig {
        match mode {
            ConversationMode::Coding { .. } => {
                // Look for "coder" agent, fall back to "default"
                self.agents
                    .get("coder")
                    .or_else(|| self.agents.get("default"))
                    .expect("No 'default' agent configured")
            }
            _ => self
                .agents
                .get("default")
                .expect("No 'default' agent configured"),
        }
    }

    // Helper: get portal type string for audit
    async fn get_portal_type_string(&self, portal_id: &PortalId) -> Result<String> {
        let portals = self.portals.read().await;
        let portal = portals
            .get(portal_id)
            .ok_or(ThresholdError::PortalNotFound { id: portal_id.0 })?;

        match &portal.portal_type {
            PortalType::Discord {
                guild_id,
                channel_id,
            } => Ok(format!("Discord({}:{})", guild_id, channel_id)),
        }
    }

    // Helper: write audit event
    async fn write_audit_event(
        &self,
        conversation_id: &ConversationId,
        event: &ConversationAuditEvent,
    ) -> Result<()> {
        crate::audit::write_audit_event(&self.audit_dir, conversation_id, event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use threshold_core::CliProvider;
    use threshold_core::config::AgentConfigToml;

    fn test_config() -> ThresholdConfig {
        use threshold_core::config::{ClaudeCliConfig, CliConfig, ToolsConfig};

        ThresholdConfig {
            data_dir: Some(tempdir().unwrap().path().to_path_buf()),
            log_level: None,
            cli: CliConfig {
                claude: ClaudeCliConfig {
                    command: Some("claude".to_string()),
                    model: Some("sonnet".to_string()),
                    timeout_seconds: None,
                    skip_permissions: Some(false),
                    extra_flags: vec![],
                },
            },
            discord: None,
            agents: vec![
                AgentConfigToml {
                    id: "default".to_string(),
                    name: "Default Agent".to_string(),
                    cli_provider: "claude".to_string(),
                    model: Some("sonnet".to_string()),
                    system_prompt: Some("You are helpful.".to_string()),
                    system_prompt_file: None,
                    tools: Some("full".to_string()),
                },
                AgentConfigToml {
                    id: "coder".to_string(),
                    name: "Coding Agent".to_string(),
                    cli_provider: "claude".to_string(),
                    model: Some("opus".to_string()),
                    system_prompt: Some("You write code.".to_string()),
                    system_prompt_file: None,
                    tools: Some("standard".to_string()),
                },
            ],
            tools: ToolsConfig::default(),
            heartbeat: None,
            scheduler: None,
        }
    }

    #[tokio::test]
    async fn transform_agent_config_claude() {
        let toml = AgentConfigToml {
            id: "test".to_string(),
            name: "Test".to_string(),
            cli_provider: "claude".to_string(),
            model: Some("sonnet".to_string()),
            system_prompt: Some("test".to_string()),
            system_prompt_file: None,
            tools: Some("full".to_string()),
        };

        let agent = ConversationEngine::transform_agent_config(&toml).unwrap();

        assert_eq!(agent.id, "test");
        assert!(matches!(
            agent.cli_provider,
            CliProvider::Claude { model } if model == "sonnet"
        ));
        assert_eq!(agent.tool_profile, ToolProfile::Full);
    }

    #[tokio::test]
    async fn transform_agent_config_unknown_provider_errors() {
        let toml = AgentConfigToml {
            id: "test".to_string(),
            name: "Test".to_string(),
            cli_provider: "unknown".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_file: None,
            tools: None,
        };

        let result = ConversationEngine::transform_agent_config(&toml);

        assert!(result.is_err());
        match result.unwrap_err() {
            ThresholdError::Config(message) => {
                assert!(message.contains("Unknown CLI provider"));
            }
            _ => panic!("expected Config error"),
        }
    }

    #[tokio::test]
    async fn resolve_agent_for_mode_coding_uses_coder() {
        let config = test_config();
        // Create a dummy ClaudeClient (won't be used in this test)
        let claude = Arc::new(
            threshold_cli_wrapper::ClaudeClient::new(
                "claude".to_string(),
                tempdir().unwrap().path().to_path_buf(),
                false,
            )
            .await
            .unwrap(),
        );

        let engine = ConversationEngine::new(&config, claude, None).await.unwrap();

        let agent = engine.resolve_agent_for_mode(&ConversationMode::Coding {
            project: "test".to_string(),
        });

        assert_eq!(agent.id, "coder");
    }

    #[tokio::test]
    async fn resolve_agent_for_mode_general_uses_default() {
        let config = test_config();
        let claude = Arc::new(
            threshold_cli_wrapper::ClaudeClient::new(
                "claude".to_string(),
                tempdir().unwrap().path().to_path_buf(),
                false,
            )
            .await
            .unwrap(),
        );

        let engine = ConversationEngine::new(&config, claude, None).await.unwrap();

        let agent = engine.resolve_agent_for_mode(&ConversationMode::General);

        assert_eq!(agent.id, "default");
    }

    #[tokio::test]
    async fn build_memory_prompt_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        let conv_id = ConversationId::new();

        let result = ConversationEngine::build_memory_prompt(&conv_id, dir.path());
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn build_memory_prompt_returns_none_for_empty_file() {
        let dir = tempdir().unwrap();
        let conv_id = ConversationId::new();

        let conv_dir = dir
            .path()
            .join("conversations")
            .join(conv_id.0.to_string());
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::write(conv_dir.join("memory.md"), "").unwrap();

        let result = ConversationEngine::build_memory_prompt(&conv_id, dir.path());
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn build_memory_prompt_includes_content() {
        let dir = tempdir().unwrap();
        let conv_id = ConversationId::new();

        let conv_dir = dir
            .path()
            .join("conversations")
            .join(conv_id.0.to_string());
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::write(conv_dir.join("memory.md"), "# My Memory\nSome notes here").unwrap();

        let result = ConversationEngine::build_memory_prompt(&conv_id, dir.path()).unwrap();
        assert!(result.contains("### Conversation Memory"));
        assert!(result.contains("# My Memory"));
        assert!(result.contains("Some notes here"));
        assert!(result.contains("memory.md"));
        // Should NOT contain truncation notice
        assert!(!result.contains("Memory truncated"));
    }

    #[tokio::test]
    async fn build_memory_prompt_truncates_at_4kb() {
        let dir = tempdir().unwrap();
        let conv_id = ConversationId::new();

        let conv_dir = dir
            .path()
            .join("conversations")
            .join(conv_id.0.to_string());
        std::fs::create_dir_all(&conv_dir).unwrap();

        // Write content larger than 4KB
        let large_content = "x".repeat(5000);
        std::fs::write(conv_dir.join("memory.md"), &large_content).unwrap();

        let result = ConversationEngine::build_memory_prompt(&conv_id, dir.path()).unwrap();
        // Should contain truncation notice
        assert!(result.contains("Memory truncated"));
        assert!(result.contains("Read it directly for complete context"));
        // Should NOT contain all 5000 bytes of content
        assert!(result.len() < 5000 + 500); // prompt overhead
    }

    #[tokio::test]
    async fn build_memory_prompt_truncation_utf8_safe() {
        let dir = tempdir().unwrap();
        let conv_id = ConversationId::new();

        let conv_dir = dir
            .path()
            .join("conversations")
            .join(conv_id.0.to_string());
        std::fs::create_dir_all(&conv_dir).unwrap();

        // Create content where byte 4096 falls in the middle of a multibyte char.
        // 'a' is 1 byte, '\u{1F600}' is 4 bytes.
        // 4095 ASCII bytes + 1 emoji (4 bytes) = 4099 bytes total.
        // Byte 4096 falls inside the emoji (byte 2 of 4), so naive slicing
        // at [..4096] would panic. floor_char_boundary should back up to 4095.
        let mut content = "a".repeat(4095);
        content.push('\u{1F600}'); // 4-byte emoji
        assert_eq!(content.len(), 4099);
        std::fs::write(conv_dir.join("memory.md"), &content).unwrap();

        // This should NOT panic — floor_char_boundary handles the mid-char cut
        let result = ConversationEngine::build_memory_prompt(&conv_id, dir.path()).unwrap();
        assert!(result.contains("Memory truncated"));
        // The truncated content should contain 4095 'a's but NOT the emoji
        assert!(result.contains(&"a".repeat(4095)));
        assert!(!result.contains("\u{1F600}"));
    }

    #[test]
    fn combine_prompts_both_present() {
        let result = ConversationEngine::combine_prompts(Some("base"), Some("memory"));
        assert_eq!(result, Some("base\n\nmemory".to_string()));
    }

    #[test]
    fn combine_prompts_base_only() {
        let result = ConversationEngine::combine_prompts(Some("base"), None);
        assert_eq!(result, Some("base".to_string()));
    }

    #[test]
    fn combine_prompts_memory_only() {
        let result = ConversationEngine::combine_prompts(None, Some("memory"));
        assert_eq!(result, Some("memory".to_string()));
    }

    #[test]
    fn combine_prompts_neither() {
        let result = ConversationEngine::combine_prompts(None, None);
        assert!(result.is_none());
    }
}
