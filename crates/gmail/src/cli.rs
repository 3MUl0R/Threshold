//! CLI subcommand definitions and handler for `threshold gmail`.
//!
//! All subcommands output JSON to stdout for Claude to parse.
//! All actions are logged to the audit trail.

use std::path::Path;
use std::sync::Arc;

use threshold_core::config::GmailToolConfig;
use threshold_core::{AuditTrail, SecretStore};

use crate::auth::GmailAuth;
use crate::client::GmailClient;

/// Arguments for the `threshold gmail` command.
#[derive(clap::Args)]
pub struct GmailArgs {
    #[command(subcommand)]
    pub command: GmailCommands,
}

/// Gmail CLI subcommands.
#[derive(clap::Subcommand)]
pub enum GmailCommands {
    /// Run OAuth setup flow for an inbox (interactive, opens browser)
    Auth {
        /// Email address to authorize
        #[arg(long)]
        inbox: String,
        /// Include send permission in OAuth scope
        #[arg(long)]
        include_send: bool,
    },
    /// List recent messages from an inbox
    List {
        /// Email inbox to access
        #[arg(long)]
        inbox: String,
        /// Gmail search query (optional)
        #[arg(long)]
        query: Option<String>,
        /// Maximum number of messages to return
        #[arg(long, default_value = "10")]
        max: u32,
    },
    /// Read a specific message by ID
    Read {
        /// Email inbox to access
        #[arg(long)]
        inbox: String,
        /// Message ID
        id: String,
    },
    /// Search messages with Gmail search syntax
    Search {
        /// Email inbox to access
        #[arg(long)]
        inbox: String,
        /// Gmail search query (e.g., "from:alice subject:meeting")
        query: String,
        /// Maximum number of results
        #[arg(long, default_value = "10")]
        max: u32,
    },
    /// Send a new email (requires allow_send = true in config)
    Send {
        /// Email inbox to send from
        #[arg(long)]
        inbox: String,
        /// Recipient email address
        #[arg(long)]
        to: String,
        /// Email subject
        #[arg(long)]
        subject: String,
        /// Email body text
        #[arg(long)]
        body: String,
    },
    /// Reply to an existing email (requires allow_send = true in config)
    Reply {
        /// Email inbox to send from
        #[arg(long)]
        inbox: String,
        /// Message ID to reply to
        id: String,
        /// Reply body text
        #[arg(long)]
        body: String,
    },
}

