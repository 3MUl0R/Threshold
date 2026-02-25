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
use threshold_cli_wrapper::HaikuClient;
use threshold_cli_wrapper::response::Usage;
use threshold_core::config::{AgentConfigToml, ThresholdConfig};
use threshold_core::{
    ActiveConversations, AgentConfig, CliProvider, Conversation, ConversationId, ConversationMode,
    PortalId, PortalType, Result, RunId, ThresholdError, ToolProfile,
};
use tokio::sync::{RwLock, broadcast};

/// Events emitted by the engine
#[derive(Debug, Clone)]
pub enum ConversationEvent {
    AssistantMessage {
        conversation_id: ConversationId,
        run_id: RunId,
        content: String,
        artifacts: Vec<Artifact>,
        usage: Option<Usage>,
        timestamp: DateTime<Utc>,
    },
    Error {
        conversation_id: ConversationId,
        run_id: Option<RunId>,
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
    ConversationDeleted {
        conversation_id: ConversationId,
    },
    /// Immediate acknowledgment (Phase 14C).
    Acknowledgment {
        conversation_id: ConversationId,
        run_id: RunId,
        content: String,
    },
    /// Periodic status update during processing (Phase 14D).
    StatusUpdate {
        conversation_id: ConversationId,
        run_id: RunId,
        summary: String,
        elapsed_secs: u64,
    },
    /// Task was aborted by user.
    Aborted {
        conversation_id: ConversationId,
        run_id: RunId,
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
    /// Shared tracker of conversations with active CLI invocations.
    active_conversations: Arc<ActiveConversations>,
    /// Optional Haiku client for acknowledgments and/or status updates.
    haiku: Option<Arc<HaikuClient>>,
    /// Whether to send Haiku acknowledgment messages.
    ack_enabled: bool,
    /// Interval in seconds for live status updates (0 = disabled).
    status_interval_secs: u64,
}

impl ConversationEngine {
    /// Create a new engine.
    ///
    /// `tool_prompt` is an optional tool-availability section (from `build_tool_prompt()`)
    /// that gets prepended to each agent's system prompt when launching Claude sessions.
    ///
    /// `active_conversations` is an optional shared tracker. When `None`, a private
    /// instance is created (sufficient for tests where cross-component tracking isn't needed).
    pub async fn new(
        config: &ThresholdConfig,
        claude: Arc<ClaudeClient>,
        tool_prompt: Option<String>,
        active_conversations: Option<Arc<ActiveConversations>>,
        haiku: Option<Arc<HaikuClient>>,
        ack_enabled: bool,
        status_interval_secs: u64,
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
            active_conversations: active_conversations
                .unwrap_or_else(|| Arc::new(ActiveConversations::new())),
            haiku,
            ack_enabled,
            status_interval_secs,
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

    /// Handle an incoming user message.
    ///
    /// Uses streaming mode to read Claude CLI output line-by-line. This avoids
    /// the pipe buffer deadlock for long responses and enables live progress
    /// tracking in future phases.
    pub async fn handle_message(&self, portal_id: &PortalId, content: &str) -> Result<()> {
        use threshold_cli_wrapper::stream::StreamEvent;

        let run_id = RunId::new();

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

        // 5. Spawn Haiku acknowledgment (independent, no lock needed).
        //    Capped at 5 seconds — if Haiku hasn't responded by then the ack
        //    would arrive too late to be useful and is silently dropped.
        if self.ack_enabled {
            if let Some(haiku) = &self.haiku {
                let haiku = haiku.clone();
                let event_tx = self.event_tx.clone();
                let ack_conversation_id = conversation_id;
                let ack_run_id = run_id;
                let ack_audit_dir = self.audit_dir.clone();
                let ack_prompt = format!(
                    "You are generating a brief acknowledgment for a Discord chat message. \
                     The user just sent a message to an AI assistant. Generate a short (1-2 sentence) \
                     acknowledgment that shows you understand what they're asking and sets expectations. \
                     Be conversational and natural — like a teammate saying \"on it.\" \
                     Do NOT actually do the work. Just acknowledge.\n\nUser message: {}",
                    content
                );
                tokio::spawn(async move {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        haiku.generate(&ack_prompt, None),
                    )
                    .await;
                    match result {
                        Ok(Ok(ack_text)) => {
                            tracing::info!(
                                conversation_id = %ack_conversation_id.0,
                                run_id = %ack_run_id,
                                "Haiku acknowledgment sent"
                            );
                            if let Err(e) = crate::audit::write_audit_event(
                                &ack_audit_dir,
                                &ack_conversation_id,
                                &ConversationAuditEvent::Acknowledgment {
                                    run_id: ack_run_id.0.to_string(),
                                    content: ack_text.clone(),
                                    timestamp: Utc::now(),
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "Failed to write ack audit event");
                            }
                            let _ = event_tx.send(ConversationEvent::Acknowledgment {
                                conversation_id: ack_conversation_id,
                                run_id: ack_run_id,
                                content: ack_text,
                            });
                        }
                        Ok(Err(e)) => {
                            tracing::info!(
                                error = %e,
                                "Haiku acknowledgment failed (non-fatal)"
                            );
                        }
                        Err(_) => {
                            tracing::info!(
                                "Haiku acknowledgment timed out after 30s (dropped)"
                            );
                        }
                    }
                });
            }
        }

        // 6. Acquire per-conversation lock (held for entire stream lifetime).
        //    Use try_lock first to detect queuing and notify user.
        let status_interval = self.status_interval_secs;
        let _guard = match self.claude.locks().try_lock(conversation_id.0).await {
            Some(guard) => guard,
            None => {
                // Another message is processing — show queued status
                tracing::info!(
                    conversation_id = %conversation_id.0,
                    run_id = %run_id,
                    "Message queued — waiting for previous message to complete"
                );
                if status_interval > 0 {
                    let _ = self.event_tx.send(ConversationEvent::StatusUpdate {
                        conversation_id,
                        run_id,
                        summary: "Queued \u{2014} waiting for previous message to complete\u{2026}"
                            .to_string(),
                        elapsed_secs: 0,
                    });
                }
                self.claude.locks().lock(conversation_id.0).await
            }
        };

        // 7. Start streaming CLI invocation
        self.active_conversations.insert(conversation_id).await;
        let start = std::time::Instant::now();

        // Prefix the message with a timestamp so the agent knows the current
        // date/time. This prevents confusion when conversations resume after
        // hours or days. Cost: ~10 tokens per message — negligible.
        let now = chrono::Local::now();
        let timestamped_content = format!(
            "[{} {}]\n{}",
            now.format("%Y-%m-%d %H:%M"),
            now.format("%Z"),
            content,
        );

        let stream_result = self
            .claude
            .send_message_streaming(
                conversation_id.0,
                run_id,
                &timestamped_content,
                system_prompt,
                Some(model),
            )
            .await;

        let mut stream_handle = match stream_result {
            Ok(handle) => handle,
            Err(e) => {
                self.active_conversations.remove(&conversation_id).await;
                self.claude.tracker().deregister(&run_id).await;
                self.broadcast_error(&conversation_id, Some(run_id), &e).await;
                return Err(e);
            }
        };

        // 8. Consume stream events, build final response.
        //    Periodically summarize activity via Haiku and broadcast StatusUpdate.
        let mut final_text = String::new();
        let mut final_session_id = None;
        let mut final_usage = None;
        let mut saw_result = false;

        // Status update tracking
        let mut event_log: Vec<String> = Vec::new();
        let mut last_status = std::time::Instant::now();
        let status_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Track whether any "interesting" events (tool use, tool result, errors)
        // occurred since the last status update. Pure text-writing phases produce
        // repetitive "Writing code..." summaries — skip Haiku calls for those.
        let mut has_notable_events = false;

        while let Some(event) = stream_handle.event_rx.recv().await {
            match event {
                StreamEvent::Result {
                    text,
                    session_id,
                    usage,
                } => {
                    final_text = text;
                    final_session_id = session_id;
                    final_usage = usage;
                    saw_result = true;
                    break;
                }
                StreamEvent::Error { message } => {
                    // CLI error — treat as failure
                    self.active_conversations.remove(&conversation_id).await;
                    self.claude.tracker().deregister(&run_id).await;
                    let err = ThresholdError::CliError {
                        provider: "claude".into(),
                        code: -1,
                        stderr: message,
                    };
                    self.broadcast_error(&conversation_id, Some(run_id), &err)
                        .await;
                    return Err(err);
                }
                StreamEvent::TextDelta { text } => {
                    final_text.push_str(&text);
                    // TextDelta events are not added to event_log — status updates
                    // only fire on tool-use events (has_notable_events guard), so
                    // accumulating text previews would just grow memory unboundedly
                    // during long writing phases without ever being consumed.
                }
                StreamEvent::ToolUse { tool_name, .. } => {
                    tracing::info!(
                        conversation_id = %conversation_id.0,
                        run_id = %run_id,
                        tool = %tool_name,
                        "Tool use detected"
                    );
                    if status_interval > 0 {
                        event_log.push(format!("Using tool: {}", tool_name));
                        has_notable_events = true;
                    }
                }
                StreamEvent::ToolResult { tool_name, is_error } => {
                    if status_interval > 0 {
                        if is_error {
                            event_log.push(format!("Tool {} returned error", tool_name));
                        } else {
                            event_log.push(format!("Tool {} completed", tool_name));
                        }
                        has_notable_events = true;
                    }
                }
            }

            // Emit periodic status update via Haiku.
            // Skip if a previous status call is still in-flight to prevent
            // stale/out-of-order edits when Haiku is slow.
            if status_interval > 0
                && has_notable_events
                && last_status.elapsed()
                    >= std::time::Duration::from_secs(status_interval)
                && !status_in_flight.load(std::sync::atomic::Ordering::Relaxed)
            {
                if let Some(haiku) = &self.haiku {
                    status_in_flight
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    let elapsed_secs = start.elapsed().as_secs();
                    let summary_prompt = format!(
                        "Summarize the AI assistant's live activity for a short status line.\n\
                         Rules:\n\
                         - Output ONLY the status text, nothing else\n\
                         - No quotes, no markdown, no bold, no code fences, no preamble\n\
                         - Under 80 characters, plain text, present tense\n\
                         - Focus on the most recent activity\n\
                         Examples of correct output:\n\
                         Reading project structure — 12 files examined\n\
                         Writing implementation for user auth module\n\
                         Running test suite — 3 of 8 passing so far\n\n\
                         Recent events:\n{}",
                        event_log.join("\n")
                    );
                    // Fire and forget — don't block the stream loop
                    let haiku = haiku.clone();
                    let event_tx = self.event_tx.clone();
                    let status_cid = conversation_id;
                    let status_rid = run_id;
                    let in_flight = status_in_flight.clone();
                    let status_audit_dir = self.audit_dir.clone();
                    tokio::spawn(async move {
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            haiku.generate(&summary_prompt, None),
                        )
                        .await;
                        if let Ok(Ok(raw_summary)) = result {
                            let summary = sanitize_status_summary(&raw_summary);
                            tracing::info!(
                                conversation_id = %status_cid.0,
                                run_id = %status_rid,
                                elapsed_secs,
                                summary = %summary,
                                "Status update sent"
                            );
                            if let Err(e) = crate::audit::write_audit_event(
                                &status_audit_dir,
                                &status_cid,
                                &ConversationAuditEvent::StatusUpdate {
                                    run_id: status_rid.0.to_string(),
                                    summary: summary.clone(),
                                    elapsed_secs,
                                    timestamp: Utc::now(),
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "Failed to write status audit event");
                            }
                            let _ = event_tx.send(ConversationEvent::StatusUpdate {
                                conversation_id: status_cid,
                                run_id: status_rid,
                                summary,
                                elapsed_secs,
                            });
                        }
                        in_flight.store(false, std::sync::atomic::Ordering::Relaxed);
                    });
                }
                event_log.clear();
                has_notable_events = false;
                last_status = std::time::Instant::now();
            }
        }

        // Check abort flag from the streaming reader task (explicit signal,
        // not string matching)
        let was_aborted = stream_handle
            .was_aborted
            .load(std::sync::atomic::Ordering::Relaxed);

        self.active_conversations.remove(&conversation_id).await;
        self.claude.tracker().deregister(&run_id).await;

        // 9. Handle abort
        if was_aborted {
            self.write_audit_event(
                &conversation_id,
                &ConversationAuditEvent::Error {
                    error: "Task aborted by user".to_string(),
                    timestamp: Utc::now(),
                },
            )
            .await?;

            let _ = self.event_tx.send(ConversationEvent::Aborted {
                conversation_id,
                run_id,
            });

            return Err(ThresholdError::Aborted);
        }

        // 10. Handle stream close without Result event (process crashed)
        if !saw_result {
            let err = ThresholdError::CliError {
                provider: "claude".into(),
                code: -1,
                stderr: "CLI process exited without producing a result".into(),
            };
            self.broadcast_error(&conversation_id, Some(run_id), &err)
                .await;
            return Err(err);
        }

        let duration = start.elapsed();

        // 11. Update session ID if returned
        if let Some(session_id) = &final_session_id {
            self.claude
                .sessions()
                .set(conversation_id.0, session_id.clone())
                .await?;
        }

        // 12. Write assistant response to audit
        self.write_audit_event(
            &conversation_id,
            &ConversationAuditEvent::AssistantMessage {
                content: final_text.clone(),
                usage: final_usage.clone(),
                duration_ms: duration.as_millis() as u64,
                timestamp: Utc::now(),
            },
        )
        .await?;

        // 13. Update conversation.last_active
        {
            let mut conversations = self.conversations.write().await;
            if let Some(conv) = conversations.get_mut(&conversation_id) {
                conv.last_active = Utc::now();
            }
        }
        {
            let conversations = self.conversations.read().await;
            conversations.save().await?;
        }

        // 14. Broadcast AssistantMessage event
        let _ = self.event_tx.send(ConversationEvent::AssistantMessage {
            conversation_id,
            run_id,
            content: final_text,
            artifacts: Vec::new(),
            usage: final_usage,
            timestamp: Utc::now(),
        });

        Ok(())
    }

    /// Broadcast an error event for a conversation.
    async fn broadcast_error(
        &self,
        conversation_id: &ConversationId,
        run_id: Option<RunId>,
        error: &ThresholdError,
    ) {
        let _ = self
            .write_audit_event(
                conversation_id,
                &ConversationAuditEvent::Error {
                    error: error.to_string(),
                    timestamp: Utc::now(),
                },
            )
            .await;

        let _ = self.event_tx.send(ConversationEvent::Error {
            conversation_id: *conversation_id,
            run_id,
            error: error.to_string(),
        });
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

    /// Delete a conversation and clean up all associated resources.
    ///
    /// - Rejects deletion of the General conversation (singleton fallback).
    /// - Re-attaches all portals to the General conversation.
    /// - Removes the conversation from the store.
    /// - Deletes the conversation directory (memory.md, heartbeat.md).
    /// - Broadcasts `ConversationDeleted` for listeners (scheduler, session cleanup).
    pub async fn delete_conversation(&self, conversation_id: &ConversationId) -> Result<()> {
        // 0. Look up conversation and reject General
        let general_id = {
            let conversations = self.conversations.read().await;
            let conv = conversations
                .get(conversation_id)
                .ok_or(ThresholdError::ConversationNotFound {
                    id: conversation_id.0,
                })?;
            if conv.mode == ConversationMode::General {
                return Err(ThresholdError::InvalidInput {
                    message: "Cannot delete the General conversation".into(),
                });
            }
            // Find General's ID for portal re-attachment
            conversations
                .find_by_mode(&ConversationMode::General)
                .map(|c| c.id)
                .expect("General conversation must exist")
        };

        // 1. Re-attach all portals from this conversation to General.
        //
        // NOTE: There is a small TOCTOU window between snapshotting portal IDs and
        // re-attaching them. A concurrent switch_mode/join_conversation could re-attach
        // a portal back to the soon-to-be-deleted conversation. This is acceptable since
        // deletion is a rare, user-initiated operation. If a race occurs, the affected
        // portal's next message attempt will return ConversationNotFound, which Discord
        // surfaces as an error. The user can then /general or /coding to re-attach.
        let portals_to_move = {
            let portals = self.portals.read().await;
            portals
                .get_portals_for_conversation(conversation_id)
                .into_iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
        };
        if !portals_to_move.is_empty() {
            {
                let mut portals = self.portals.write().await;
                for portal_id in &portals_to_move {
                    portals.attach(portal_id, general_id)?;
                }
                portals.save().await?;
            }
            // Emit PortalAttached events so active listeners (Discord portal_listener)
            // update their tracked conversation_id to General.
            for portal_id in &portals_to_move {
                let _ = self.event_tx.send(ConversationEvent::PortalAttached {
                    portal_id: *portal_id,
                    conversation_id: general_id,
                });
            }
        }

        // 2. Remove conversation from store
        {
            let mut conversations = self.conversations.write().await;
            conversations.remove(conversation_id);
            conversations.save().await?;
        }

        // 3. Remove conversation directory (memory.md, heartbeat.md)
        let conv_dir = self
            .data_dir
            .join("conversations")
            .join(conversation_id.0.to_string());
        if conv_dir.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&conv_dir).await {
                tracing::warn!(
                    conversation_id = %conversation_id.0,
                    error = %e,
                    "failed to remove conversation directory"
                );
            }
        }

        // 4. Broadcast deletion event — listeners handle:
        //    - Scheduler: remove heartbeat task for this conversation
        //    - Server: remove CLI session mapping
        let _ = self.event_tx.send(ConversationEvent::ConversationDeleted {
            conversation_id: *conversation_id,
        });

        tracing::info!(conversation_id = %conversation_id.0, "conversation deleted");
        Ok(())
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

    /// Get access to the Claude client (for process tracker / abort).
    pub fn claude(&self) -> &Arc<ClaudeClient> {
        &self.claude
    }

    /// Get the data directory path (for building file paths externally).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Get the conversation ID for a portal.
    pub async fn get_portal_conversation(&self, portal_id: &PortalId) -> Result<ConversationId> {
        let portals = self.portals.read().await;
        portals
            .get_conversation(portal_id)
            .copied()
            .ok_or(ThresholdError::PortalNotFound { id: portal_id.0 })
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
        // Verify conversation exists and backfill directory if needed
        {
            let conversations = self.conversations.read().await;
            let conv = conversations
                .get(conversation_id)
                .ok_or(ThresholdError::ConversationNotFound {
                    id: conversation_id.0,
                })?;
            conversations.ensure_conversation_dir(conversation_id, &conv.mode);
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

    /// Send a message directly to a conversation (for heartbeat, cron).
    ///
    /// Uses the same streaming path as user messages, giving scheduled tasks:
    /// - Abort support via the process tracker
    /// - Status updates through the portal system
    /// - Proper per-conversation lock handling with timeout
    ///
    /// If the conversation is already processing a message, returns an error
    /// immediately rather than blocking indefinitely.
    pub async fn send_to_conversation(
        &self,
        conversation_id: &ConversationId,
        content: &str,
    ) -> Result<String> {
        use threshold_cli_wrapper::stream::StreamEvent;

        let run_id = RunId::new();

        // 1. Look up conversation
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

        // 2. Look up agent
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

        // 3. Acquire per-conversation lock with timeout.
        //    If the conversation is busy (user message in progress), fail fast
        //    rather than blocking the scheduler for hours.
        let _guard = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.claude.locks().lock(conversation_id.0),
        )
        .await
        {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!(
                    conversation_id = %conversation_id.0,
                    "Scheduled message skipped: conversation busy (lock timeout)"
                );
                return Err(ThresholdError::InvalidInput {
                    message: "Conversation is busy — scheduled message skipped".into(),
                });
            }
        };

        // 4. Start streaming CLI invocation
        self.active_conversations.insert(*conversation_id).await;
        let start = std::time::Instant::now();

        let now = chrono::Local::now();
        let timestamped_content = format!(
            "[{} {}]\n{}",
            now.format("%Y-%m-%d %H:%M"),
            now.format("%Z"),
            content,
        );

        let stream_result = self
            .claude
            .send_message_streaming(
                conversation_id.0,
                run_id,
                &timestamped_content,
                system_prompt,
                Some(model),
            )
            .await;

        let mut stream_handle = match stream_result {
            Ok(handle) => handle,
            Err(e) => {
                self.active_conversations.remove(conversation_id).await;
                self.claude.tracker().deregister(&run_id).await;
                self.broadcast_error(conversation_id, Some(run_id), &e).await;
                return Err(e);
            }
        };

        // 5. Consume stream events, build final response
        let mut final_text = String::new();
        let mut final_session_id = None;
        let mut final_usage = None;
        let mut saw_result = false;

        let status_interval = self.status_interval_secs;
        let mut event_log: Vec<String> = Vec::new();
        let mut last_status = std::time::Instant::now();
        let status_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut has_notable_events = false;

        while let Some(event) = stream_handle.event_rx.recv().await {
            match event {
                StreamEvent::Result {
                    text,
                    session_id,
                    usage,
                } => {
                    final_text = text;
                    final_session_id = session_id;
                    final_usage = usage;
                    saw_result = true;
                    break;
                }
                StreamEvent::Error { message } => {
                    self.active_conversations.remove(conversation_id).await;
                    self.claude.tracker().deregister(&run_id).await;
                    let err = ThresholdError::CliError {
                        provider: "claude".into(),
                        code: -1,
                        stderr: message,
                    };
                    self.broadcast_error(conversation_id, Some(run_id), &err)
                        .await;
                    return Err(err);
                }
                StreamEvent::TextDelta { text } => {
                    final_text.push_str(&text);
                }
                StreamEvent::ToolUse { tool_name, .. } => {
                    tracing::info!(
                        conversation_id = %conversation_id.0,
                        run_id = %run_id,
                        tool = %tool_name,
                        "Tool use detected (scheduled)"
                    );
                    if status_interval > 0 {
                        event_log.push(format!("Using tool: {}", tool_name));
                        has_notable_events = true;
                    }
                }
                StreamEvent::ToolResult { tool_name, is_error } => {
                    if status_interval > 0 {
                        if is_error {
                            event_log.push(format!("Tool {} returned error", tool_name));
                        } else {
                            event_log.push(format!("Tool {} completed", tool_name));
                        }
                        has_notable_events = true;
                    }
                }
            }

            // Periodic status updates (same as handle_message)
            if status_interval > 0
                && has_notable_events
                && last_status.elapsed()
                    >= std::time::Duration::from_secs(status_interval)
                && !status_in_flight.load(std::sync::atomic::Ordering::Relaxed)
            {
                if let Some(haiku) = &self.haiku {
                    status_in_flight
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    let elapsed_secs = start.elapsed().as_secs();
                    let summary_prompt = format!(
                        "Summarize the AI assistant's live activity for a short status line.\n\
                         Rules:\n\
                         - Output ONLY the status text, nothing else\n\
                         - No quotes, no markdown, no bold, no code fences, no preamble\n\
                         - Under 80 characters, plain text, present tense\n\
                         - Focus on the most recent activity\n\
                         Examples of correct output:\n\
                         Reading project structure — 12 files examined\n\
                         Writing implementation for user auth module\n\
                         Running test suite — 3 of 8 passing so far\n\n\
                         Recent events:\n{}",
                        event_log.join("\n")
                    );
                    let haiku = haiku.clone();
                    let event_tx = self.event_tx.clone();
                    let status_cid = *conversation_id;
                    let status_rid = run_id;
                    let in_flight = status_in_flight.clone();
                    let status_audit_dir = self.audit_dir.clone();
                    tokio::spawn(async move {
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            haiku.generate(&summary_prompt, None),
                        )
                        .await;
                        if let Ok(Ok(raw_summary)) = result {
                            let summary = sanitize_status_summary(&raw_summary);
                            tracing::info!(
                                conversation_id = %status_cid.0,
                                run_id = %status_rid,
                                elapsed_secs,
                                summary = %summary,
                                "Status update sent (scheduled)"
                            );
                            if let Err(e) = crate::audit::write_audit_event(
                                &status_audit_dir,
                                &status_cid,
                                &ConversationAuditEvent::StatusUpdate {
                                    run_id: status_rid.0.to_string(),
                                    summary: summary.clone(),
                                    elapsed_secs,
                                    timestamp: Utc::now(),
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "Failed to write status audit event");
                            }
                            let _ = event_tx.send(ConversationEvent::StatusUpdate {
                                conversation_id: status_cid,
                                run_id: status_rid,
                                summary,
                                elapsed_secs,
                            });
                        }
                        in_flight.store(false, std::sync::atomic::Ordering::Relaxed);
                    });
                }
                event_log.clear();
                has_notable_events = false;
                last_status = std::time::Instant::now();
            }
        }

        // 6. Check abort
        let was_aborted = stream_handle
            .was_aborted
            .load(std::sync::atomic::Ordering::Relaxed);

        self.active_conversations.remove(conversation_id).await;
        self.claude.tracker().deregister(&run_id).await;

        if was_aborted {
            self.write_audit_event(
                conversation_id,
                &ConversationAuditEvent::Error {
                    error: "Scheduled task aborted by user".to_string(),
                    timestamp: Utc::now(),
                },
            )
            .await?;

            let _ = self.event_tx.send(ConversationEvent::Aborted {
                conversation_id: *conversation_id,
                run_id,
            });

            return Err(ThresholdError::Aborted);
        }

        // 7. Handle stream close without Result event
        if !saw_result {
            let err = ThresholdError::CliError {
                provider: "claude".into(),
                code: -1,
                stderr: "CLI process exited without producing a result".into(),
            };
            self.broadcast_error(conversation_id, Some(run_id), &err)
                .await;
            return Err(err);
        }

        let duration = start.elapsed();

        // 8. Update session ID
        if let Some(session_id) = &final_session_id {
            self.claude
                .sessions()
                .set(conversation_id.0, session_id.clone())
                .await?;
        }

        // 9. Write assistant response to audit
        self.write_audit_event(
            conversation_id,
            &ConversationAuditEvent::AssistantMessage {
                content: final_text.clone(),
                usage: final_usage.clone(),
                duration_ms: duration.as_millis() as u64,
                timestamp: Utc::now(),
            },
        )
        .await?;

        // 10. Update last_active
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

        // 11. Broadcast AssistantMessage for portal delivery
        let _ = self.event_tx.send(ConversationEvent::AssistantMessage {
            conversation_id: *conversation_id,
            run_id,
            content: final_text.clone(),
            artifacts: Vec::new(),
            usage: final_usage,
            timestamp: Utc::now(),
        });

        Ok(final_text)
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

/// Strip markdown formatting and quoting artifacts from Haiku status summaries.
///
/// Despite explicit prompt instructions, Haiku occasionally wraps its output in
/// code fences, quotes, bold markers, or adds preamble text. This function
/// normalises the summary to plain text.
fn sanitize_status_summary(raw: &str) -> String {
    let s = raw.trim();

    // 1. Strip wrapping code fences: ```...\n<content>\n```
    let s = if s.starts_with("```") && s.ends_with("```") {
        let inner = s.trim_start_matches('`');
        let inner = inner.strip_prefix('\n').unwrap_or(inner);
        let inner = inner.trim_end_matches('`').trim();
        // If there are multiple lines, check if the first line is a language
        // tag (e.g. "rust", "text"). Language tags are short, no spaces, and
        // all-lowercase ascii. Skip it if so; otherwise keep everything.
        if let Some(nl) = inner.find('\n') {
            let first_line = &inner[..nl];
            if first_line.len() < 12
                && !first_line.contains(' ')
                && first_line.chars().all(|c| c.is_ascii_lowercase())
            {
                inner[nl + 1..].trim()
            } else {
                inner
            }
        } else {
            // Single line inside fences — that IS the content
            inner
        }
    } else {
        s
    };

    // 2. Reduce multiline output to last non-empty line (strips preamble).
    //    Must happen before quote/bold stripping so those apply to the final line.
    let s = if s.contains('\n') {
        s.lines()
            .rev()
            .find(|line| {
                let l = line.trim();
                !l.is_empty() && !l.starts_with("```")
            })
            .unwrap_or(s)
    } else {
        s
    };

    // 3. Strip wrapping double-quotes
    let s = s.trim();
    let s = if s.starts_with('"') && s.ends_with('"') && s.len() > 2 {
        &s[1..s.len() - 1]
    } else {
        s
    };

    // 4. Strip leading bullet "- " (before bold, so "- **text**" works)
    let s = s.strip_prefix("- ").unwrap_or(s);

    // 5. Strip leading/trailing bold markers: **text** → text
    let s = s.trim();
    let s = s.strip_prefix("**").unwrap_or(s);
    let s = s.strip_suffix("**").unwrap_or(s);

    s.trim().to_string()
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
            secret_backend: None,
            cli: CliConfig {
                claude: ClaudeCliConfig {
                    command: Some("claude".to_string()),
                    model: Some("sonnet".to_string()),
                    timeout_seconds: None,
                    skip_permissions: Some(false),
                    ack_enabled: None,
                    status_interval_seconds: None,
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
            web: None,
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
        let tmp = tempdir().unwrap();
        let sessions = Arc::new(threshold_cli_wrapper::session::SessionManager::new(
            tmp.path().join("cli-sessions.json"),
        ));
        let locks = Arc::new(threshold_cli_wrapper::ConversationLockMap::new());
        let tracker = Arc::new(threshold_cli_wrapper::ProcessTracker::new());
        let claude = Arc::new(
            threshold_cli_wrapper::ClaudeClient::new(
                "claude".to_string(),
                tmp.path().to_path_buf(),
                false,
                300,
                sessions,
                locks,
                tracker,
            )
            .await
            .unwrap(),
        );

        let engine = ConversationEngine::new(&config, claude, None, None, None, false, 0).await.unwrap();

        let agent = engine.resolve_agent_for_mode(&ConversationMode::Coding {
            project: "test".to_string(),
        });

        assert_eq!(agent.id, "coder");
    }

    #[tokio::test]
    async fn resolve_agent_for_mode_general_uses_default() {
        let config = test_config();
        let tmp = tempdir().unwrap();
        let sessions = Arc::new(threshold_cli_wrapper::session::SessionManager::new(
            tmp.path().join("cli-sessions.json"),
        ));
        let locks = Arc::new(threshold_cli_wrapper::ConversationLockMap::new());
        let tracker = Arc::new(threshold_cli_wrapper::ProcessTracker::new());
        let claude = Arc::new(
            threshold_cli_wrapper::ClaudeClient::new(
                "claude".to_string(),
                tmp.path().to_path_buf(),
                false,
                300,
                sessions,
                locks,
                tracker,
            )
            .await
            .unwrap(),
        );

        let engine = ConversationEngine::new(&config, claude, None, None, None, false, 0).await.unwrap();

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

    async fn make_engine_with_dir(
        dir: &std::path::Path,
    ) -> (Arc<ConversationEngine>, broadcast::Receiver<ConversationEvent>) {
        use threshold_core::config::{ClaudeCliConfig, CliConfig, ToolsConfig};

        let sessions = Arc::new(threshold_cli_wrapper::session::SessionManager::new(
            dir.join("cli").join("cli-sessions.json"),
        ));
        let locks = Arc::new(threshold_cli_wrapper::ConversationLockMap::new());
        let tracker = Arc::new(threshold_cli_wrapper::ProcessTracker::new());
        let claude = Arc::new(
            threshold_cli_wrapper::ClaudeClient::new(
                "claude".to_string(),
                dir.join("cli"),
                false,
                300,
                sessions,
                locks,
                tracker,
            )
            .await
            .unwrap(),
        );
        let config = ThresholdConfig {
            data_dir: Some(dir.to_path_buf()),
            log_level: None,
            secret_backend: None,
            cli: CliConfig {
                claude: ClaudeCliConfig {
                    command: Some("claude".to_string()),
                    model: Some("sonnet".to_string()),
                    timeout_seconds: None,
                    skip_permissions: Some(false),
                    ack_enabled: None,
                    status_interval_seconds: None,
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
            web: None,
        };
        let engine = Arc::new(ConversationEngine::new(&config, claude, None, None, None, false, 0).await.unwrap());
        let rx = engine.subscribe();
        (engine, rx)
    }

    #[tokio::test]
    async fn delete_conversation_rejects_general() {
        let dir = tempdir().unwrap();
        let (engine, _rx) = make_engine_with_dir(dir.path()).await;

        // Register a portal (creates General)
        let _portal_id = engine
            .register_portal(PortalType::Discord {
                guild_id: 1,
                channel_id: 1,
            })
            .await
            .unwrap();

        let conversations = engine.list_conversations().await;
        let general = conversations
            .iter()
            .find(|c| c.mode == ConversationMode::General)
            .unwrap();

        let result = engine.delete_conversation(&general.id).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Cannot delete the General conversation"),
            "expected InvalidInput error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn delete_conversation_removes_and_broadcasts() {
        let dir = tempdir().unwrap();
        let (engine, mut rx) = make_engine_with_dir(dir.path()).await;

        // Register portal (creates General)
        let portal_id = engine
            .register_portal(PortalType::Discord {
                guild_id: 1,
                channel_id: 1,
            })
            .await
            .unwrap();

        // Drain creation/attach events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        // Switch to a coding conversation
        let coding_id = engine
            .switch_mode(
                &portal_id,
                ConversationMode::Coding {
                    project: "test-project".to_string(),
                },
            )
            .await
            .unwrap();

        // Drain switch events
        let _ = rx.try_recv(); // ConversationCreated
        let _ = rx.try_recv(); // PortalDetached
        let _ = rx.try_recv(); // PortalAttached

        // Verify coding conversation directory exists
        let conv_dir = dir
            .path()
            .join("conversations")
            .join(coding_id.0.to_string());
        assert!(conv_dir.exists());

        // Delete the coding conversation
        engine.delete_conversation(&coding_id).await.unwrap();

        // Verify conversation is gone
        let conversations = engine.list_conversations().await;
        assert!(
            !conversations.iter().any(|c| c.id == coding_id),
            "coding conversation should be deleted"
        );

        // Verify directory is removed
        assert!(!conv_dir.exists(), "conversation directory should be removed");

        // Verify events: PortalAttached (re-attach to General) then ConversationDeleted
        let event1 = rx.try_recv().unwrap();
        assert!(
            matches!(event1, ConversationEvent::PortalAttached { .. }),
            "expected PortalAttached for re-attached portal"
        );
        let event2 = rx.try_recv().unwrap();
        assert!(matches!(
            event2,
            ConversationEvent::ConversationDeleted { conversation_id } if conversation_id == coding_id
        ));
    }

    #[tokio::test]
    async fn delete_conversation_reattaches_portals_to_general() {
        let dir = tempdir().unwrap();
        let (engine, _rx) = make_engine_with_dir(dir.path()).await;

        // Register portal (creates General + attaches)
        let portal_id = engine
            .register_portal(PortalType::Discord {
                guild_id: 1,
                channel_id: 1,
            })
            .await
            .unwrap();

        // Switch to coding
        let coding_id = engine
            .switch_mode(
                &portal_id,
                ConversationMode::Coding {
                    project: "test".to_string(),
                },
            )
            .await
            .unwrap();

        // Verify portal is on coding conversation
        let portal_conv = engine.get_portal_conversation(&portal_id).await.unwrap();
        assert_eq!(portal_conv, coding_id);

        // Delete coding conversation — portal should move to General
        engine.delete_conversation(&coding_id).await.unwrap();

        let portal_conv_after = engine.get_portal_conversation(&portal_id).await.unwrap();
        // Should be on General now
        let conversations = engine.list_conversations().await;
        let general = conversations
            .iter()
            .find(|c| c.mode == ConversationMode::General)
            .unwrap();
        assert_eq!(portal_conv_after, general.id);
    }

    #[tokio::test]
    async fn delete_nonexistent_conversation_returns_error() {
        let dir = tempdir().unwrap();
        let (engine, _rx) = make_engine_with_dir(dir.path()).await;

        let fake_id = ConversationId::new();
        let result = engine.delete_conversation(&fake_id).await;
        assert!(result.is_err());
    }

    // --- sanitize_status_summary tests ---

    #[test]
    fn sanitize_strips_code_fences() {
        let raw = "```\nReading project files — 5 examined\n```";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Reading project files — 5 examined"
        );
    }

    #[test]
    fn sanitize_code_fences_single_word() {
        // Single-word status inside fences should not be mistaken for a language tag
        let raw = "```\nRefactoring\n```";
        assert_eq!(super::sanitize_status_summary(raw), "Refactoring");
    }

    #[test]
    fn sanitize_code_fences_with_language_tag() {
        let raw = "```text\nRunning tests — 5 passing\n```";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Running tests — 5 passing"
        );
    }

    #[test]
    fn sanitize_strips_wrapping_quotes() {
        let raw = r#""Updating scheduler timezone support — editing task.rs""#;
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Updating scheduler timezone support — editing task.rs"
        );
    }

    #[test]
    fn sanitize_strips_bold_markers() {
        let raw = "**Writing code changes — implementation in progress**";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Writing code changes — implementation in progress"
        );
    }

    #[test]
    fn sanitize_strips_preamble_and_bold() {
        let raw = "Looking at the recent events, the assistant is writing code. Here's a concise status line:\n\n**Analyzing project state — 7 bash queries executed**";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Analyzing project state — 7 bash queries executed"
        );
    }

    #[test]
    fn sanitize_strips_bullet_prefix() {
        let raw = "- Running test suite — 3 of 8 passing";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Running test suite — 3 of 8 passing"
        );
    }

    #[test]
    fn sanitize_passes_clean_text_through() {
        let raw = "Reading project structure — 12 files examined";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Reading project structure — 12 files examined"
        );
    }

    #[test]
    fn sanitize_handles_empty_input() {
        assert_eq!(super::sanitize_status_summary(""), "");
        assert_eq!(super::sanitize_status_summary("  "), "");
    }

    #[test]
    fn sanitize_bullet_then_bold() {
        let raw = "- **Running tests — 3 passing**";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Running tests — 3 passing"
        );
    }

    #[test]
    fn sanitize_preamble_then_quoted_status() {
        let raw = "Here's a status update:\n\n\"Running test suite — verifying changes\"";
        assert_eq!(
            super::sanitize_status_summary(raw),
            "Running test suite — verifying changes"
        );
    }
}
