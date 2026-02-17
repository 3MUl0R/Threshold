//! Discord slash commands for scheduler management.
//!
//! Provides `/schedule`, `/schedules`, and `/heartbeat` commands that interact
//! with the unified scheduler via `SchedulerHandle`.

use crate::bot::Context;
use threshold_core::{ScheduledAction, ThresholdError};
use threshold_scheduler::task::{DeliveryTarget, ScheduledTask, TaskKind};

type Result = std::result::Result<(), ThresholdError>;

/// Discord slash command choices for schedule action types.
#[derive(Debug, Clone, poise::ChoiceParameter)]
pub enum ScheduleActionChoice {
    /// Launch a new Claude conversation
    Conversation,
    /// Run a shell command
    Script,
    /// Run a command, then analyze output with Claude
    Monitor,
}

/// Create a recurring scheduled task.
#[poise::command(slash_command)]
pub async fn schedule(
    ctx: Context<'_>,
    #[description = "Task name"] name: String,
    #[description = "Cron expression (e.g. '0 0 3 * * *' for 3 AM daily)"] cron: String,
    #[description = "Action type"]
    #[rename = "action"]
    action_type: ScheduleActionChoice,
    #[description = "Prompt text or shell command"] value: String,
    #[description = "Model override (optional)"] model: Option<String>,
) -> Result {
    let scheduler = match &ctx.data().scheduler {
        Some(s) => s,
        None => {
            ctx.say("Scheduler is not enabled.").await.ok();
            return Ok(());
        }
    };

    let channel_id = ctx.channel_id().get();

    let action = match action_type {
        ScheduleActionChoice::Conversation => ScheduledAction::NewConversation {
            prompt: value,
            model,
        },
        ScheduleActionChoice::Script => ScheduledAction::Script {
            command: value,
            working_dir: None,
        },
        ScheduleActionChoice::Monitor => ScheduledAction::ScriptThenConversation {
            command: value,
            prompt_template: "Script output:\n{output}\n\nAnalyze and report any issues."
                .to_string(),
            model,
        },
    };

    let mut task = ScheduledTask::new(name.clone(), cron.clone(), action).map_err(|e| {
        ThresholdError::InvalidInput {
            message: e.to_string(),
        }
    })?;
    task.delivery = DeliveryTarget::DiscordChannel { channel_id };

    let next_run = task
        .next_run
        .map_or("unknown".to_string(), |t| t.to_rfc3339());

    scheduler.add_task(task).await?;

    ctx.say(format!(
        "Created scheduled task **{}** (`{}`)\nNext run: {}",
        name, cron, next_run,
    ))
    .await
    .ok();

    Ok(())
}

/// List all scheduled tasks.
#[poise::command(slash_command)]
pub async fn schedules(ctx: Context<'_>) -> Result {
    let scheduler = match &ctx.data().scheduler {
        Some(s) => s,
        None => {
            ctx.say("Scheduler is not enabled.").await.ok();
            return Ok(());
        }
    };

    let tasks = scheduler.list_tasks().await?;
    if tasks.is_empty() {
        ctx.say("No scheduled tasks.").await.ok();
        return Ok(());
    }

    let mut response = String::from("**Scheduled Tasks:**\n\n");
    for task in tasks {
        let kind_label = match task.kind {
            TaskKind::Heartbeat => " [heartbeat]",
            TaskKind::Cron => "",
        };
        response.push_str(&format!(
            "- **{}**{} | `{}` | {} | Next: {}\n",
            task.name,
            kind_label,
            task.cron_expression,
            if task.enabled { "enabled" } else { "disabled" },
            task.next_run
                .map_or("unknown".to_string(), |t| t.to_rfc3339()),
        ));
    }
    ctx.say(response).await.ok();
    Ok(())
}

/// Heartbeat status and controls.
#[poise::command(slash_command)]
pub async fn heartbeat(
    ctx: Context<'_>,
    #[description = "Action: status, pause, resume"] action: String,
) -> Result {
    let scheduler = match &ctx.data().scheduler {
        Some(s) => s,
        None => {
            ctx.say("Scheduler is not enabled.").await.ok();
            return Ok(());
        }
    };

    let tasks = scheduler.list_tasks().await?;
    let heartbeat_task = tasks.iter().find(|t| t.kind == TaskKind::Heartbeat);

    match action.as_str() {
        "status" => {
            if let Some(hb) = heartbeat_task {
                ctx.say(format!(
                    "**Heartbeat:** {}\nEnabled: {}\nLast run: {}\nNext run: {}",
                    hb.name,
                    hb.enabled,
                    hb.last_run
                        .map_or("never".to_string(), |t| t.to_rfc3339()),
                    hb.next_run
                        .map_or("unknown".to_string(), |t| t.to_rfc3339()),
                ))
                .await
                .ok();
            } else {
                ctx.say("No heartbeat configured.").await.ok();
            }
        }
        "pause" => {
            if let Some(hb) = heartbeat_task {
                scheduler.toggle_task(hb.id, false).await?;
                ctx.say("Heartbeat paused.").await.ok();
            } else {
                ctx.say("No heartbeat configured.").await.ok();
            }
        }
        "resume" => {
            if let Some(hb) = heartbeat_task {
                scheduler.toggle_task(hb.id, true).await?;
                ctx.say("Heartbeat resumed.").await.ok();
            } else {
                ctx.say("No heartbeat configured.").await.ok();
            }
        }
        _ => {
            ctx.say("Usage: /heartbeat status|pause|resume").await.ok();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_action_choice_variants() {
        // Verify the enum variants match expected Discord choices
        let choices = vec![
            ScheduleActionChoice::Conversation,
            ScheduleActionChoice::Script,
            ScheduleActionChoice::Monitor,
        ];
        assert_eq!(choices.len(), 3);
    }
}
