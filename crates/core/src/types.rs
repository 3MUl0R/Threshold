use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
        for profile in [ToolProfile::Minimal, ToolProfile::Coding, ToolProfile::Full] {
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
    fn agent_config_serde_round_trip() {
        let agent = AgentConfig {
            id: "coder".into(),
            name: "Code Assistant".into(),
            cli_provider: CliProvider::Claude {
                model: "opus".into(),
            },
            system_prompt: Some("You are a coding assistant.".into()),
            tool_profile: ToolProfile::Coding,
        };
        let json = serde_json::to_string(&agent).unwrap();
        let restored: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(agent.id, restored.id);
        assert_eq!(agent.name, restored.name);
        assert_eq!(agent.tool_profile, restored.tool_profile);
        assert_eq!(agent.system_prompt, restored.system_prompt);
    }
}
