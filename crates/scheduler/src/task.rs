//! Scheduled task types — the data model for the unified scheduler.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use threshold_core::{ConversationId, PortalId, ScheduledAction};
use uuid::Uuid;

use crate::cron_utils;

/// A fully-specified scheduled task (persisted to schedules.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: Uuid,
    pub name: String,
    pub cron_expression: String,
    pub action: ScheduledAction,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_result: Option<TaskRunResult>,
    pub next_run: Option<DateTime<Utc>>,

    /// What kind of task this is — used for identity, not behavior.
    #[serde(default)]
    pub kind: TaskKind,

    /// Where to deliver results for standalone tasks.
    pub delivery: DeliveryTarget,

    /// Conversation context (for agent-created tasks).
    #[serde(default)]
    pub conversation_id: Option<ConversationId>,
    #[serde(default)]
    pub portal_id: Option<PortalId>,
    #[serde(default)]
    pub created_by_agent: bool,

    /// If true, skip this firing when the previous execution is still running.
    #[serde(default)]
    pub skip_if_running: bool,
    /// Path to handoff notes file (heartbeat tasks).
    #[serde(default)]
    pub handoff_notes_path: Option<PathBuf>,
}

/// Explicit task identity — distinguishes heartbeats from user-created cron jobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum TaskKind {
    /// User-created cron job (the default).
    #[default]
    Cron,
    /// The heartbeat task — created from config, has special semantics.
    Heartbeat,
}

/// Where to deliver task results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeliveryTarget {
    /// Send results to a Discord channel.
    DiscordChannel { channel_id: u64 },
    /// Send results as a DM to a user.
    DiscordDm { user_id: u64 },
    /// Log only (conversation-attached tasks deliver via portal).
    AuditLogOnly,
}

/// Result of a single task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunResult {
    pub timestamp: DateTime<Utc>,
    pub success: bool,
    pub summary: String,
    pub duration_ms: u64,
}

impl ScheduledTask {
    /// Create a new scheduled task with the given name, cron expression, and action.
    ///
    /// Validates the cron expression and computes the initial `next_run`.
    /// Defaults to enabled, `TaskKind::Cron`, and `DeliveryTarget::AuditLogOnly`.
    pub fn new(
        name: String,
        cron_expression: String,
        action: ScheduledAction,
    ) -> Result<Self, String> {
        cron_utils::validate_cron(&cron_expression)?;

        let next_run = cron_utils::compute_next_run(&cron_expression);

        Ok(Self {
            id: Uuid::new_v4(),
            name,
            cron_expression,
            action,
            enabled: true,
            created_at: Utc::now(),
            last_run: None,
            last_result: None,
            next_run,
            kind: TaskKind::Cron,
            delivery: DeliveryTarget::AuditLogOnly,
            conversation_id: None,
            portal_id: None,
            created_by_agent: false,
            skip_if_running: false,
            handoff_notes_path: None,
        })
    }

