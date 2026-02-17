//! Schedule subcommands for managing scheduled tasks.
//!
//! These commands communicate with the running daemon via Unix socket.
//! The daemon-side socket listener is implemented in Milestone 6.

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
        /// Cron expression (e.g., "0 0 3 * * *" for 3 AM daily)
        #[arg(short, long)]
        cron: String,
        /// Prompt to send to Claude
        #[arg(short, long)]
        prompt: String,
        /// Model override (optional)
        #[arg(short, long)]
        model: Option<String>,
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
    /// Toggle a scheduled task on/off
    Toggle {
        /// Task ID to toggle
        id: String,
        /// Output format
        #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::default())]
        format: OutputFormat,
    },
}

/// Handle a schedule subcommand by dispatching to the daemon.
pub async fn handle_schedule_command(command: ScheduleCommands) -> anyhow::Result<()> {
    let client = DaemonClient::new();

    let daemon_command = match &command {
        ScheduleCommands::Conversation {
            name,
            cron,
            prompt,
            model,
            ..
        } => DaemonCommand::ScheduleConversation {
            name: name.clone(),
            cron: cron.clone(),
            prompt: prompt.clone(),
            model: model.clone(),
        },
        ScheduleCommands::Script {
            name,
            cron,
            command: cmd,
            working_dir,
            ..
        } => DaemonCommand::ScheduleScript {
            name: name.clone(),
            cron: cron.clone(),
            command: cmd.clone(),
            working_dir: working_dir.clone(),
        },
        ScheduleCommands::Monitor {
            name,
            cron,
            command: cmd,
            prompt_template,
            model,
            ..
        } => DaemonCommand::ScheduleMonitor {
            name: name.clone(),
            cron: cron.clone(),
            command: cmd.clone(),
            prompt_template: prompt_template.clone(),
            model: model.clone(),
        },
        ScheduleCommands::List { .. } => DaemonCommand::ScheduleList,
        ScheduleCommands::Delete { id, .. } => DaemonCommand::ScheduleDelete { id: id.clone() },
        ScheduleCommands::Toggle { id, .. } => DaemonCommand::ScheduleToggle { id: id.clone() },
    };

    let response = client.send_command(&daemon_command).await?;

    // Format and print the response
    let _format = match &command {
        ScheduleCommands::Conversation { format, .. }
        | ScheduleCommands::Script { format, .. }
        | ScheduleCommands::Monitor { format, .. }
        | ScheduleCommands::List { format, .. }
        | ScheduleCommands::Delete { format, .. }
        | ScheduleCommands::Toggle { format, .. } => format,
    };

    // TODO(Milestone 6): Format response based on OutputFormat
    println!("{}", serde_json::to_string_pretty(&response)?);

    Ok(())
}
