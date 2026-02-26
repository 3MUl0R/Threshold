use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ──── Run Tracking ────

/// Unique identifier for a single user request → agent response cycle.
///
/// Each time a user sends a message, a new RunId is generated. All progress
/// events (ack, status updates, final response, abort) are tagged with this ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub Uuid);

impl RunId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0.to_string()[..8])
    }
}

// ──── Conversations ────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl ConversationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ConversationId {
    fn default() -> Self {
        Self::new()
    }
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
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PortalId {
    fn default() -> Self {
        Self::new()
    }
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
    #[serde(alias = "Coding")]
    Standard,
    Full,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolPermissionMode {
    FullAuto,
    ApproveDestructive,
    ApproveAll,
}

// ──── Scheduled Actions ────

/// What a scheduled task should do when it fires.
///
/// This is the source of truth for all scheduling action types across the system.
/// The scheduler engine (Milestone 6) executes these, and the CLI/Discord
/// interfaces create tasks that reference them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Use `{output}` placeholder in `prompt_template` for script output.
    ScriptThenConversation {
        command: String,
        prompt_template: String,
        model: Option<String>,
    },
}

// ──── Result Delivery ────

/// Trait for delivering scheduled task results to external channels.
///
/// Defined in core to break the scheduler→discord circular dependency.
/// The discord crate implements this as `DiscordOutbound`, and it's injected
/// into the scheduler at construction time in the server binary.
#[async_trait::async_trait]
pub trait ResultSender: Send + Sync {
    /// Send a message to a Discord channel (or other channel-like destination).
    async fn send_to_channel(&self, channel_id: u64, message: &str) -> Result<(), crate::ThresholdError>;
    /// Send a direct message to a user.
    async fn send_dm(&self, user_id: u64, message: &str) -> Result<(), crate::ThresholdError>;
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

// ──── Daemon State ────

/// Static health configuration, set at daemon startup and never changing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub version: String,
}

/// Shared daemon state for drain management and active work tracking.
///
/// Uses atomics for lock-free reads from health checks and drain checks.
/// An `Arc<DaemonState>` is created at daemon startup and passed to all subsystems.
#[derive(Debug)]
pub struct DaemonState {
    /// True when the daemon is preparing to shut down.
    draining: std::sync::atomic::AtomicBool,
    /// Number of active work items (conversations, script tasks, etc.).
    active_work: std::sync::atomic::AtomicU32,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            draining: std::sync::atomic::AtomicBool::new(false),
            active_work: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set_draining(&self, value: bool) {
        self.draining
            .store(value, std::sync::atomic::Ordering::Release);
    }

