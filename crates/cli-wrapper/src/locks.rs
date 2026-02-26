//! Per-conversation execution locks.
//!
//! Replaces the global `ExecutionQueue` with per-conversation mutexes,
//! allowing multiple conversations to invoke Claude concurrently while
//! serializing messages within the same conversation.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use uuid::Uuid;

/// Per-conversation execution locks.
///
/// Each conversation gets its own `Mutex`, so conversations run in parallel
/// while messages within the same conversation are serialized (preventing
/// session race conditions on the Claude CLI's `--session-id` files).
pub struct ConversationLockMap {
    locks: RwLock<HashMap<Uuid, Arc<Mutex<()>>>>,
}

impl ConversationLockMap {
    pub fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::new()),
        }
    }

    /// Acquire the lock for a conversation. Creates it if it doesn't exist.
    pub async fn lock(&self, conversation_id: Uuid) -> OwnedMutexGuard<()> {
        let mutex = self.get_or_create(conversation_id).await;
        mutex.lock_owned().await
    }

    /// Non-blocking lock attempt. Returns `Some(guard)` if acquired,
    /// `None` if the conversation lock is already held.
    ///
    /// Used by the engine to detect "queued" status before blocking.
    pub async fn try_lock(&self, conversation_id: Uuid) -> Option<OwnedMutexGuard<()>> {
        let mutex = self.get_or_create(conversation_id).await;
        mutex.try_lock_owned().ok()
    }

    /// Remove the lock for a deleted conversation.
    ///
    /// Only removes the entry if no guard is currently held (strong count == 1,
    /// meaning only the map owns the Arc). If a request is in-flight, the entry
    /// is left in place — `sweep_idle()` will clean it up later.
    pub async fn remove(&self, conversation_id: Uuid) {
        let mut map = self.locks.write().await;
        if let Some(mutex) = map.get(&conversation_id) {
            if Arc::strong_count(mutex) <= 1 {
                map.remove(&conversation_id);
            } else {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "skipping lock removal: guard still held, sweep_idle will clean up"
                );
            }
        }
    }

    /// Remove lock entries that are idle (not held by anyone).
    ///
    /// An entry is idle when its `Arc` strong count is 1, meaning only the
    /// map itself holds a reference (no outstanding guards). Called
    /// periodically to prevent unbounded growth from one-shot conversations.
    pub async fn sweep_idle(&self) {
        let mut map = self.locks.write().await;
        let before = map.len();
        map.retain(|_id, mutex| Arc::strong_count(mutex) > 1);
        let removed = before - map.len();
        if removed > 0 {
            tracing::debug!(
                removed,
                remaining = map.len(),
                "swept idle conversation locks"
            );
        }
    }

    async fn get_or_create(&self, conversation_id: Uuid) -> Arc<Mutex<()>> {
        let mut map = self.locks.write().await;
        map.entry(conversation_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

impl Default for ConversationLockMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn multiple_conversations_run_concurrently() {
        let locks = Arc::new(ConversationLockMap::new());
        let conv_a = Uuid::new_v4();
        let conv_b = Uuid::new_v4();

        let started = Arc::new(tokio::sync::Barrier::new(2));

        // Spawn two tasks for different conversations — they should run in parallel
        let locks_a = locks.clone();
        let started_a = started.clone();
        let task_a = tokio::spawn(async move {
            let _guard = locks_a.lock(conv_a).await;
            started_a.wait().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let locks_b = locks.clone();
        let started_b = started.clone();
        let task_b = tokio::spawn(async move {
            let _guard = locks_b.lock(conv_b).await;
            started_b.wait().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        // Both should complete within ~50ms if concurrent (not ~100ms if serial)
        let start = std::time::Instant::now();
        let _ = tokio::join!(task_a, task_b);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(150),
            "expected concurrent execution, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn same_conversation_serialized() {
        let locks = Arc::new(ConversationLockMap::new());
        let conv = Uuid::new_v4();
        let order = Arc::new(tokio::sync::Mutex::new(Vec::new()));

        let locks1 = locks.clone();
        let order1 = order.clone();
        let task1 = tokio::spawn(async move {
            let _guard = locks1.lock(conv).await;
            order1.lock().await.push(1);
            tokio::time::sleep(Duration::from_millis(50)).await;
            order1.lock().await.push(2);
        });

        // Small delay to ensure task1 gets the lock first
        tokio::time::sleep(Duration::from_millis(10)).await;

        let locks2 = locks.clone();
        let order2 = order.clone();
        let task2 = tokio::spawn(async move {
            let _guard = locks2.lock(conv).await;
            order2.lock().await.push(3);
        });

        let _ = tokio::join!(task1, task2);

        let seq = order.lock().await;
        // task1 should complete (1, 2) before task2 starts (3)
        assert_eq!(*seq, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn try_lock_returns_none_when_held() {
        let locks = ConversationLockMap::new();
        let conv = Uuid::new_v4();

        let _guard = locks.lock(conv).await;
        let result = locks.try_lock(conv).await;
        assert!(result.is_none(), "try_lock should fail when lock is held");
    }

    #[tokio::test]
    async fn try_lock_returns_guard_when_free() {
        let locks = ConversationLockMap::new();
        let conv = Uuid::new_v4();

        let result = locks.try_lock(conv).await;
        assert!(
            result.is_some(),
            "try_lock should succeed when lock is free"
        );
    }

    #[tokio::test]
    async fn remove_cleans_up() {
        let locks = ConversationLockMap::new();
        let conv = Uuid::new_v4();

        // Create an entry
        let _guard = locks.lock(conv).await;
        drop(_guard);

        locks.remove(conv).await;

        // Entry should be gone from the inner map
        let map = locks.locks.read().await;
        assert!(!map.contains_key(&conv));
    }

    #[tokio::test]
    async fn remove_skips_when_guard_held() {
        let locks = ConversationLockMap::new();
        let conv = Uuid::new_v4();

        let _guard = locks.lock(conv).await;

        // remove() should skip because a guard is still held
        locks.remove(conv).await;

        // Entry should still exist
        assert!(locks.locks.read().await.contains_key(&conv));
    }

    #[tokio::test]
    async fn sweep_idle_removes_unused_entries() {
        let locks = ConversationLockMap::new();
        let conv = Uuid::new_v4();

        // Create then release a lock — entry exists but idle
        let guard = locks.lock(conv).await;
        drop(guard);

        // Before sweep: entry exists
        assert!(locks.locks.read().await.contains_key(&conv));

        locks.sweep_idle().await;

        // After sweep: idle entry removed
        assert!(!locks.locks.read().await.contains_key(&conv));
    }

    #[tokio::test]
    async fn sweep_idle_keeps_held_entries() {
        let locks = ConversationLockMap::new();
        let conv = Uuid::new_v4();

        let _guard = locks.lock(conv).await;

        locks.sweep_idle().await;

        // Entry should still exist because guard is held
        assert!(locks.locks.read().await.contains_key(&conv));
    }
}
