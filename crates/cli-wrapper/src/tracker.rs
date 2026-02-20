//! Process tracking for abort support.
//!
//! The `ProcessTracker` stores a `CancellationToken` per running CLI invocation,
//! keyed by `RunId`. A secondary index maps `ConversationId → RunId` for O(1)
//! abort lookups. The per-conversation lock in `ConversationLockMap` ensures at
//! most one active run per conversation; the secondary index enforces this
//! invariant at the tracker level too (registering a new run for the same
//! conversation cancels and replaces the previous one).

use std::collections::HashMap;
use std::time::Instant;
use threshold_core::{ConversationId, RunId, ThresholdError};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Metadata for a running CLI invocation.
struct RunHandle {
    abort_token: CancellationToken,
    conversation_id: ConversationId,
    started_at: Instant,
}

/// Tracks running CLI processes for abort support.
///
/// Each running invocation is registered with a `RunId` and a `CancellationToken`.
/// The streaming read loop monitors the token; `/abort` cancels it.
pub struct ProcessTracker {
    runs: RwLock<Inner>,
}

struct Inner {
    by_run: HashMap<RunId, RunHandle>,
    by_conversation: HashMap<ConversationId, RunId>,
}

impl ProcessTracker {
    pub fn new() -> Self {
        Self {
            runs: RwLock::new(Inner {
                by_run: HashMap::new(),
                by_conversation: HashMap::new(),
            }),
        }
    }

    /// Register a running process. Returns the abort token for the streaming loop.
    ///
    /// If another run is already registered for the same conversation, it is
    /// cancelled and replaced (this shouldn't happen normally because the
    /// per-conversation lock serializes runs).
    pub async fn register(
        &self,
        run_id: RunId,
        conversation_id: ConversationId,
    ) -> CancellationToken {
        let token = CancellationToken::new();
        let mut inner = self.runs.write().await;

        // If there's already a run for this conversation, cancel and remove it.
        if let Some(&old_run_id) = inner.by_conversation.get(&conversation_id) {
            if let Some(old_handle) = inner.by_run.remove(&old_run_id) {
                old_handle.abort_token.cancel();
                tracing::warn!(
                    old_run_id = %old_run_id,
                    new_run_id = %run_id,
                    "replaced existing run for conversation"
                );
            }
        }

        inner.by_run.insert(
            run_id,
            RunHandle {
                abort_token: token.clone(),
                conversation_id,
                started_at: Instant::now(),
            },
        );
        inner.by_conversation.insert(conversation_id, run_id);
        token
    }

    /// Deregister a completed process.
    pub async fn deregister(&self, run_id: &RunId) {
        let mut inner = self.runs.write().await;
        if let Some(handle) = inner.by_run.remove(run_id) {
            // Only remove from by_conversation if it still points to this run_id
            // (another run may have already replaced it).
            if inner.by_conversation.get(&handle.conversation_id) == Some(run_id) {
                inner.by_conversation.remove(&handle.conversation_id);
            }
        }
    }

    /// Abort the active run for a conversation.
    ///
    /// Cancels the `CancellationToken`, which signals the streaming loop to
    /// kill the child process. Returns the `RunId` of the aborted run.
    pub async fn abort_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<RunId, ThresholdError> {
        let inner = self.runs.read().await;
        let run_id = inner
            .by_conversation
            .get(conversation_id)
            .ok_or(ThresholdError::InvalidInput {
                message: "No running task for this conversation".into(),
            })?;
        let handle = inner.by_run.get(run_id).ok_or(ThresholdError::InvalidInput {
            message: "No running task for this conversation".into(),
        })?;
        let run_id = *run_id;
        handle.abort_token.cancel();
        Ok(run_id)
    }

    /// Get the active run for a conversation, if any.
    pub async fn active_run(&self, conversation_id: &ConversationId) -> Option<RunId> {
        self.runs
            .read()
            .await
            .by_conversation
            .get(conversation_id)
            .copied()
    }

