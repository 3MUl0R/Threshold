//! Audit trail integration - per-conversation JSONL logs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use threshold_cli_wrapper::response::Usage;
use threshold_core::{ConversationId, ConversationMode, MessageSource, PortalId, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Audit events written to per-conversation JSONL files
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MessageSource>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MessageSource>,
    },
    Acknowledgment {
        run_id: String,
        content: String,
        timestamp: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MessageSource>,
    },
    StatusUpdate {
        run_id: String,
        summary: String,
        elapsed_secs: u64,
        timestamp: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MessageSource>,
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
            source: None,
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

    // --- Phase 15B tests ---

    #[tokio::test]
    async fn source_field_round_trip() {
        let dir = tempdir().unwrap();
        let audit_dir = dir.path().to_path_buf();
        let conversation_id = ConversationId::new();

        let source = MessageSource::Portal {
            portal_id: PortalId::new(),
            platform: "Discord".to_string(),
        };
        let event = ConversationAuditEvent::AssistantMessage {
            content: "Hello!".to_string(),
            usage: None,
            duration_ms: 1234,
            timestamp: Utc::now(),
            source: Some(source),
        };

        write_audit_event(&audit_dir, &conversation_id, &event)
            .await
            .unwrap();

        let file_path = audit_dir.join(format!("{}.jsonl", conversation_id.0));
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        let restored: ConversationAuditEvent =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        match restored {
            ConversationAuditEvent::AssistantMessage { source, .. } => {
                let source = source.unwrap();
                assert!(
                    matches!(source, MessageSource::Portal { platform, .. } if platform == "Discord")
                );
            }
            _ => panic!("expected AssistantMessage"),
        }
    }

    #[tokio::test]
    async fn backward_compat_no_source() {
        // Old-format JSONL without source field should still deserialize
        let old_json = r#"{"type":"AssistantMessage","content":"Hi","usage":null,"duration_ms":100,"timestamp":"2025-01-01T00:00:00Z"}"#;
        let event: ConversationAuditEvent = serde_json::from_str(old_json).unwrap();
        match event {
            ConversationAuditEvent::AssistantMessage {
                source, content, ..
            } => {
                assert!(source.is_none(), "old events without source should be None");
                assert_eq!(content, "Hi");
            }
            _ => panic!("expected AssistantMessage"),
        }

        // Also verify Error backward compat
        let old_error = r#"{"type":"Error","error":"test","timestamp":"2025-01-01T00:00:00Z"}"#;
        let event: ConversationAuditEvent = serde_json::from_str(old_error).unwrap();
        assert!(matches!(
            event,
            ConversationAuditEvent::Error { source: None, .. }
        ));
    }
}
