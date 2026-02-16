//! JSONL audit trail for append-only event logging.
//!
//! # Format
//!
//! Each entry is a single JSON object followed by a newline (JSONL format).
//! The audit trail supports two modes:
//! - Raw: Caller provides full JSON object via `append_raw()`
//! - Wrapped: Automatic timestamp/type envelope via `append_event()`
//!
//! # Durability Model
//!
//! Audit entries are flushed to disk but NOT fsynced. This provides
//! best-effort durability with good performance. On power loss or OS crash,
//! recently written entries may be lost from the OS page cache.
//!
//! This is acceptable for audit logs (debugging, accountability) but would
//! NOT be appropriate for write-ahead logs (transaction recovery).
//!
//! # Concurrency
//!
//! Multiple concurrent appends are serialized via a per-file mutex. This
//! prevents corruption when multiple tasks write to the same file.
//!
//! # Security
//!
//! On Unix systems, audit files are created with permissions 0600 (owner
//! read/write only) to prevent unauthorized access to sensitive events.

use chrono::Utc;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Maximum size for a single audit entry (64KB).
const MAX_ENTRY_SIZE: usize = 65536;

/// Append-only JSONL audit trail.
pub struct AuditTrail {
    path: PathBuf,
    writer: Arc<Mutex<()>>, // Serializes writes to this file
}

