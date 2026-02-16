//! Conversation Engine - the main orchestrator for Threshold.
//!
//! Handles message routing, event broadcasting, and mode switching.

use crate::audit::ConversationAuditEvent;
use crate::portals::PortalRegistry;
use crate::store::ConversationStore;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
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
}

impl ConversationEngine {
    /// Create a new engine
    pub async fn new(config: &ThresholdConfig, claude: Arc<ClaudeClient>) -> Result<Self> {
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
            Some("coding") => ToolProfile::Coding,
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

        let (system_prompt, model) = match &cli_provider {
            CliProvider::Claude { model } => (agent.system_prompt.as_deref(), model.as_str()),
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
                    tools: Some("coding".to_string()),
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

        let engine = ConversationEngine::new(&config, claude).await.unwrap();

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

        let engine = ConversationEngine::new(&config, claude).await.unwrap();

        let agent = engine.resolve_agent_for_mode(&ConversationMode::General);

        assert_eq!(agent.id, "default");
    }
}