    /// Recompute `next_run` from the current time using the stored cron expression.
    pub fn refresh_next_run(&mut self) {
        self.next_run = cron_utils::compute_next_run(&self.cron_expression);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_task_new_sets_fields() {
        let task = ScheduledTask::new(
            "test-task".into(),
            "0 0 3 * * * *".into(),
            ScheduledAction::Script {
                command: "echo hello".into(),
                working_dir: None,
            },
        )
        .unwrap();

        assert_eq!(task.name, "test-task");
        assert_eq!(task.cron_expression, "0 0 3 * * * *");
        assert!(task.enabled);
        assert!(task.next_run.is_some());
        assert_eq!(task.kind, TaskKind::Cron);
        assert!(task.last_run.is_none());
        assert!(task.last_result.is_none());
        assert!(!task.skip_if_running);
    }

    #[test]
    fn scheduled_task_new_rejects_invalid_cron() {
        let result = ScheduledTask::new(
            "bad".into(),
            "not valid".into(),
            ScheduledAction::Script {
                command: "echo".into(),
                working_dir: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn scheduled_task_serde_round_trip() {
        let task = ScheduledTask::new(
            "nightly-tests".into(),
            "0 0 3 * * * *".into(),
            ScheduledAction::NewConversation {
                prompt: "Run the test suite".into(),
                model: Some("sonnet".into()),
            },
        )
        .unwrap();

        let json = serde_json::to_string(&task).unwrap();
        let restored: ScheduledTask = serde_json::from_str(&json).unwrap();
        assert_eq!(task.id, restored.id);
        assert_eq!(task.name, restored.name);
        assert_eq!(task.cron_expression, restored.cron_expression);
        assert_eq!(task.enabled, restored.enabled);
        assert_eq!(task.kind, restored.kind);
    }

    #[test]
    fn scheduled_task_refresh_next_run() {
        let mut task = ScheduledTask::new(
            "test".into(),
            "0 * * * * * *".into(), // every minute
            ScheduledAction::Script {
                command: "echo".into(),
                working_dir: None,
            },
        )
        .unwrap();

        let original_next = task.next_run;
        // Simulate that we've passed the next_run point
        task.next_run = Some(Utc::now() - chrono::Duration::hours(1));
        task.refresh_next_run();
        // Should have been updated to a future time
        assert!(task.next_run.unwrap() > Utc::now() - chrono::Duration::seconds(1));
        // The original should also have been in the future
        assert!(original_next.is_some());
    }

    #[test]
    fn task_kind_default_is_cron() {
        assert_eq!(TaskKind::default(), TaskKind::Cron);
    }

    #[test]
    fn task_kind_serde_round_trip() {
        for kind in [TaskKind::Cron, TaskKind::Heartbeat] {
            let json = serde_json::to_string(&kind).unwrap();
            let restored: TaskKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, restored);
        }
    }

    #[test]
    fn delivery_target_serde_round_trip() {
        let targets = vec![
            DeliveryTarget::DiscordChannel { channel_id: 123 },
            DeliveryTarget::DiscordDm { user_id: 456 },
            DeliveryTarget::AuditLogOnly,
        ];

        for target in targets {
            let json = serde_json::to_string(&target).unwrap();
            let restored: DeliveryTarget = serde_json::from_str(&json).unwrap();
            // Check round-trip by re-serializing
            assert_eq!(json, serde_json::to_string(&restored).unwrap());
        }
    }

    #[test]
    fn task_run_result_serde_round_trip() {
        let result = TaskRunResult {
            timestamp: Utc::now(),
            success: true,
            summary: "All tests passed".into(),
            duration_ms: 1234,
        };

        let json = serde_json::to_string(&result).unwrap();
        let restored: TaskRunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.success, restored.success);
        assert_eq!(result.summary, restored.summary);
        assert_eq!(result.duration_ms, restored.duration_ms);
    }

    #[test]
    fn scheduled_task_with_heartbeat_fields() {
        let mut task = ScheduledTask::new(
            "heartbeat".into(),
            "0 */30 * * * * *".into(),
            ScheduledAction::ResumeConversation {
                conversation_id: ConversationId::new(),
                prompt: "Continue working".into(),
            },
        )
        .unwrap();

        task.kind = TaskKind::Heartbeat;
        task.skip_if_running = true;
        task.handoff_notes_path = Some(PathBuf::from("/tmp/notes.md"));

        let json = serde_json::to_string(&task).unwrap();
        let restored: ScheduledTask = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.kind, TaskKind::Heartbeat);
        assert!(restored.skip_if_running);
        assert_eq!(
            restored.handoff_notes_path,
            Some(PathBuf::from("/tmp/notes.md"))
        );
    }

    #[test]
    fn scheduled_task_backward_compat_missing_new_fields() {
        // Simulate a JSON from an older version without newer fields
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "old-task",
            "cron_expression": "0 0 3 * * * *",
            "action": {"Script": {"command": "echo hi", "working_dir": null}},
            "enabled": true,
            "created_at": "2025-01-01T00:00:00Z",
            "last_run": null,
            "last_result": null,
            "next_run": null,
            "delivery": "AuditLogOnly"
        }"#;

        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(task.kind, TaskKind::Cron); // default
        assert!(!task.skip_if_running); // default
        assert!(task.handoff_notes_path.is_none()); // default
        assert!(!task.created_by_agent); // default
    }
}