/// Handle a Gmail CLI command.
///
/// If `audit_path` is provided, all actions are logged to the audit trail.
pub async fn handle_gmail_command(
    args: GmailArgs,
    config: &GmailToolConfig,
    audit_path: Option<&Path>,
) -> anyhow::Result<()> {
    if !config.enabled {
        let output = serde_json::json!({
            "error": "Gmail integration is disabled. Set tools.gmail.enabled = true in config."
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        anyhow::bail!("Gmail is disabled");
    }

    let audit = audit_path.map(|p| AuditTrail::new(p.to_path_buf()));
    let secret_store = Arc::new(SecretStore::new());

    match args.command {
        GmailCommands::Auth {
            inbox,
            include_send,
        } => {
            validate_inbox(&inbox, config)?;
            // If requesting send scope, verify allow_send is enabled
            if include_send {
                check_send_permission(config)?;
            }
            let auth = GmailAuth::new(secret_store, &inbox);
            auth.authorize(include_send).await?;

            audit_log(&audit, &serde_json::json!({
                "action": "auth",
                "inbox": inbox
            })).await;

            let output = serde_json::json!({
                "status": "ok",
                "inbox": inbox,
                "message": "Gmail OAuth setup complete"
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }

        GmailCommands::List { inbox, query, max } => {
            validate_inbox(&inbox, config)?;
            let client = GmailClient::new(secret_store, &inbox);
            let messages = client.list_messages(query.as_deref(), max).await?;

            audit_log(&audit, &serde_json::json!({
                "action": "list",
                "inbox": inbox,
                "count": messages.len()
            })).await;

            println!("{}", serde_json::to_string_pretty(&messages)?);
        }

        GmailCommands::Read { inbox, id } => {
            validate_inbox(&inbox, config)?;
            let client = GmailClient::new(secret_store, &inbox);
            let message = client.get_message(&id).await?;

            audit_log(&audit, &serde_json::json!({
                "action": "read",
                "inbox": inbox,
                "message_id": id
            })).await;

            println!("{}", serde_json::to_string_pretty(&message)?);
        }

        GmailCommands::Search { inbox, query, max } => {
            validate_inbox(&inbox, config)?;
            let client = GmailClient::new(secret_store, &inbox);
            let messages = client.search(&query, max).await?;

            audit_log(&audit, &serde_json::json!({
                "action": "search",
                "inbox": inbox,
                "query": query,
                "count": messages.len()
            })).await;

            println!("{}", serde_json::to_string_pretty(&messages)?);
        }

        GmailCommands::Send {
            inbox,
            to,
            subject,
            body,
        } => {
            validate_inbox(&inbox, config)?;
            check_send_permission(config)?;
            let client = GmailClient::new(secret_store, &inbox);
            client.send(&to, &subject, &body).await?;

            audit_log(&audit, &serde_json::json!({
                "action": "send",
                "inbox": inbox,
                "to": to,
                "subject": subject
            })).await;

            let output = serde_json::json!({
                "status": "ok",
                "message": "Email sent",
                "to": to,
                "subject": subject
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }

        GmailCommands::Reply { inbox, id, body } => {
            validate_inbox(&inbox, config)?;
            check_send_permission(config)?;
            let client = GmailClient::new(secret_store, &inbox);
            client.reply(&id, &body).await?;

            audit_log(&audit, &serde_json::json!({
                "action": "reply",
                "inbox": inbox,
                "message_id": id
            })).await;

            let output = serde_json::json!({
                "status": "ok",
                "message": "Reply sent",
                "in_reply_to": id
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }

    Ok(())
}

/// Log an action to the audit trail (non-fatal: log warning on failure).
async fn audit_log(audit: &Option<AuditTrail>, data: &serde_json::Value) {
    if let Some(trail) = audit
        && let Err(e) = trail.append_event("gmail", data).await
    {
        tracing::warn!("Failed to write Gmail audit entry: {}", e);
    }
}

/// Validate that the inbox is in the config allowlist (if set).
/// Comparison is case-insensitive since email addresses are case-insensitive.
fn validate_inbox(inbox: &str, config: &GmailToolConfig) -> anyhow::Result<()> {
    if let Some(ref allowed) = config.inboxes {
        let inbox_lower = inbox.to_lowercase();
        if !allowed.iter().any(|a| a.to_lowercase() == inbox_lower) {
            let output = serde_json::json!({
                "error": format!(
                    "Inbox '{}' is not in the allowed list. Allowed: {:?}",
                    inbox, allowed
                )
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
            anyhow::bail!("Inbox '{}' not in allowlist", inbox);
        }
    }
    Ok(())
}

/// Check that send permission is enabled in config.
fn check_send_permission(config: &GmailToolConfig) -> anyhow::Result<()> {
    if !config.allow_send.unwrap_or(false) {
        let output = serde_json::json!({
            "error": "Email sending is disabled. Set tools.gmail.allow_send = true in config."
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        anyhow::bail!("Gmail sending is disabled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config() -> GmailToolConfig {
        GmailToolConfig {
            enabled: true,
            inboxes: None,
            allow_send: None,
        }
    }

    fn config_with_inboxes() -> GmailToolConfig {
        GmailToolConfig {
            enabled: true,
            inboxes: Some(vec![
                "alice@gmail.com".into(),
                "bob@company.com".into(),
            ]),
            allow_send: None,
        }
    }

    #[test]
    fn validate_inbox_no_allowlist_passes() {
        let config = enabled_config();
        assert!(validate_inbox("any@email.com", &config).is_ok());
    }

    #[test]
    fn validate_inbox_in_allowlist_passes() {
        let config = config_with_inboxes();
        assert!(validate_inbox("alice@gmail.com", &config).is_ok());
        assert!(validate_inbox("bob@company.com", &config).is_ok());
    }

    #[test]
    fn validate_inbox_not_in_allowlist_fails() {
        let config = config_with_inboxes();
        assert!(validate_inbox("eve@hacker.com", &config).is_err());
    }

    #[test]
    fn validate_inbox_case_insensitive() {
        let config = config_with_inboxes();
        assert!(validate_inbox("Alice@Gmail.Com", &config).is_ok());
        assert!(validate_inbox("BOB@COMPANY.COM", &config).is_ok());
    }

    #[test]
    fn check_send_permission_none_blocks() {
        let config = GmailToolConfig {
            enabled: true,
            inboxes: None,
            allow_send: None,
        };
        assert!(check_send_permission(&config).is_err());
    }

    #[test]
    fn check_send_permission_false_blocks() {
        let config = GmailToolConfig {
            enabled: true,
            inboxes: None,
            allow_send: Some(false),
        };
        assert!(check_send_permission(&config).is_err());
    }

    #[test]
    fn check_send_permission_true_allows() {
        let config = GmailToolConfig {
            enabled: true,
            inboxes: None,
            allow_send: Some(true),
        };
        assert!(check_send_permission(&config).is_ok());
    }

    #[tokio::test]
    async fn audit_log_writes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("gmail-audit.jsonl");
        let audit = Some(AuditTrail::new(audit_path.clone()));

        audit_log(
            &audit,
            &serde_json::json!({"action": "list", "inbox": "test@gmail.com", "count": 5}),
        )
        .await;

        let content = tokio::fs::read_to_string(&audit_path).await.unwrap();
        let entry: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(entry["type"], "gmail");
        assert_eq!(entry["data"]["action"], "list");
        assert_eq!(entry["data"]["inbox"], "test@gmail.com");
        assert_eq!(entry["data"]["count"], 5);
        assert!(entry["ts"].is_string());
    }

    #[tokio::test]
    async fn audit_log_send_includes_to_and_subject() {
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("gmail-audit.jsonl");
        let audit = Some(AuditTrail::new(audit_path.clone()));

        audit_log(
            &audit,
            &serde_json::json!({
                "action": "send",
                "inbox": "sender@gmail.com",
                "to": "recipient@example.com",
                "subject": "Important update"
            }),
        )
        .await;

        let content = tokio::fs::read_to_string(&audit_path).await.unwrap();
        let entry: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(entry["data"]["action"], "send");
        assert_eq!(entry["data"]["to"], "recipient@example.com");
        assert_eq!(entry["data"]["subject"], "Important update");
    }

    #[tokio::test]
    async fn audit_log_none_does_nothing() {
        // No audit trail configured — should not panic
        audit_log(
            &None,
            &serde_json::json!({"action": "list", "inbox": "test@gmail.com"}),
        )
        .await;
    }
}
