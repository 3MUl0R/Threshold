//! CLI subcommand definitions and handler for `threshold gmail`.
//!
//! All subcommands output JSON to stdout for Claude to parse.

use std::sync::Arc;

use threshold_core::config::GmailToolConfig;
use threshold_core::SecretStore;

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
pub async fn handle_gmail_command(
    args: GmailArgs,
    config: &GmailToolConfig,
) -> anyhow::Result<()> {
    if !config.enabled {
        let output = serde_json::json!({
            "error": "Gmail integration is disabled. Set tools.gmail.enabled = true in config."
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        anyhow::bail!("Gmail is disabled");
    }

    let secret_store = Arc::new(SecretStore::new());

    match args.command {
        GmailCommands::Auth {
            inbox,
            include_send,
        } => {
            validate_inbox(&inbox, config)?;
            let auth = GmailAuth::new(secret_store, &inbox);
            auth.authorize(include_send).await?;

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

            println!("{}", serde_json::to_string_pretty(&messages)?);
        }

        GmailCommands::Read { inbox, id } => {
            validate_inbox(&inbox, config)?;
            let client = GmailClient::new(secret_store, &inbox);
            let message = client.get_message(&id).await?;

            println!("{}", serde_json::to_string_pretty(&message)?);
        }

        GmailCommands::Search { inbox, query, max } => {
            validate_inbox(&inbox, config)?;
            let client = GmailClient::new(secret_store, &inbox);
            let messages = client.search(&query, max).await?;

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

/// Validate that the inbox is in the config allowlist (if set).
fn validate_inbox(inbox: &str, config: &GmailToolConfig) -> anyhow::Result<()> {
    if let Some(ref allowed) = config.inboxes {
        if !allowed.iter().any(|a| a == inbox) {
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
pub fn check_send_permission(config: &GmailToolConfig) -> anyhow::Result<()> {
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
}