    pub fn active_work(&self) -> u32 {
        self.active_work
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn increment_work(&self) -> u32 {
        self.active_work
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1
    }

    pub fn decrement_work(&self) -> u32 {
        let prev = self
            .active_work
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| Some(n.saturating_sub(1)),
            )
            .unwrap(); // fetch_update with Some always succeeds
        debug_assert!(prev > 0, "active_work underflow: double-decrement bug");
        prev.saturating_sub(1)
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that decrements `DaemonState.active_work` on drop.
///
/// Guarantees the counter is decremented on all exit paths — success, error,
/// panic, and cancellation. Use `WorkGuard::acquire()` to atomically
/// increment the counter and create the guard.
pub struct WorkGuard(std::sync::Arc<DaemonState>);

impl WorkGuard {
    /// Atomically increment the active work counter and return an RAII guard
    /// that decrements it on drop.
    pub fn acquire(state: &std::sync::Arc<DaemonState>) -> Self {
        state.increment_work();
        Self(state.clone())
    }
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0.decrement_work();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_id_unique() {
        let a = ConversationId::new();
        let b = ConversationId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn conversation_mode_key_general() {
        assert_eq!(ConversationMode::General.key(), "general");
    }

    #[test]
    fn conversation_mode_key_coding_lowercase() {
        let mode = ConversationMode::Coding {
            project: "MyProject".into(),
        };
        assert_eq!(mode.key(), "coding:myproject");
    }

    #[test]
    fn conversation_mode_key_research_lowercase() {
        let mode = ConversationMode::Research {
            topic: "Quantum Computing".into(),
        };
        assert_eq!(mode.key(), "research:quantum computing");
    }

    #[test]
    fn conversation_mode_key_stability() {
        let mode_a = ConversationMode::Coding {
            project: "Alpha".into(),
        };
        let mode_b = ConversationMode::Coding {
            project: "Alpha".into(),
        };
        assert_eq!(mode_a.key(), mode_b.key());
    }

    #[test]
    fn conversation_mode_key_case_insensitive() {
        let upper = ConversationMode::Coding {
            project: "PROJECT".into(),
        };
        let lower = ConversationMode::Coding {
            project: "project".into(),
        };
        assert_eq!(upper.key(), lower.key());
    }

    #[test]
    fn conversation_serde_round_trip() {
        let conv = Conversation {
            id: ConversationId::new(),
            mode: ConversationMode::Coding {
                project: "threshold".into(),
            },
            cli_provider: CliProvider::Claude {
                model: "sonnet".into(),
            },
            agent_id: "default".into(),
            created_at: Utc::now(),
            last_active: Utc::now(),
        };
        let json = serde_json::to_string(&conv).unwrap();
        let restored: Conversation = serde_json::from_str(&json).unwrap();
        assert_eq!(conv.id, restored.id);
        assert_eq!(conv.mode, restored.mode);
        assert_eq!(conv.agent_id, restored.agent_id);
    }

    #[test]
    fn portal_serde_round_trip() {
        let portal = Portal {
            id: PortalId::new(),
            portal_type: PortalType::Discord {
                guild_id: 123,
                channel_id: 456,
            },
            conversation_id: ConversationId::new(),
            connected_at: Utc::now(),
        };
        let json = serde_json::to_string(&portal).unwrap();
        let restored: Portal = serde_json::from_str(&json).unwrap();
        assert_eq!(portal.id, restored.id);
        assert_eq!(portal.conversation_id, restored.conversation_id);
    }

    #[test]
    fn message_serde_round_trip() {
        let msg = Message {
            id: Uuid::new_v4(),
            conversation_id: ConversationId::new(),
            role: MessageRole::User,
            content: "Hello, world!".into(),
            portal_source: Some(PortalId::new()),
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, restored.id);
        assert_eq!(msg.content, restored.content);
    }

    #[test]
    fn tool_profile_serde_round_trip() {
        for profile in [ToolProfile::Minimal, ToolProfile::Standard, ToolProfile::Full] {
            let json = serde_json::to_string(&profile).unwrap();
            let restored: ToolProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(profile, restored);
        }
    }

    #[test]
    fn tool_permission_mode_serde_round_trip() {
        for mode in [
            ToolPermissionMode::FullAuto,
            ToolPermissionMode::ApproveDestructive,
            ToolPermissionMode::ApproveAll,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let restored: ToolPermissionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, restored);
        }
    }

    #[test]
    fn scheduled_action_new_conversation_serde_round_trip() {
        let action = ScheduledAction::NewConversation {
            prompt: "Run nightly tests".into(),
            model: Some("sonnet".into()),
        };
        let json = serde_json::to_string(&action).unwrap();
        let restored: ScheduledAction = serde_json::from_str(&json).unwrap();
        assert_eq!(action, restored);
    }

    #[test]
    fn scheduled_action_resume_conversation_serde_round_trip() {
        let action = ScheduledAction::ResumeConversation {
            conversation_id: ConversationId::new(),
            prompt: "Continue working on the project".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let restored: ScheduledAction = serde_json::from_str(&json).unwrap();
        assert_eq!(action, restored);
    }

    #[test]
    fn scheduled_action_script_serde_round_trip() {
        let action = ScheduledAction::Script {
            command: "cargo test".into(),
            working_dir: Some("/home/user/project".into()),
        };
        let json = serde_json::to_string(&action).unwrap();
        let restored: ScheduledAction = serde_json::from_str(&json).unwrap();
        assert_eq!(action, restored);
    }

    #[test]
    fn scheduled_action_script_then_conversation_serde_round_trip() {
        let action = ScheduledAction::ScriptThenConversation {
            command: "curl https://api.example.com/health".into(),
            prompt_template: "API health check result:\n{output}\n\nAnalyze this.".into(),
            model: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        let restored: ScheduledAction = serde_json::from_str(&json).unwrap();
        assert_eq!(action, restored);
    }

    #[test]
    fn scheduled_action_script_optional_fields() {
        let action = ScheduledAction::Script {
            command: "echo hello".into(),
            working_dir: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        let restored: ScheduledAction = serde_json::from_str(&json).unwrap();
        assert_eq!(action, restored);

        // Verify None working_dir serializes correctly
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["Script"]["working_dir"], serde_json::Value::Null);
    }

    #[test]
    fn tool_profile_coding_alias_deserializes_to_standard() {
        // Regression test: legacy "Coding" value must deserialize into Standard
        // to maintain backwards compatibility after the Coding→Standard rename.
        let json = r#""Coding""#;
        let profile: ToolProfile = serde_json::from_str(json).unwrap();
        assert_eq!(profile, ToolProfile::Standard);
    }

    #[test]
    fn agent_config_serde_round_trip() {
        let agent = AgentConfig {
            id: "coder".into(),
            name: "Code Assistant".into(),
            cli_provider: CliProvider::Claude {
                model: "opus".into(),
            },
            system_prompt: Some("You are a coding assistant.".into()),
            tool_profile: ToolProfile::Standard,
        };
        let json = serde_json::to_string(&agent).unwrap();
        let restored: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(agent.id, restored.id);
        assert_eq!(agent.name, restored.name);
        assert_eq!(agent.tool_profile, restored.tool_profile);
        assert_eq!(agent.system_prompt, restored.system_prompt);
    }

    #[test]
    fn health_config_serde_round_trip() {
        let config = HealthConfig {
            pid: 12345,
            started_at: Utc::now(),
            version: "0.1.0".into(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: HealthConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pid, 12345);
        assert_eq!(restored.version, "0.1.0");
    }

    #[test]
    fn daemon_state_default_values() {
        let state = DaemonState::new();
        assert!(!state.is_draining());
        assert_eq!(state.active_work(), 0);
    }

    #[test]
    fn daemon_state_draining_toggle() {
        let state = DaemonState::new();
        state.set_draining(true);
        assert!(state.is_draining());
        state.set_draining(false);
        assert!(!state.is_draining());
    }

    #[test]
    fn daemon_state_active_work_increment_decrement() {
        let state = DaemonState::new();
        assert_eq!(state.increment_work(), 1);
        assert_eq!(state.increment_work(), 2);
        assert_eq!(state.active_work(), 2);
        assert_eq!(state.decrement_work(), 1);
        assert_eq!(state.decrement_work(), 0);
        assert_eq!(state.active_work(), 0);
    }

    #[test]
    #[should_panic(expected = "active_work underflow")]
    fn daemon_state_decrement_saturates_at_zero() {
        let state = DaemonState::new();
        // In debug builds, the debug_assert fires on underflow.
        // The value still saturates (won't wrap to u32::MAX) via saturating_sub.
        state.decrement_work();
    }

    #[test]
    fn work_guard_decrements_on_drop() {
        let state = std::sync::Arc::new(DaemonState::new());
        assert_eq!(state.active_work(), 0);
        {
            let _guard = WorkGuard::acquire(&state);
            // active_work incremented to 1 while guard is alive
            assert_eq!(state.active_work(), 1);
        }
        // guard dropped, counter decremented
        assert_eq!(state.active_work(), 0);
    }
}
