//! Data types for Gmail API responses.
//!
//! These types are serialized to JSON for CLI output and deserialized from
//! Google Gmail API responses.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Summary of a Gmail message (used in list/search results).
///
/// Contains metadata only — no body content. Use `EmailMessage` for full content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub date: DateTime<Utc>,
    pub labels: Vec<String>,
    pub is_unread: bool,
}

/// Full email message with body content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailMessage {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub date: DateTime<Utc>,
    pub labels: Vec<String>,
    pub attachments: Vec<AttachmentInfo>,
}

/// Metadata about an email attachment (not the content itself).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

/// Google OAuth token exchange response.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

/// Google OAuth error response.
#[derive(Debug, Deserialize)]
pub struct TokenErrorResponse {
    pub error: String,
    pub error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn message_summary_serde_round_trip() {
        let summary = MessageSummary {
            id: "msg123".into(),
            from: "alice@example.com".into(),
            subject: "Hello World".into(),
            snippet: "This is a test...".into(),
            date: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            labels: vec!["INBOX".into(), "UNREAD".into()],
            is_unread: true,
        };

        let json = serde_json::to_string(&summary).unwrap();
        let restored: MessageSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary.id, restored.id);
        assert_eq!(summary.from, restored.from);
        assert_eq!(summary.subject, restored.subject);
        assert_eq!(summary.is_unread, restored.is_unread);
        assert_eq!(summary.labels.len(), restored.labels.len());
    }

    #[test]
    fn email_message_serde_round_trip() {
        let message = EmailMessage {
            id: "msg456".into(),
            from: "bob@example.com".into(),
            to: vec!["alice@example.com".into()],
            cc: vec!["charlie@example.com".into()],
            subject: "Project Update".into(),
            body_text: "Here is the latest update.".into(),
            body_html: Some("<p>Here is the latest update.</p>".into()),
            date: Utc.with_ymd_and_hms(2025, 6, 15, 14, 30, 0).unwrap(),
            labels: vec!["INBOX".into()],
            attachments: vec![AttachmentInfo {
                filename: "report.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 1024,
            }],
        };

        let json = serde_json::to_string(&message).unwrap();
        let restored: EmailMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(message.id, restored.id);
        assert_eq!(message.from, restored.from);
        assert_eq!(message.to, restored.to);
        assert_eq!(message.cc, restored.cc);
        assert_eq!(message.body_text, restored.body_text);
        assert_eq!(message.body_html, restored.body_html);
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(restored.attachments[0].filename, "report.pdf");
    }

    #[test]
    fn email_message_without_html_body() {
        let message = EmailMessage {
            id: "msg789".into(),
            from: "sender@example.com".into(),
            to: vec!["recipient@example.com".into()],
            cc: vec![],
            subject: "Plain text only".into(),
            body_text: "No HTML here.".into(),
            body_html: None,
            date: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            labels: vec![],
            attachments: vec![],
        };

        let json = serde_json::to_string(&message).unwrap();
        let restored: EmailMessage = serde_json::from_str(&json).unwrap();
        assert!(restored.body_html.is_none());
        assert!(restored.attachments.is_empty());
    }

    #[test]
    fn attachment_info_serde_round_trip() {
        let info = AttachmentInfo {
            filename: "image.png".into(),
            mime_type: "image/png".into(),
            size_bytes: 2048,
        };

        let json = serde_json::to_string(&info).unwrap();
        let restored: AttachmentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info.filename, restored.filename);
        assert_eq!(info.mime_type, restored.mime_type);
        assert_eq!(info.size_bytes, restored.size_bytes);
    }

    #[test]
    fn token_response_deserializes() {
        let json = r#"{
            "access_token": "ya29.abc123",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "1//xEodef456",
            "scope": "https://www.googleapis.com/auth/gmail.readonly"
        }"#;

        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "ya29.abc123");
        assert_eq!(resp.token_type, "Bearer");
        assert_eq!(resp.expires_in, 3600);
        assert_eq!(resp.refresh_token.unwrap(), "1//xEodef456");
    }

    #[test]
    fn token_response_without_refresh_token() {
        let json = r#"{
            "access_token": "ya29.abc123",
            "token_type": "Bearer",
            "expires_in": 3600
        }"#;

        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert!(resp.refresh_token.is_none());
        assert!(resp.scope.is_none());
    }

    #[test]
    fn token_error_response_deserializes() {
        let json = r#"{
            "error": "invalid_grant",
            "error_description": "Token has been revoked."
        }"#;

        let resp: TokenErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "invalid_grant");
        assert_eq!(resp.error_description.unwrap(), "Token has been revoked.");
    }
}
