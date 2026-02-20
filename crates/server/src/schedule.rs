//! Schedule subcommands for managing scheduled tasks.
//!
//! These commands communicate with the running daemon via Unix socket.

use threshold_core::{ConversationId, ScheduledAction};
use threshold_scheduler::task::{DeliveryTarget, ScheduledTask};

use crate::daemon_client::{DaemonClient, DaemonCommand};
use crate::output::OutputFormat;

/// Schedule management subcommands.
#[derive(clap::Subcommand)]
pub enum ScheduleCommands {
    /// Schedule a new conversation
    Conversation {
        /// Task name
        #[arg(short, long)]
        name: String,
        /// Cron expression (e.g., "0 0 12 * * *" for noon daily)
        #[arg(short, long)]
        cron: String,
        /// Prompt to send to Claude
        #[arg(short, long)]
        prompt: String,
        /// Model override (optional)
        #[arg(short, long)]
        model: Option<String>,
        /// IANA timezone (e.g., "America/Los_Angeles"). Cron is evaluated in this timezone.
        #[arg(long)]
        timezone: Option<String>,
        /// Deliver results to a Discord channel (channel ID)
        #[arg(long)]
        discord_channel: Option<u64>,
        /// Deliver results as a Discord DM (user ID)
        #[arg(long)]
        discord_dm: Option<u64>,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
    /// Schedule a script execution
    Script {
        /// Task name
        #[arg(short, long)]
        name: String,
        /// Cron expression
        #[arg(short, long)]
        cron: String,
        /// Shell command to execute
        #[arg(short = 'x', long)]
        command: String,
        /// Working directory (optional)
        #[arg(short, long)]
        working_dir: Option<String>,
        /// IANA timezone (e.g., "America/Los_Angeles"). Cron is evaluated in this timezone.
        #[arg(long)]
        timezone: Option<String>,
        /// Deliver results to a Discord channel (channel ID)
        #[arg(long)]
        discord_channel: Option<u64>,
        /// Deliver results as a Discord DM (user ID)
        #[arg(long)]
        discord_dm: Option<u64>,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
    /// Schedule a monitoring task (script output + AI analysis)
    Monitor {
        /// Task name
        #[arg(short, long)]
        name: String,
        /// Cron expression
        #[arg(short, long)]
        cron: String,
        /// Shell command to execute
        #[arg(short = 'x', long)]
        command: String,
        /// Prompt template (use {output} for script output)
        #[arg(short = 't', long)]
        prompt_template: String,
        /// Model override (optional)
        #[arg(short = 'o', long)]
        model: Option<String>,
        /// IANA timezone (e.g., "America/Los_Angeles"). Cron is evaluated in this timezone.
        #[arg(long)]
        timezone: Option<String>,
        /// Deliver results to a Discord channel (channel ID)
        #[arg(long)]
        discord_channel: Option<u64>,
        /// Deliver results as a Discord DM (user ID)
        #[arg(long)]
        discord_dm: Option<u64>,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
    /// Schedule a recurring message to an existing conversation
    Resume {
        /// Task name
        #[arg(short, long)]
        name: String,
        /// Cron expression
        #[arg(short, long)]
        cron: String,
        /// Conversation ID to resume
        #[arg(long)]
        conversation_id: String,
        /// Prompt to send to the conversation
        #[arg(short, long)]
        prompt: String,
        /// IANA timezone (e.g., "America/Los_Angeles"). Cron is evaluated in this timezone.
        #[arg(long)]
        timezone: Option<String>,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
    /// List all scheduled tasks
    List {
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
    /// Delete a scheduled task
    Delete {
        /// Task ID to delete
        id: String,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
    /// Enable a scheduled task
    Enable {
        /// Task ID to enable
        id: String,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
    /// Disable a scheduled task
    Disable {
        /// Task ID to disable
        id: String,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
}

/// Handle a schedule subcommand by dispatching to the daemon.
pub async fn handle_schedule_command(command: ScheduleCommands) -> anyhow::Result<()> {
    let client = DaemonClient::new()?;

    let daemon_command = match &command {
        ScheduleCommands::Conversation {
            name,
            cron,
            prompt,
            model,
            timezone,
            discord_channel,
            discord_dm,
            ..
        } => {
            let mut task = build_task(
                name.clone(),
                cron.clone(),
                ScheduledAction::NewConversation {
                    prompt: prompt.clone(),
                    model: model.clone(),
                },
                timezone.clone(),
            )?;
            task.delivery = resolve_delivery(*discord_channel, *discord_dm);
            DaemonCommand::ScheduleCreate(task)
        }
        ScheduleCommands::Script {
            name,
            cron,
            command: cmd,
            working_dir,
            timezone,
            discord_channel,
            discord_dm,
            ..
        } => {
            let mut task = build_task(
                name.clone(),
                cron.clone(),
                ScheduledAction::Script {
                    command: cmd.clone(),
                    working_dir: working_dir.clone(),
                },
                timezone.clone(),
            )?;
            task.delivery = resolve_delivery(*discord_channel, *discord_dm);
            DaemonCommand::ScheduleCreate(task)
        }
        ScheduleCommands::Monitor {
            name,
            cron,
            command: cmd,
            prompt_template,
            model,
            timezone,
            discord_channel,
            discord_dm,
            ..
        } => {
            let mut task = build_task(
                name.clone(),
                cron.clone(),
                ScheduledAction::ScriptThenConversation {
                    command: cmd.clone(),
                    prompt_template: prompt_template.clone(),
                    model: model.clone(),
                },
                timezone.clone(),
            )?;
            task.delivery = resolve_delivery(*discord_channel, *discord_dm);
            DaemonCommand::ScheduleCreate(task)
        }
        ScheduleCommands::Resume {
            name,
            cron,
            conversation_id,
            prompt,
            timezone,
            ..
        } => {
            let conv_id = ConversationId(
                uuid::Uuid::parse_str(conversation_id)
                    .map_err(|e| anyhow::anyhow!("Invalid conversation ID: {}", e))?,
            );
            let mut task = build_task(
                name.clone(),
                cron.clone(),
                ScheduledAction::ResumeConversation {
                    conversation_id: conv_id,
                    prompt: prompt.clone(),
                },
                timezone.clone(),
            )?;
            // Mark as conversation-attached so deliver_result() skips
            // duplicate delivery — output goes through the portal system.
            task.conversation_id = Some(conv_id);
            DaemonCommand::ScheduleCreate(task)
        }
        ScheduleCommands::List { .. } => DaemonCommand::ScheduleList,
        ScheduleCommands::Delete { id, .. } => DaemonCommand::ScheduleDelete { id: id.clone() },
        ScheduleCommands::Enable { id, .. } => DaemonCommand::ScheduleToggle {
            id: id.clone(),
            enabled: true,
        },
        ScheduleCommands::Disable { id, .. } => DaemonCommand::ScheduleToggle {
            id: id.clone(),
            enabled: false,
        },
    };

    let response = client.send_command(&daemon_command).await?;

    // Format and print the response
    let _format = match &command {
        ScheduleCommands::Conversation { format, .. }
        | ScheduleCommands::Script { format, .. }
        | ScheduleCommands::Monitor { format, .. }
        | ScheduleCommands::Resume { format, .. }
        | ScheduleCommands::List { format, .. }
        | ScheduleCommands::Delete { format, .. }
        | ScheduleCommands::Enable { format, .. }
        | ScheduleCommands::Disable { format, .. } => format,
    };

    println!("{}", serde_json::to_string_pretty(&response)?);

    Ok(())
}

/// Build a `ScheduledTask` from CLI arguments, optionally timezone-aware.
fn build_task(
    name: String,
    cron: String,
    action: ScheduledAction,
    timezone: Option<String>,
) -> anyhow::Result<ScheduledTask> {
    match timezone {
        Some(tz) => ScheduledTask::new_with_timezone(name, cron, action, tz)
            .map_err(|e| anyhow::anyhow!("{}", e)),
        None => ScheduledTask::new(name, cron, action).map_err(|e| anyhow::anyhow!("{}", e)),
    }
}

/// Resolve delivery target from CLI flags. Prefers channel over DM.
fn resolve_delivery(discord_channel: Option<u64>, discord_dm: Option<u64>) -> DeliveryTarget {
    if let Some(channel_id) = discord_channel {
        DeliveryTarget::DiscordChannel { channel_id }
    } else if let Some(user_id) = discord_dm {
        DeliveryTarget::DiscordDm { user_id }
    } else {
        DeliveryTarget::AuditLogOnly
    }
}
