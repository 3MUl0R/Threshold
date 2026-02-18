//! Shared tracker for conversations with active CLI invocations.
//!
//! Injected (via `Arc`) into both the conversation engine and the scheduler
//! so that the scheduler can skip heartbeats for conversations already being
//! processed — whether from a user message or another scheduled task.
//!
//! Uses a refcount (`HashMap<ConversationId, usize>`) so that overlapping
//! invocations on the same conversation (e.g., a user message and a scheduler
//! task) don't clear the active flag prematurely when one finishes.

use std::collections::HashMap;

use tokio::sync::RwLock;

use crate::ConversationId;

/// Tracks which conversations currently have an active CLI invocation.
///
/// Uses reference counting internally: each `insert` increments the count
/// and each `remove` decrements it. The conversation is only removed from the
/// active set when the count reaches zero.
pub struct ActiveConversations {
    inner: RwLock<HashMap<ConversationId, usize>>,
}

impl ActiveConversations {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Mark a conversation as having an active CLI invocation.
    ///
    /// Multiple calls for the same conversation increment a refcount.
    pub async fn insert(&self, id: ConversationId) {
        let mut map = self.inner.write().await;
        *map.entry(id).or_insert(0) += 1;
    }

    /// Remove one active invocation for a conversation.
    ///
    /// Decrements the refcount; only removes the entry when it reaches zero.
    pub async fn remove(&self, id: &ConversationId) {
        let mut map = self.inner.write().await;
        if let Some(count) = map.get_mut(id) {
            *count -= 1;
            if *count == 0 {
                map.remove(id);
            }
        }
    }

    /// Check whether a conversation currently has an active CLI invocation.
    pub async fn contains(&self, id: &ConversationId) -> bool {
        self.inner.read().await.contains_key(id)
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

    #[tokio::test]
    async fn refcount_concurrent_inserts() {
        let tracker = ActiveConversations::new();
        let id = ConversationId::new();

        // Two overlapping inserts
        tracker.insert(id).await;
        tracker.insert(id).await;
        assert!(tracker.contains(&id).await);

        // First remove — still active
        tracker.remove(&id).await;
        assert!(tracker.contains(&id).await);

        // Second remove — now cleared
        tracker.remove(&id).await;
        assert!(!tracker.contains(&id).await);
    }
}
