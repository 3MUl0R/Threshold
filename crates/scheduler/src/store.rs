//! Persistence for scheduled tasks — JSON file load/save.
//!
//! Tasks are stored at `~/.threshold/state/schedules.json` (or a custom path).
//! Uses atomic write (write to .tmp, then rename) to prevent corruption.

use std::path::Path;

use threshold_core::ThresholdError;

use crate::task::ScheduledTask;

/// Load tasks from a JSON file.
///
/// Returns an empty `Vec` if the file doesn't exist.
/// Returns an error if the file exists but can't be read or parsed.
pub async fn load_tasks(store_path: &Path) -> Result<Vec<ScheduledTask>, ThresholdError> {
    let data = match tokio::fs::read_to_string(store_path).await {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(ThresholdError::IoError {
                path: store_path.display().to_string(),
                message: e.to_string(),
            });
        }
    };

    let tasks: Vec<ScheduledTask> =
        serde_json::from_str(&data).map_err(|e| ThresholdError::IoError {
            path: store_path.display().to_string(),
            message: format!("JSON parse error: {}", e),
        })?;

    Ok(tasks)
}

/// Save tasks to a JSON file using atomic write.
///
/// Writes to a `.tmp` file first, then renames to the target path.
/// Creates parent directories if needed.
pub async fn save_tasks(store_path: &Path, tasks: &[ScheduledTask]) -> Result<(), ThresholdError> {
    if let Some(parent) = store_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ThresholdError::IoError {
                path: parent.display().to_string(),
                message: e.to_string(),
            })?;
    }

    let data = serde_json::to_string_pretty(tasks).map_err(|e| ThresholdError::IoError {
        path: store_path.display().to_string(),
        message: format!("JSON serialize error: {}", e),
    })?;

    // Atomic write: write to .tmp, then rename
    let tmp_path = store_path.with_extension("json.tmp");
    tokio::fs::write(&tmp_path, &data)
        .await
        .map_err(|e| ThresholdError::IoError {
            path: tmp_path.display().to_string(),
            message: e.to_string(),
        })?;

    tokio::fs::rename(&tmp_path, store_path)
        .await
        .map_err(|e| ThresholdError::IoError {
            path: store_path.display().to_string(),
            message: format!("atomic rename failed: {}", e),
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{DeliveryTarget, TaskKind};
    use threshold_core::ScheduledAction;

    fn make_test_task(name: &str) -> ScheduledTask {
        let mut task = ScheduledTask::new(
            name.into(),
            "0 0 3 * * * *".into(),
            ScheduledAction::Script {
                command: "echo hello".into(),
                working_dir: None,
            },
        )
        .unwrap();
        task.delivery = DeliveryTarget::AuditLogOnly;
        task
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let tasks = load_tasks(&path).await.unwrap();
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("schedules.json");

        let tasks = vec![make_test_task("task-1"), make_test_task("task-2")];

        save_tasks(&path, &tasks).await.unwrap();

        let loaded = load_tasks(&path).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "task-1");
        assert_eq!(loaded[1].name, "task-2");
        assert_eq!(loaded[0].id, tasks[0].id);
        assert_eq!(loaded[1].id, tasks[1].id);
    }

    #[tokio::test]
    async fn save_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("deep")
            .join("nested")
            .join("schedules.json");

        save_tasks(&path, &[make_test_task("test")]).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn atomic_write_no_tmp_file_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedules.json");
        let tmp_path = dir.path().join("schedules.json.tmp");

        save_tasks(&path, &[make_test_task("test")]).await.unwrap();

        assert!(path.exists());
        assert!(!tmp_path.exists());
    }

    #[tokio::test]
    async fn load_corrupted_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedules.json");

        tokio::fs::write(&path, "this is not valid JSON")
            .await
            .unwrap();

        let result = load_tasks(&path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn backward_compat_missing_new_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedules.json");

        // Write JSON without newer fields (kind, skip_if_running, etc.)
        let json = r#"[{
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "legacy-task",
            "cron_expression": "0 0 3 * * * *",
            "action": {"Script": {"command": "echo hi", "working_dir": null}},
            "enabled": true,
            "created_at": "2025-01-01T00:00:00Z",
            "last_run": null,
            "last_result": null,
            "next_run": null,
            "delivery": "AuditLogOnly"
        }]"#;
        tokio::fs::write(&path, json).await.unwrap();

        let tasks = load_tasks(&path).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "legacy-task");
        assert_eq!(tasks[0].kind, TaskKind::Cron);
        assert!(!tasks[0].skip_if_running);
    }

    #[tokio::test]
    async fn save_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedules.json");

        save_tasks(&path, &[]).await.unwrap();

        let loaded = load_tasks(&path).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn overwrite_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedules.json");

        // First save
        save_tasks(&path, &[make_test_task("first")]).await.unwrap();
        assert_eq!(load_tasks(&path).await.unwrap().len(), 1);

        // Second save overwrites
        save_tasks(&path, &[make_test_task("a"), make_test_task("b")])
            .await
            .unwrap();

        let loaded = load_tasks(&path).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "a");
        assert_eq!(loaded[1].name, "b");
    }
}
