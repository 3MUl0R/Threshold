//! Audit trail integration - per-conversation JSONL logs.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::PathBuf;
use threshold_cli_wrapper::response::Usage;
use threshold_core::{ConversationId, ConversationMode, PortalId, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Audit events written to per-conversation JSONL files
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ConversationAuditEvent {
    UserMessage {
        portal_id: PortalId,
        portal_type: String,
        content: String,
        timestamp: DateTime<Utc>,
    },
    AssistantMessage {
        content: String,
        usage: Option<Usage>,
        duration_ms: u64,
        timestamp: DateTime<Utc>,
    },
    ModeSwitch {
        portal_id: PortalId,
        from_conversation: Option<ConversationId>,
        to_conversation: ConversationId,
        mode: ConversationMode,
        timestamp: DateTime<Utc>,
    },
    SessionCreated {
        cli_session_id: String,
        model: String,
        agent_id: String,
        timestamp: DateTime<Utc>,
    },
    Error {
        error: String,
        timestamp: DateTime<Utc>,
    },
}

/// Write an audit event to the conversation's JSONL file
pub async fn write_audit_event(
    audit_dir: &PathBuf,
    conversation_id: &ConversationId,
    event: &ConversationAuditEvent,
) -> Result<()> {
    // Create audit directory if needed
    tokio::fs::create_dir_all(audit_dir).await?;

    // Path: ~/.threshold/audit/<conversation_id>.jsonl
    let file_path = audit_dir.join(format!("{}.jsonl", conversation_id.0));

    // Serialize event
    let mut json = serde_json::to_string(event)?;
    json.push('\n');

    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)
        .await?;

    file.write_all(json.as_bytes()).await?;
    file.flush().await?;

    // Set restrictive permissions (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&file_path).await?.permissions();
        perms.set_mode(0o600);
        tokio::fs::set_permissions(&file_path, perms).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn write_audit_event_creates_file() {
        let dir = tempdir().unwrap();
        let audit_dir = dir.path().to_path_buf();
        let conversation_id = ConversationId::new();

        let event = ConversationAuditEvent::UserMessage {
            portal_id: PortalId::new(),
            portal_type: "Discord(123:456)".to_string(),
            content: "test message".to_string(),
            timestamp: Utc::now(),
        };

        write_audit_event(&audit_dir, &conversation_id, &event)
            .await
            .unwrap();

        let file_path = audit_dir.join(format!("{}.jsonl", conversation_id.0));
        assert!(file_path.exists());
    }

    #[tokio::test]
    async fn write_audit_event_appends_to_existing() {
        let dir = tempdir().unwrap();
        let audit_dir = dir.path().to_path_buf();
        let conversation_id = ConversationId::new();

        // Write two events
        for i in 0..2 {
            let event = ConversationAuditEvent::UserMessage {
                portal_id: PortalId::new(),
                portal_type: "Discord(123:456)".to_string(),
                content: format!("message {}", i),
                timestamp: Utc::now(),
            };

            write_audit_event(&audit_dir, &conversation_id, &event)
                .await
                .unwrap();
        }

        let file_path = audit_dir.join(format!("{}.jsonl", conversation_id.0));
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();

        // Should have 2 lines
        assert_eq!(content.lines().count(), 2);
    }

    #[tokio::test]
    async fn audit_file_is_valid_jsonl() {
        let dir = tempdir().unwrap();
        let audit_dir = dir.path().to_path_buf();
        let conversation_id = ConversationId::new();

        let event = ConversationAuditEvent::Error {
            error: "test error".to_string(),
            timestamp: Utc::now(),
        };

        write_audit_event(&audit_dir, &conversation_id, &event)
            .await
            .unwrap();

        let file_path = audit_dir.join(format!("{}.jsonl", conversation_id.0));
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();

        // Each line should parse as JSON
        for line in content.lines() {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("type").is_some());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn audit_file_has_0o600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let audit_dir = dir.path().to_path_buf();
        let conversation_id = ConversationId::new();

        let event = ConversationAuditEvent::UserMessage {
            portal_id: PortalId::new(),
            portal_type: "Discord(123:456)".to_string(),
            content: "test".to_string(),
            timestamp: Utc::now(),
        };

        write_audit_event(&audit_dir, &conversation_id, &event)
            .await
            .unwrap();

        let file_path = audit_dir.join(format!("{}.jsonl", conversation_id.0));
        let metadata = tokio::fs::metadata(&file_path).await.unwrap();
        let permissions = metadata.permissions();

        assert_eq!(permissions.mode() & 0o777, 0o600);
    }
}