    /// Get the elapsed time for a running task, if any.
    pub async fn elapsed(&self, run_id: &RunId) -> Option<std::time::Duration> {
        self.runs
            .read()
            .await
            .by_run
            .get(run_id)
            .map(|h| h.started_at.elapsed())
    }

    /// Number of currently tracked runs.
    pub async fn count(&self) -> usize {
        self.runs.read().await.by_run.len()
    }
}

impl Default for ProcessTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_deregister() {
        let tracker = ProcessTracker::new();
        let run_id = RunId::new();
        let conv_id = ConversationId(uuid::Uuid::new_v4());

        let _token = tracker.register(run_id, conv_id).await;
        assert_eq!(tracker.count().await, 1);

        tracker.deregister(&run_id).await;
        assert_eq!(tracker.count().await, 0);
    }

    #[tokio::test]
    async fn abort_cancels_token() {
        let tracker = ProcessTracker::new();
        let run_id = RunId::new();
        let conv_id = ConversationId(uuid::Uuid::new_v4());

        let token = tracker.register(run_id, conv_id).await;
        assert!(!token.is_cancelled());

        let aborted_id = tracker.abort_conversation(&conv_id).await.unwrap();
        assert_eq!(aborted_id, run_id);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn abort_nonexistent_returns_error() {
        let tracker = ProcessTracker::new();
        let conv_id = ConversationId(uuid::Uuid::new_v4());

        let result = tracker.abort_conversation(&conv_id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn active_run_returns_id() {
        let tracker = ProcessTracker::new();
        let run_id = RunId::new();
        let conv_id = ConversationId(uuid::Uuid::new_v4());

        assert!(tracker.active_run(&conv_id).await.is_none());

        let _token = tracker.register(run_id, conv_id).await;
        assert_eq!(tracker.active_run(&conv_id).await, Some(run_id));

        tracker.deregister(&run_id).await;
        assert!(tracker.active_run(&conv_id).await.is_none());
    }

    #[tokio::test]
    async fn elapsed_returns_duration() {
        let tracker = ProcessTracker::new();
        let run_id = RunId::new();
        let conv_id = ConversationId(uuid::Uuid::new_v4());

        let _token = tracker.register(run_id, conv_id).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let elapsed = tracker.elapsed(&run_id).await.unwrap();
        assert!(elapsed >= std::time::Duration::from_millis(10));
    }

    #[tokio::test]
    async fn multiple_conversations_tracked() {
        let tracker = ProcessTracker::new();
        let run_a = RunId::new();
        let run_b = RunId::new();
        let conv_a = ConversationId(uuid::Uuid::new_v4());
        let conv_b = ConversationId(uuid::Uuid::new_v4());

        let _token_a = tracker.register(run_a, conv_a).await;
        let _token_b = tracker.register(run_b, conv_b).await;
        assert_eq!(tracker.count().await, 2);

        // Abort only conv_a
        tracker.abort_conversation(&conv_a).await.unwrap();
        assert_eq!(tracker.count().await, 2); // still tracked until deregister

        tracker.deregister(&run_a).await;
        assert_eq!(tracker.count().await, 1);
        assert_eq!(tracker.active_run(&conv_b).await, Some(run_b));
    }

    #[tokio::test]
    async fn register_replaces_existing_run_for_same_conversation() {
        let tracker = ProcessTracker::new();
        let conv_id = ConversationId(uuid::Uuid::new_v4());
        let run_1 = RunId::new();
        let run_2 = RunId::new();

        let token_1 = tracker.register(run_1, conv_id).await;
        assert!(!token_1.is_cancelled());

        // Register a second run for the same conversation — should cancel run_1
        let _token_2 = tracker.register(run_2, conv_id).await;
        assert!(token_1.is_cancelled());
        assert_eq!(tracker.active_run(&conv_id).await, Some(run_2));
        // Old run removed from by_run
        assert!(tracker.elapsed(&run_1).await.is_none());
    }
}
