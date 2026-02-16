//! Tool execution context

use std::path::PathBuf;
use threshold_core::{ConversationId, PortalId};
use tokio_util::sync::CancellationToken;

use crate::ToolProfile;

/// Permission mode for tool execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermissionMode {
    /// Allow all tool calls without prompting
    FullAuto,
    /// Prompt only for destructive operations
    ApproveDestructive,
    /// Prompt for all tool calls
    ApproveAll,
}

/// Context provided to tools during execution
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Conversation ID (if executing within a conversation)
    pub conversation_id: Option<ConversationId>,
    /// Portal ID (if executing via a portal)
    pub portal_id: Option<PortalId>,
    /// Agent ID executing the tool
    pub agent_id: String,
    /// Working directory for file operations
    pub working_dir: PathBuf,
    /// Tool profile (controls which tools are available)
    pub profile: ToolProfile,
    /// Permission mode (controls prompting behavior)
    pub permission_mode: ToolPermissionMode,
    /// Cancellation token for graceful shutdown
    pub cancellation: CancellationToken,
}

impl ToolContext {
    /// Create a new tool context with default values
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            conversation_id: None,
            portal_id: None,
            agent_id: agent_id.into(),
            working_dir: std::env::current_dir().unwrap_or_default(),
            profile: ToolProfile::Full,
            permission_mode: ToolPermissionMode::ApproveAll,
            cancellation: CancellationToken::new(),
        }
    }

    /// Set the conversation ID
    pub fn with_conversation(mut self, conversation_id: ConversationId) -> Self {
        self.conversation_id = Some(conversation_id);
        self
    }

    /// Set the portal ID
    pub fn with_portal(mut self, portal_id: PortalId) -> Self {
        self.portal_id = Some(portal_id);
        self
    }

    /// Set the working directory
    pub fn with_working_dir(mut self, working_dir: PathBuf) -> Self {
        self.working_dir = working_dir;
        self
    }

    /// Set the tool profile
    pub fn with_profile(mut self, profile: ToolProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Set the permission mode
    pub fn with_permission_mode(mut self, permission_mode: ToolPermissionMode) -> Self {
        self.permission_mode = permission_mode;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_context_new_creates_default_context() {
        let ctx = ToolContext::new("test-agent");
        assert_eq!(ctx.agent_id, "test-agent");
        assert!(ctx.conversation_id.is_none());
        assert!(ctx.portal_id.is_none());
        assert_eq!(ctx.profile, ToolProfile::Full);
        assert_eq!(ctx.permission_mode, ToolPermissionMode::ApproveAll);
    }

    #[test]
    fn tool_context_with_conversation_sets_conversation_id() {
        let conv_id = ConversationId(uuid::Uuid::new_v4());
        let ctx = ToolContext::new("test-agent").with_conversation(conv_id);
        assert_eq!(ctx.conversation_id, Some(conv_id));
    }

    #[test]
    fn tool_context_with_profile_sets_profile() {
        let ctx = ToolContext::new("test-agent").with_profile(ToolProfile::Minimal);
        assert_eq!(ctx.profile, ToolProfile::Minimal);
    }
}