impl AuditTrail {
    /// Create a new audit trail at the given path.
    ///
    /// The file will be created on first write with permissions 0600 (Unix only).
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            writer: Arc::new(Mutex::new(())),
        }
    }

    /// Append a raw JSONL entry (caller provides full JSON object).
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::AuditWrite` if:
    /// - Entry exceeds 64KB when serialized
    /// - File I/O fails
    /// - Serialization fails
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use threshold_core::audit::AuditTrail;
    /// # use serde::Serialize;
    /// # tokio_test::block_on(async {
    /// #[derive(Serialize)]
    /// struct ToolExecution {
    ///     tool: String,
    ///     args: Vec<String>,
    ///     exit_code: i32,
    /// }
    ///
    /// let trail = AuditTrail::new("audit.jsonl".into());
    /// let event = ToolExecution {
    ///     tool: "bash".into(),
    ///     args: vec!["-c".into(), "echo hello".into()],
    ///     exit_code: 0,
    /// };
    ///
    /// trail.append_raw(&event).await?;
    /// # Ok::<(), threshold_core::ThresholdError>(())
    /// # });
    /// ```
    pub async fn append_raw<T: Serialize>(&self, entry: &T) -> crate::Result<()> {
        // Serialize first (outside lock to minimize critical section)
        let mut json = serde_json::to_string(entry)?;
        json.push('\n');

        // Enforce max entry size
        if json.len() > MAX_ENTRY_SIZE {
            return Err(crate::ThresholdError::AuditWrite(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("entry too large: {} bytes", json.len()),
            )));
        }

        // Acquire lock for this file
        let _guard = self.writer.lock().await;

        // Create parent directories
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Set restrictive permissions on first write (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if !self.path.exists() {
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false) // Don't truncate - we're about to append
                    .write(true)
                    .open(&self.path)
                    .await?;
                let mut perms = tokio::fs::metadata(&self.path).await?.permissions();
                perms.set_mode(0o600); // rw------- (owner only)
                tokio::fs::set_permissions(&self.path, perms).await?;
            }
        }

        // Append to file
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(crate::ThresholdError::AuditWrite)?;

        file.write_all(json.as_bytes())
            .await
            .map_err(crate::ThresholdError::AuditWrite)?;

        // Flush (but don't fsync - performance tradeoff)
        file.flush()
            .await
            .map_err(crate::ThresholdError::AuditWrite)?;

        Ok(())
    }

    /// Append with timestamp + type envelope.
    ///
    /// This wraps the data in a standard envelope with ISO 8601 timestamp
    /// and event type:
    ///
    /// ```json
    /// {"ts": "2024-01-15T10:30:45Z", "type": "tool_execution", "data": {...}}
    /// ```
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::AuditWrite` if serialization or I/O fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use threshold_core::audit::AuditTrail;
    /// # use serde::Serialize;
    /// # tokio_test::block_on(async {
    /// #[derive(Serialize)]
    /// struct UserAction {
    ///     action: String,
    ///     user_id: u64,
    /// }
    ///
    /// let trail = AuditTrail::new("audit.jsonl".into());
    /// let action = UserAction {
    ///     action: "login".into(),
    ///     user_id: 42,
    /// };
    ///
    /// trail.append_event("user_action", &action).await?;
    /// # Ok::<(), threshold_core::ThresholdError>(())
    /// # });
    /// ```
    pub async fn append_event<T: Serialize>(
        &self,
        event_type: &str,
        data: &T,
    ) -> crate::Result<()> {
        #[derive(Serialize)]
        struct Envelope<'a, T> {
            ts: String, // ISO 8601
            #[serde(rename = "type")]
            event_type: &'a str,
            data: &'a T,
        }

        let envelope = Envelope {
            ts: Utc::now().to_rfc3339(),
            event_type,
            data,
        };

        self.append_raw(&envelope).await
    }

    /// Read last N entries from the audit trail.
    ///
    /// Returns up to `n` entries. If the file has fewer than `n` entries,
    /// returns all available entries.
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::AuditRead` if file I/O fails.
    ///
    /// Malformed JSON lines are logged as warnings and skipped.
    pub async fn read_tail(&self, n: usize) -> crate::Result<Vec<serde_json::Value>> {
        if n == 0 {
            return Ok(Vec::new());
        }

        // Read entire file if it exists
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let content = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(crate::ThresholdError::AuditRead)?;

        let mut entries = Vec::new();
        for (line_num, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str(line) {
                Ok(value) => entries.push(value),
                Err(e) => {
                    tracing::warn!(
                        line_number = line_num + 1,
                        error = %e,
                        "malformed JSON in audit trail, skipping line"
                    );
                }
            }
        }

        // Return last N entries
        if entries.len() <= n {
            Ok(entries)
        } else {
            let skip_count = entries.len() - n;
            Ok(entries.into_iter().skip(skip_count).collect())
        }
    }

    /// Read all entries from the audit trail.
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::AuditRead` if file I/O fails.
    ///
    /// Malformed JSON lines are logged as warnings and skipped.
    pub async fn read_all(&self) -> crate::Result<Vec<serde_json::Value>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let content = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(crate::ThresholdError::AuditRead)?;

        let mut entries = Vec::new();
        for (line_num, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str(line) {
                Ok(value) => entries.push(value),
                Err(e) => {
                    tracing::warn!(
                        line_number = line_num + 1,
                        error = %e,
                        "malformed JSON in audit trail, skipping line"
                    );
                }
            }
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serial_test::serial;
    use tempfile::tempdir;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestEvent {
        message: String,
        value: i32,
    }

    #[tokio::test]
    async fn new_creates_audit_trail() {
        let trail = AuditTrail::new("test.jsonl".into());
        assert_eq!(trail.path, PathBuf::from("test.jsonl"));
    }

    #[tokio::test]
    #[serial]
    async fn append_raw_creates_file_and_writes_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path.clone());

        let event = TestEvent {
            message: "test".into(),
            value: 42,
        };

        trail.append_raw(&event).await.unwrap();

        // Verify file exists
        assert!(path.exists());

        // Verify content
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let parsed: TestEvent = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed, event);
    }

    #[tokio::test]
    #[serial]
    async fn append_raw_appends_multiple_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path.clone());

        let event1 = TestEvent {
            message: "first".into(),
            value: 1,
        };
        let event2 = TestEvent {
            message: "second".into(),
            value: 2,
        };

        trail.append_raw(&event1).await.unwrap();
        trail.append_raw(&event2).await.unwrap();

        // Verify both entries exist
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed1: TestEvent = serde_json::from_str(lines[0]).unwrap();
        let parsed2: TestEvent = serde_json::from_str(lines[1]).unwrap();

        assert_eq!(parsed1, event1);
        assert_eq!(parsed2, event2);
    }

    #[tokio::test]
    #[serial]
    async fn append_event_adds_timestamp_and_type() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path.clone());

        let event = TestEvent {
            message: "wrapped".into(),
            value: 99,
        };

        trail.append_event("test_event", &event).await.unwrap();

        // Verify envelope structure
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let envelope: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert!(envelope.get("ts").is_some());
        assert_eq!(envelope["type"], "test_event");
        assert_eq!(envelope["data"]["message"], "wrapped");
        assert_eq!(envelope["data"]["value"], 99);
    }

    #[tokio::test]
    #[serial]
    async fn append_raw_rejects_oversized_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path);

        // Create entry larger than 64KB
        let huge_string = "x".repeat(70000);
        let event = TestEvent {
            message: huge_string,
            value: 1,
        };

        let result = trail.append_raw(&event).await;
        assert!(result.is_err());

        match result.unwrap_err() {
            crate::ThresholdError::AuditWrite(e) => {
                assert!(e.to_string().contains("too large"));
            }
            _ => panic!("expected AuditWrite error"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_appends_do_not_corrupt_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = Arc::new(AuditTrail::new(path.clone()));

        // Spawn 10 concurrent tasks
        let mut handles = Vec::new();
        for i in 0..10 {
            let trail_clone = Arc::clone(&trail);
            let handle = tokio::spawn(async move {
                let event = TestEvent {
                    message: format!("concurrent-{}", i),
                    value: i,
                };
                trail_clone.append_raw(&event).await.unwrap();
            });
            handles.push(handle);
        }

        // Wait for all tasks
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify all 10 entries exist and are valid JSON
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 10);

        for line in lines {
            let parsed: TestEvent = serde_json::from_str(line).unwrap();
            assert!(parsed.message.starts_with("concurrent-"));
        }
    }

    #[tokio::test]
    #[serial]
    async fn read_tail_returns_last_n_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path);

        // Write 5 entries
        for i in 0..5 {
            let event = TestEvent {
                message: format!("entry-{}", i),
                value: i,
            };
            trail.append_raw(&event).await.unwrap();
        }

        // Read last 3
        let tail = trail.read_tail(3).await.unwrap();
        assert_eq!(tail.len(), 3);

        // Verify they are entries 2, 3, 4
        assert_eq!(tail[0]["message"], "entry-2");
        assert_eq!(tail[1]["message"], "entry-3");
        assert_eq!(tail[2]["message"], "entry-4");
    }

    #[tokio::test]
    #[serial]
    async fn read_tail_returns_all_if_fewer_than_n() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path);

        // Write 3 entries
        for i in 0..3 {
            let event = TestEvent {
                message: format!("entry-{}", i),
                value: i,
            };
            trail.append_raw(&event).await.unwrap();
        }

        // Request 5, should get 3
        let tail = trail.read_tail(5).await.unwrap();
        assert_eq!(tail.len(), 3);
    }

    #[tokio::test]
    #[serial]
    async fn read_tail_zero_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path);

        trail
            .append_raw(&TestEvent {
                message: "test".into(),
                value: 1,
            })
            .await
            .unwrap();

        let tail = trail.read_tail(0).await.unwrap();
        assert_eq!(tail.len(), 0);
    }

    #[tokio::test]
    #[serial]
    async fn read_all_returns_all_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path);

        // Write 10 entries
        for i in 0..10 {
            let event = TestEvent {
                message: format!("entry-{}", i),
                value: i,
            };
            trail.append_raw(&event).await.unwrap();
        }

        let all = trail.read_all().await.unwrap();
        assert_eq!(all.len(), 10);

        // Verify order
        for (i, entry) in all.iter().enumerate() {
            assert_eq!(entry["message"], format!("entry-{}", i));
        }
    }

    #[tokio::test]
    #[serial]
    async fn read_all_returns_empty_if_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.jsonl");
        let trail = AuditTrail::new(path);

        let all = trail.read_all().await.unwrap();
        assert_eq!(all.len(), 0);
    }

    #[tokio::test]
    #[serial]
    async fn read_tail_skips_malformed_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

        // Manually create file with mixed valid/invalid JSON
        let content = r#"{"message":"valid1","value":1}
