//! Portal subcommands for listing active portals.
//!
//! These commands communicate with the running daemon via Unix socket.

use crate::daemon_client::{DaemonClient, DaemonCommand, ResponseStatus};
use crate::output::OutputFormat;

/// Portal management subcommands.
#[derive(clap::Subcommand)]
pub enum PortalCommands {
    /// List all active portals
    List {
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
}

pub async fn handle_portal_command(command: PortalCommands) -> anyhow::Result<()> {
    let client = DaemonClient::new()?;

    match command {
        PortalCommands::List { format } => {
            let response = client.send_command(&DaemonCommand::PortalList).await?;

            if response.status == ResponseStatus::Error {
                let msg = response.message.unwrap_or_else(|| "Unknown error".into());
                anyhow::bail!("Error: {}", msg);
            }

            match format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&response.data.unwrap_or_default())?
                    );
                }
                OutputFormat::Table => {
                    if let Some(data) = response.data {
                        let portals: Vec<serde_json::Value> =
                            serde_json::from_value(data).unwrap_or_default();
                        if portals.is_empty() {
                            println!("No active portals.");
                        } else {
                            println!(
                                "{:<38} {:<22} {:<38} {:<10} {}",
                                "Portal ID", "Type", "Conversation", "Primary", "Connected"
                            );
                            println!("{}", "-".repeat(120));
                            for p in &portals {
                                println!(
                                    "{:<38} {:<22} {:<38} {:<10} {}",
                                    p["portal_id"].as_str().unwrap_or("-"),
                                    p["portal_type"].as_str().unwrap_or("-"),
                                    p["conversation_id"].as_str().unwrap_or("-"),
                                    if p["is_primary"].as_bool().unwrap_or(false) {
                                        "yes"
                                    } else {
                                        "no"
                                    },
                                    p["connected_at"].as_str().unwrap_or("-"),
                                );
                            }
                        }
                    } else {
                        println!("No portal data returned.");
                    }
                }
            }
        }
    }

    Ok(())
}
