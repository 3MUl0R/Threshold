//! Shared tracker for conversations with active CLI invocations.
//!
//! Injected (via `Arc`) into both the conversation engine and the scheduler
//! so that the scheduler can skip heartbeats for conversations already being
//! processed — whether from a user message or another scheduled task.

use std::collections::HashSet;

use tokio::sync::RwLock;

use crate::ConversationId;

/// Tracks which conversations currently have an active CLI invocation.
pub struct ActiveConversations {
    inner: RwLock<HashSet<ConversationId>>,
}

impl ActiveConversations {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashSet::new()),
        }
    }

    /// Mark a conversation as having an active CLI invocation.
    pub async fn insert(&self, id: ConversationId) {
        self.inner.write().await.insert(id);
    }

    /// Remove a conversation from the active set.
    pub async fn remove(&self, id: &ConversationId) {
        self.inner.write().await.remove(id);
    }

    /// Check whether a conversation currently has an active CLI invocation.
    pub async fn contains(&self, id: &ConversationId) -> bool {
        self.inner.read().await.contains(id)
    }
}

impl Default for ActiveConversations {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn insert_and_contains() {
        let tracker = ActiveConversations::new();
        let id = ConversationId::new();

        assert!(!tracker.contains(&id).await);
        tracker.insert(id).await;
        assert!(tracker.contains(&id).await);
    }

    #[tokio::test]
    async fn remove_clears_entry() {
        let tracker = ActiveConversations::new();
        let id = ConversationId::new();

        tracker.insert(id).await;
        assert!(tracker.contains(&id).await);

        tracker.remove(&id).await;
        assert!(!tracker.contains(&id).await);
    }

    #[tokio::test]
    async fn remove_nonexistent_is_noop() {
        let tracker = ActiveConversations::new();
        let id = ConversationId::new();

        // Should not panic
        tracker.remove(&id).await;
        assert!(!tracker.contains(&id).await);
    }

    #[tokio::test]
    async fn multiple_conversations_tracked_independently() {
        let tracker = ActiveConversations::new();
        let id_a = ConversationId::new();
        let id_b = ConversationId::new();

        tracker.insert(id_a).await;
        assert!(tracker.contains(&id_a).await);
        assert!(!tracker.contains(&id_b).await);

        tracker.insert(id_b).await;
        assert!(tracker.contains(&id_a).await);
        assert!(tracker.contains(&id_b).await);

        tracker.remove(&id_a).await;
        assert!(!tracker.contains(&id_a).await);
        assert!(tracker.contains(&id_b).await);
    }

    #[tokio::test]
    async fn default_is_empty() {
        let tracker = ActiveConversations::default();
        let id = ConversationId::new();
        assert!(!tracker.contains(&id).await);
    }
}