not valid json
{"message":"valid2","value":2}
{"incomplete":
{"message":"valid3","value":3}
"#;
        tokio::fs::write(&path, content).await.unwrap();

        let trail = AuditTrail::new(path);
        let all = trail.read_all().await.unwrap();

        // Should only get 3 valid entries
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["message"], "valid1");
        assert_eq!(all[1]["message"], "valid2");
        assert_eq!(all[2]["message"], "valid3");
    }

    #[tokio::test]
    #[serial]
    async fn read_all_skips_empty_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

        // Manually create file with empty lines
        let content = r#"{"message":"entry1","value":1}

{"message":"entry2","value":2}

{"message":"entry3","value":3}
"#;
        tokio::fs::write(&path, content).await.unwrap();

        let trail = AuditTrail::new(path);
        let all = trail.read_all().await.unwrap();

        assert_eq!(all.len(), 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn audit_file_created_with_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trail = AuditTrail::new(path.clone());

        trail
            .append_raw(&TestEvent {
                message: "test".into(),
                value: 1,
            })
            .await
            .unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        let permissions = metadata.permissions();
        assert_eq!(permissions.mode() & 0o777, 0o600);
    }

    #[tokio::test]
    #[serial]
    async fn append_creates_parent_directories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("audit.jsonl");
        let trail = AuditTrail::new(path.clone());

        trail
            .append_raw(&TestEvent {
                message: "test".into(),
                value: 1,
            })
            .await
            .unwrap();

        assert!(path.exists());
        assert!(path.parent().unwrap().exists());
    }
}
