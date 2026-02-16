//! Channel-as-portal mapping.

use threshold_conversation::ConversationEngine;
use threshold_core::PortalId;

/// Resolve an existing portal for this channel, or create a new one
/// attached to the General conversation.
pub async fn resolve_or_create_portal(
    engine: &ConversationEngine,
    guild_id: u64,
    channel_id: u64,
) -> PortalId {
    // TODO: Phase 4.6 implementation
    PortalId::new()
}
