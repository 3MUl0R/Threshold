//! Heartbeat features — handoff notes, prompt building, config conversion.
//!
//! A heartbeat is a scheduled task with `ResumeConversation` action,
//! skip-if-running guard, and handoff notes for continuity between runs.

use std::path::Path;

use threshold_core::config::HeartbeatConfig;
use threshold_core::{ConversationId, ScheduledAction, ThresholdError};

use crate::task::{DeliveryTarget, ScheduledTask, TaskKind};

/// Build the heartbeat prompt combining instructions and handoff notes.
///
/// The prompt has three sections:
/// 1. Heartbeat Instructions — from the instruction file
/// 2. Notes From Previous Heartbeat — if any exist
/// 3. Your Job Right Now — standard instructions for the agent
pub fn build_heartbeat_prompt(instructions: &str, handoff_notes: &Option<String>) -> String {
    let mut prompt = String::new();

    prompt.push_str("## Heartbeat Instructions\n\n");
    prompt.push_str(instructions);
    prompt.push_str("\n\n");

    if let Some(notes) = handoff_notes {
        prompt.push_str("## Notes From Previous Heartbeat\n\n");
        prompt.push_str(notes);
        prompt.push_str("\n\n");
    }

    prompt.push_str(
        "## Your Job Right Now\n\n\
         Review the instructions above. Decide what to work on. Execute any \
         needed actions. When you're done (or need to pause), write handoff \
         notes explaining what you did and what should happen next.\n\n\
         Format your handoff notes in a section starting with `## Handoff Notes`.",
    );

    prompt
}

/// Extract handoff notes from Claude's response.
///
/// Looks for a `## Handoff Notes` section and returns everything after it.
pub fn extract_handoff_notes(response: &str) -> Option<String> {
    let marker = "## Handoff Notes";
    if let Some(idx) = response.find(marker) {
        let notes = &response[idx + marker.len()..];
        let notes = notes.trim();
        if !notes.is_empty() {
            return Some(notes.to_string());
        }
    }
    None
}

/// Load handoff notes from a file.
///
/// Returns `None` if the file doesn't exist or can't be read.
pub async fn load_handoff_notes(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

/// Save handoff notes to a file, creating parent directories as needed.
pub async fn save_handoff_notes(path: &Path, notes: &str) -> Result<(), ThresholdError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ThresholdError::IoError {
                path: parent.display().to_string(),
                message: e.to_string(),
            })?;
    }
    tokio::fs::write(path, notes)
        .await
        .map_err(|e| ThresholdError::IoError {
            path: path.display().to_string(),
            message: e.to_string(),
        })
}

