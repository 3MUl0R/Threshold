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
