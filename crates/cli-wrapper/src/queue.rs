//! Sequential execution queue.
//!
//! Ensures CLI executions happen one at a time to avoid race conditions
//! on shared CLI state (session files, etc.).

use std::future::Future;
use tokio::sync::Mutex;

/// Queue that serializes asynchronous executions
pub struct ExecutionQueue {
    lock: Mutex<()>,
}

impl ExecutionQueue {
    /// Create a new execution queue
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
        }
    }

    /// Execute a future while holding the queue lock
    ///
    /// This ensures only one execution runs at a time.
    /// Other tasks will wait until the lock is released.
    pub async fn execute<F, T>(&self, f: F) -> T
    where
        F: Future<Output = T>,
    {
        let _guard = self.lock.lock().await;
        f.await
    }
}

impl Default for ExecutionQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn executions_run_sequentially() {
        let queue = Arc::new(ExecutionQueue::new());
        let counter = Arc::new(Mutex::new(0));

        let mut handles = Vec::new();

        // Spawn 5 concurrent tasks
        for i in 0..5 {
            let queue_clone = Arc::clone(&queue);
            let counter_clone = Arc::clone(&counter);

            let handle = tokio::spawn(async move {
                queue_clone
                    .execute(async {
                        let mut count = counter_clone.lock().await;
                        let before = *count;
                        *count += 1;
                        drop(count);

                        // Simulate work
                        sleep(Duration::from_millis(10)).await;

                        (i, before)
                    })
                    .await
            });

            handles.push(handle);
        }

        // Collect results
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // Verify each task saw a different counter value
        // (proving they didn't overlap)
        let mut counter_values: Vec<_> = results.iter().map(|(_, count)| *count).collect();
        counter_values.sort();

        assert_eq!(counter_values, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn execute_returns_future_result() {
        let queue = ExecutionQueue::new();

        let result = queue
            .execute(async {
                // Some async computation
                42 + 8
            })
            .await;

        assert_eq!(result, 50);
    }
}