/// Convert a `HeartbeatConfig` into a `ScheduledTask`.
///
/// This is called at startup to create the heartbeat task from config.
/// The instruction file is resolved relative to `data_dir`.
pub fn heartbeat_task_from_config(
    config: &HeartbeatConfig,
    data_dir: &Path,
) -> Result<ScheduledTask, ThresholdError> {
    // Convert interval_minutes to cron expression
    let interval = config.interval_minutes.unwrap_or(30);
    let cron_expr = if interval >= 60 && interval % 60 == 0 {
        let hours = interval / 60;
        format!("0 0 */{} * * *", hours)
    } else {
        format!("0 */{} * * * *", interval)
    };

    // Resolve conversation_id — use existing or create new
    let conversation_id = config
        .conversation_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(ConversationId)
        .unwrap_or_else(ConversationId::new);

    // The prompt is a placeholder — at fire time, build_heartbeat_prompt()
    // dynamically assembles the real prompt from instruction file + handoff notes.
    let action = ScheduledAction::ResumeConversation {
        conversation_id,
        prompt: String::new(), // replaced at fire time
    };

    let mut task = ScheduledTask::new("heartbeat".into(), cron_expr, action)
        .map_err(|e| ThresholdError::InvalidInput { message: e })?;

    task.kind = TaskKind::Heartbeat;
    task.skip_if_running = config.skip_if_running.unwrap_or(true);

    // Resolve handoff notes path
    task.handoff_notes_path = Some(
        config
            .handoff_notes_path
            .as_ref()
            .map(|p| threshold_core::resolve_path(p, data_dir))
            .unwrap_or_else(|| data_dir.join("state").join("heartbeat-notes.md")),
    );

    // Resolve notification channel for delivery
    if let Some(channel_id) = config.notification_channel_id {
        task.delivery = DeliveryTarget::DiscordChannel { channel_id };
    }

    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_prompt_without_notes() {
        let prompt = build_heartbeat_prompt("Do the thing.", &None);
        assert!(prompt.contains("## Heartbeat Instructions"));
        assert!(prompt.contains("Do the thing."));
        assert!(prompt.contains("## Your Job Right Now"));
        assert!(!prompt.contains("## Notes From Previous Heartbeat"));
    }

    #[test]
    fn build_prompt_with_notes() {
        let notes = Some("I finished task A. Next: task B.".to_string());
        let prompt = build_heartbeat_prompt("Do the thing.", &notes);
        assert!(prompt.contains("## Heartbeat Instructions"));
        assert!(prompt.contains("Do the thing."));
        assert!(prompt.contains("## Notes From Previous Heartbeat"));
        assert!(prompt.contains("I finished task A. Next: task B."));
        assert!(prompt.contains("## Your Job Right Now"));
    }

    #[test]
    fn build_prompt_section_order() {
        let notes = Some("previous notes".to_string());
        let prompt = build_heartbeat_prompt("instructions", &notes);

        let instructions_pos = prompt.find("## Heartbeat Instructions").unwrap();
        let notes_pos = prompt.find("## Notes From Previous Heartbeat").unwrap();
        let job_pos = prompt.find("## Your Job Right Now").unwrap();

        assert!(instructions_pos < notes_pos);
        assert!(notes_pos < job_pos);
    }

    #[test]
    fn build_prompt_includes_handoff_format_instructions() {
        let prompt = build_heartbeat_prompt("test", &None);
        assert!(prompt.contains("## Handoff Notes"));
    }

    #[test]
    fn extract_handoff_notes_present() {
        let response = "I did some work.\n\n## Handoff Notes\n\nFinished task A.\nStarting B next.";
        let notes = extract_handoff_notes(response);
        assert!(notes.is_some());
        let notes = notes.unwrap();
        assert!(notes.contains("Finished task A."));
        assert!(notes.contains("Starting B next."));
    }

    #[test]
    fn extract_handoff_notes_missing() {
        let response = "I did some work but didn't write handoff notes.";
        assert!(extract_handoff_notes(response).is_none());
    }

    #[test]
    fn extract_handoff_notes_empty_section() {
        let response = "Some work.\n\n## Handoff Notes\n\n";
        assert!(extract_handoff_notes(response).is_none());
    }

    #[test]
    fn extract_handoff_notes_only_whitespace() {
        let response = "Some work.\n\n## Handoff Notes\n   \n  \n";
        assert!(extract_handoff_notes(response).is_none());
    }

    #[tokio::test]
    async fn handoff_notes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("notes.md");

        let notes = "Finished task A.\nStarting task B.";
        save_handoff_notes(&path, notes).await.unwrap();

        let loaded = load_handoff_notes(&path).await;
        assert_eq!(loaded.as_deref(), Some(notes));
    }

    #[tokio::test]
    async fn load_handoff_notes_missing_file() {
        let result = load_handoff_notes(Path::new("/nonexistent/notes.md")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn save_handoff_notes_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("nested").join("notes.md");

        save_handoff_notes(&path, "test notes").await.unwrap();
        assert!(path.exists());
    }

    #[test]
    fn heartbeat_task_from_config_defaults() {
        let config = HeartbeatConfig {
            enabled: true,
            interval_minutes: None,
            instruction_file: None,
            handoff_notes_path: None,
            conversation_id: None,
            skip_if_running: None,
            notification_channel_id: None,
        };

        let data_dir = PathBuf::from("/tmp/threshold");
        let task = heartbeat_task_from_config(&config, &data_dir).unwrap();

        assert_eq!(task.name, "heartbeat");
        assert_eq!(task.kind, TaskKind::Heartbeat);
        assert!(task.skip_if_running); // default true
        assert_eq!(
            task.handoff_notes_path,
            Some(PathBuf::from("/tmp/threshold/state/heartbeat-notes.md"))
        );
        // Default 30min → every 30 minutes cron
        assert!(task.cron_expression.contains("*/30"));
    }

    #[test]
    fn heartbeat_task_from_config_custom_interval() {
        let config = HeartbeatConfig {
            enabled: true,
            interval_minutes: Some(60),
            instruction_file: None,
            handoff_notes_path: None,
            conversation_id: None,
            skip_if_running: Some(false),
            notification_channel_id: Some(123456),
        };

        let data_dir = PathBuf::from("/tmp/threshold");
        let task = heartbeat_task_from_config(&config, &data_dir).unwrap();

        // 60min → hourly
        assert!(task.cron_expression.contains("*/1"), "cron: {}", task.cron_expression);
        assert!(!task.skip_if_running);
        match &task.delivery {
            DeliveryTarget::DiscordChannel { channel_id } => assert_eq!(*channel_id, 123456),
            other => panic!("Expected DiscordChannel, got {:?}", other),
        }
    }

    #[test]
    fn heartbeat_task_from_config_with_conversation_id() {
        let conv_id = uuid::Uuid::new_v4();
        let config = HeartbeatConfig {
            enabled: true,
            interval_minutes: Some(15),
            instruction_file: None,
            handoff_notes_path: Some("~/.threshold/state/my-notes.md".into()),
            conversation_id: Some(conv_id.to_string()),
            skip_if_running: None,
            notification_channel_id: None,
        };

        let data_dir = PathBuf::from("/tmp/threshold");
        let task = heartbeat_task_from_config(&config, &data_dir).unwrap();

        // Check the conversation_id was parsed
        if let ScheduledAction::ResumeConversation {
            conversation_id, ..
        } = &task.action
        {
            assert_eq!(conversation_id.0, conv_id);
        } else {
            panic!("Expected ResumeConversation action");
        }

        // 15min → every 15 minutes cron
        assert!(task.cron_expression.contains("*/15"));
    }

    #[test]
    fn heartbeat_task_from_config_2_hour_interval() {
        let config = HeartbeatConfig {
            enabled: true,
            interval_minutes: Some(120),
            instruction_file: None,
            handoff_notes_path: None,
            conversation_id: None,
            skip_if_running: None,
            notification_channel_id: None,
        };

        let data_dir = PathBuf::from("/tmp/threshold");
        let task = heartbeat_task_from_config(&config, &data_dir).unwrap();

        // 120min = 2 hours → hourly cron with step
        assert!(task.cron_expression.contains("*/2"), "cron: {}", task.cron_expression);
        assert!(task.cron_expression.contains("0 0"), "cron: {}", task.cron_expression);
    }
}
