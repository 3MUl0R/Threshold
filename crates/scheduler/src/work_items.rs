//! Work item store — a simple file-backed task list for the heartbeat.
//!
//! The heartbeat reads and updates work items during its cycles.
//! Work items can also be created by users via Discord or CLI.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use threshold_core::ThresholdError;

/// A single work item for the heartbeat agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: Uuid,
    pub description: String,
    pub status: WorkItemStatus,
    pub priority: u32,
    pub project: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub notes: Option<String>,
}

/// Status of a work item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkItemStatus {
    Pending,
    InProgress,
    Completed,
    Blocked { reason: String },
}

/// A file-backed store for work items.
///
/// Persists to `~/.threshold/state/tasks.json` by default.
pub struct TaskStore {
    path: PathBuf,
    items: Vec<WorkItem>,
}

impl TaskStore {
    /// Load the task store from disk.
    ///
    /// If the file doesn't exist, returns an empty store.
    pub async fn load(path: &Path) -> Result<Self, ThresholdError> {
        let items = match tokio::fs::read_to_string(path).await {
            Ok(data) => serde_json::from_str(&data).map_err(|e| ThresholdError::IoError {
                path: path.display().to_string(),
                message: format!("JSON parse error: {}", e),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(ThresholdError::IoError {
                    path: path.display().to_string(),
                    message: e.to_string(),
                });
            }
        };

        Ok(Self {
            path: path.to_path_buf(),
            items,
        })
    }

    /// Save the task store to disk.
    ///
    /// Creates parent directories if needed.
    pub async fn save(&self) -> Result<(), ThresholdError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ThresholdError::IoError {
                    path: parent.display().to_string(),
                    message: e.to_string(),
                })?;
        }

        let data = serde_json::to_string_pretty(&self.items).map_err(|e| ThresholdError::IoError {
            path: self.path.display().to_string(),
            message: format!("JSON serialize error: {}", e),
        })?;

        tokio::fs::write(&self.path, data)
            .await
            .map_err(|e| ThresholdError::IoError {
                path: self.path.display().to_string(),
                message: e.to_string(),
            })
    }

    /// Add a new work item with the given description and priority.
    pub fn add(&mut self, description: &str, priority: u32) -> &WorkItem {
        let now = Utc::now();
        let item = WorkItem {
            id: Uuid::new_v4(),
            description: description.to_string(),
            status: WorkItemStatus::Pending,
            priority,
            project: None,
            created_at: now,
            updated_at: now,
            notes: None,
        };
        self.items.push(item);
        self.items.last().unwrap()
    }

    /// Update the status of a work item by ID.
    pub fn update_status(
        &mut self,
        id: &Uuid,
        status: WorkItemStatus,
    ) -> Result<(), ThresholdError> {
        let item = self
            .items
            .iter_mut()
            .find(|i| i.id == *id)
            .ok_or_else(|| ThresholdError::NotFound {
                message: format!("work item {}", id),
            })?;
        item.status = status;
        item.updated_at = Utc::now();
        Ok(())
    }

    /// List all pending work items, sorted by priority (highest first).
    pub fn list_pending(&self) -> Vec<&WorkItem> {
        let mut items: Vec<_> = self
            .items
            .iter()
            .filter(|i| matches!(i.status, WorkItemStatus::Pending))
            .collect();
        items.sort_by(|a, b| b.priority.cmp(&a.priority));
        items
    }

    /// List all work items, sorted by priority (highest first).
    pub fn list_all(&self) -> Vec<&WorkItem> {
        let mut items: Vec<_> = self.items.iter().collect();
        items.sort_by(|a, b| b.priority.cmp(&a.priority));
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_item_serde_round_trip() {
        let item = WorkItem {
            id: Uuid::new_v4(),
            description: "Fix the bug".into(),
            status: WorkItemStatus::Pending,
            priority: 3,
            project: Some("threshold".into()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            notes: Some("Found in module X".into()),
        };

        let json = serde_json::to_string(&item).unwrap();
        let restored: WorkItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item.id, restored.id);
        assert_eq!(item.description, restored.description);
        assert_eq!(item.priority, restored.priority);
    }

    #[test]
    fn work_item_status_serde_round_trip() {
        let statuses = vec![
            WorkItemStatus::Pending,
            WorkItemStatus::InProgress,
            WorkItemStatus::Completed,
            WorkItemStatus::Blocked {
                reason: "Waiting on API".into(),
            },
        ];

        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let restored: WorkItemStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, restored);
        }
    }

    #[tokio::test]
    async fn task_store_empty_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let store = TaskStore::load(&path).await.unwrap();
        assert!(store.list_all().is_empty());
    }

    #[tokio::test]
    async fn task_store_add_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut store = TaskStore::load(&path).await.unwrap();

        store.add("Task A", 1);
        store.add("Task B", 3);
        store.add("Task C", 2);

        let all = store.list_all();
        assert_eq!(all.len(), 3);
        // Sorted by priority descending
        assert_eq!(all[0].description, "Task B");
        assert_eq!(all[1].description, "Task C");
        assert_eq!(all[2].description, "Task A");

        let pending = store.list_pending();
        assert_eq!(pending.len(), 3);
    }

    #[tokio::test]
    async fn task_store_update_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut store = TaskStore::load(&path).await.unwrap();

        let item = store.add("Task A", 1);
        let id = item.id;

        store
            .update_status(&id, WorkItemStatus::InProgress)
            .unwrap();

        // Should no longer appear in pending
        assert!(store.list_pending().is_empty());
        assert_eq!(store.list_all().len(), 1);
        assert_eq!(store.list_all()[0].status, WorkItemStatus::InProgress);
    }

    #[tokio::test]
    async fn task_store_update_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut store = TaskStore::load(&path).await.unwrap();

        let result = store.update_status(&Uuid::new_v4(), WorkItemStatus::Completed);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn task_store_save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("tasks.json");

        // Create and save
        {
            let mut store = TaskStore::load(&path).await.unwrap();
            store.add("Task A", 3);
            store.add("Task B", 1);
            store.save().await.unwrap();
        }

        // Load and verify
        {
            let store = TaskStore::load(&path).await.unwrap();
            let items = store.list_all();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].description, "Task A"); // priority 3 first
            assert_eq!(items[1].description, "Task B"); // priority 1 second
        }
    }

    #[tokio::test]
    async fn task_store_blocked_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut store = TaskStore::load(&path).await.unwrap();

        let item = store.add("Blocked task", 5);
        let id = item.id;

        store
            .update_status(
                &id,
                WorkItemStatus::Blocked {
                    reason: "Waiting on dependency".into(),
                },
            )
            .unwrap();

        let all = store.list_all();
        assert_eq!(all.len(), 1);
        match &all[0].status {
            WorkItemStatus::Blocked { reason } => {
                assert_eq!(reason, "Waiting on dependency");
            }
            other => panic!("Expected Blocked, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn task_store_completed_not_in_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut store = TaskStore::load(&path).await.unwrap();

        let a = store.add("Task A", 1);
        let a_id = a.id;
        store.add("Task B", 2);

        store
            .update_status(&a_id, WorkItemStatus::Completed)
            .unwrap();

        let pending = store.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].description, "Task B");
    }
}
