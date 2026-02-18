//! Discord slash commands for scheduler management.
//!
//! Provides `/schedule`, `/schedules`, and `/heartbeat` commands that interact
//! with the unified scheduler via `SchedulerHandle`.

use crate::bot::Context;
use crate::portals::resolve_or_create_portal;
use threshold_core::{ConversationId, ScheduledAction, ThresholdError};
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

/// Default content for a new heartbeat.md file.
const DEFAULT_HEARTBEAT_CONTENT: &str = "\
# Heartbeat Instructions

This is a heartbeat wake-up. Review your memory file and this heartbeat file for context.

If you find specific instructions or pending tasks below, follow them.
Do not infer or repeat tasks that are already complete.

If nothing needs attention, reply: \"Heartbeat OK — nothing requires attention.\"

## Pending Tasks
(none)

## Status Log
(Agent: update this section with timestamps when you complete work during heartbeats)
";

/// Valid minute-level intervals — divisors of 60 that produce even cadence.
const VALID_MINUTE_INTERVALS: &[u64] = &[1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30];

/// Build a cron expression for heartbeat with phase-shift jitter.
///
/// Uses the conversation ID as a deterministic jitter source to spread
/// heartbeats across time and avoid simultaneous firing.
///
/// Returns `None` for intervals that can't produce even cadence in cron:
/// minute-level intervals must divide evenly into 60, and intervals >= 60
/// must be exact multiples of 60.
fn heartbeat_cron(interval_minutes: u64, conversation_id: &ConversationId) -> Option<String> {
    let interval = interval_minutes.max(1);
    if interval >= 60 && interval % 60 == 0 {
        let hours = interval / 60;
        // Hour-level intervals with jitter on the minute (0..59)
        let jitter = (conversation_id.0.as_u128() % 60) as u64;
        Some(format!("0 {} */{} * * * *", jitter, hours))
    } else if VALID_MINUTE_INTERVALS.contains(&interval) {
        // Minute-level intervals with phase-shifted start
        let jitter = (conversation_id.0.as_u128() % interval as u128) as u64;
        Some(format!("0 {}/{} * * * * *", jitter, interval))
    } else {
        // Non-divisor intervals produce irregular cadence at hour boundaries
        None
    }
}

/// Find the heartbeat task for a specific conversation.
fn find_heartbeat_for_conversation(
    tasks: &[ScheduledTask],
    conversation_id: &ConversationId,
) -> Option<ScheduledTask> {
    tasks
        .iter()
        .find(|t| {
            t.kind == TaskKind::Heartbeat
                && matches!(
                    &t.action,
                    ScheduledAction::ResumeConversation { conversation_id: cid, .. }
                    if *cid == *conversation_id
                )
        })
        .cloned()
}

/// Per-conversation heartbeat controls.
///
/// Resolves the current channel's conversation and manages its heartbeat.
#[poise::command(slash_command)]
pub async fn heartbeat(
    ctx: Context<'_>,
    #[description = "Action: enable, disable, status, pause, resume"] action: String,
    #[description = "Interval in minutes (for enable, default: 30)"] interval: Option<u64>,
) -> Result {
    let scheduler = match &ctx.data().scheduler {
        Some(s) => s,
        None => {
            ctx.say("Scheduler is not enabled.").await.ok();
            return Ok(());
        }
    };

    // Resolve this channel's conversation
    let guild_id = ctx.guild_id().map(|g| g.get()).unwrap_or(0);
    let channel_id = ctx.channel_id().get();
    let portal_id = resolve_or_create_portal(&ctx.data().engine, guild_id, channel_id).await;
    let conversation_id = ctx.data().engine.get_portal_conversation(&portal_id).await?;

    let tasks = scheduler.list_tasks().await?;
    let heartbeat_task = find_heartbeat_for_conversation(&tasks, &conversation_id);

    match action.as_str() {
        "enable" => {
            if heartbeat_task.is_some() {
                ctx.say("Heartbeat is already enabled for this conversation.")
                    .await
                    .ok();
                return Ok(());
            }

            let interval_minutes = interval.unwrap_or(30);
            if interval_minutes == 0 {
                ctx.say("Interval must be at least 1 minute.").await.ok();
                return Ok(());
            }
            let cron_expr = match heartbeat_cron(interval_minutes, &conversation_id) {
                Some(expr) => expr,
                None => {
                    ctx.say(format!(
                        "Interval of {} minutes cannot be expressed as a cron schedule. \
                         Use a value that divides evenly into 60 (e.g., 5, 10, 15, 30) \
                         or a multiple of 60 (e.g., 60, 120, 180).",
                        interval_minutes,
                    ))
                    .await
                    .ok();
                    return Ok(());
                }
            };

            // Create heartbeat.md if it doesn't exist
            let data_dir = ctx.data().engine.data_dir();
            let conv_dir = data_dir
                .join("conversations")
                .join(conversation_id.0.to_string());
            let heartbeat_path = conv_dir.join("heartbeat.md");
            if !heartbeat_path.exists() {
                if let Err(e) = std::fs::create_dir_all(&conv_dir) {
                    ctx.say(format!("Failed to create conversation directory: {}", e))
                        .await
                        .ok();
                    return Err(ThresholdError::IoError {
                        path: conv_dir.display().to_string(),
                        message: e.to_string(),
                    });
                }
                if let Err(e) = std::fs::write(&heartbeat_path, DEFAULT_HEARTBEAT_CONTENT) {
                    ctx.say(format!("Failed to write heartbeat.md: {}", e))
                        .await
                        .ok();
                    return Err(ThresholdError::IoError {
                        path: heartbeat_path.display().to_string(),
                        message: e.to_string(),
                    });
                }
            }

            // Create heartbeat scheduled task
            let action = ScheduledAction::ResumeConversation {
                conversation_id,
                prompt: String::new(), // replaced at fire time by dynamic prompt
            };

            let mut task =
                ScheduledTask::new(format!("heartbeat-{}", &conversation_id.0), cron_expr.clone(), action)
                    .map_err(|e| ThresholdError::InvalidInput {
                        message: e.to_string(),
                    })?;
            task.kind = TaskKind::Heartbeat;
            task.skip_if_running = true;
            task.conversation_id = Some(conversation_id);

            let next_run = task
                .next_run
                .map_or("unknown".to_string(), |t| t.to_rfc3339());

            scheduler.add_task(task).await?;

            ctx.say(format!(
                "Heartbeat enabled for this conversation (every {} min).\nCron: `{}`\nNext run: {}",
                interval_minutes, cron_expr, next_run,
            ))
            .await
            .ok();
        }
        "disable" => {
            if let Some(hb) = heartbeat_task {
                scheduler.remove_task(hb.id).await?;
                ctx.say("Heartbeat disabled for this conversation.")
                    .await
                    .ok();
            } else {
                ctx.say("No heartbeat is enabled for this conversation.")
                    .await
                    .ok();
            }
        }
        "status" => {
            if let Some(hb) = heartbeat_task {
                ctx.say(format!(
                    "**Heartbeat** for conversation `{}`\n\
                     Enabled: {}\n\
                     Schedule: `{}`\n\
                     Last run: {}\n\
                     Next run: {}",
                    conversation_id.0,
                    hb.enabled,
                    hb.cron_expression,
                    hb.last_run
                        .map_or("never".to_string(), |t| t.to_rfc3339()),
                    hb.next_run
                        .map_or("unknown".to_string(), |t| t.to_rfc3339()),
                ))
                .await
                .ok();
            } else {
                ctx.say(format!(
                    "No heartbeat for conversation `{}`. Use `/heartbeat enable` to set one up.",
                    conversation_id.0,
                ))
                .await
                .ok();
            }
        }
        "pause" => {
            if let Some(hb) = heartbeat_task {
                scheduler.toggle_task(hb.id, false).await?;
                ctx.say("Heartbeat paused.").await.ok();
            } else {
                ctx.say("No heartbeat is enabled for this conversation.")
                    .await
                    .ok();
            }
        }
        "resume" => {
            if let Some(hb) = heartbeat_task {
                scheduler.toggle_task(hb.id, true).await?;
                ctx.say("Heartbeat resumed.").await.ok();
            } else {
                ctx.say("No heartbeat is enabled for this conversation.")
                    .await
                    .ok();
            }
        }
        _ => {
            ctx.say("Usage: `/heartbeat enable|disable|status|pause|resume`")
                .await
                .ok();
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

    #[test]
    fn heartbeat_cron_30_minute_interval() {
        let conv_id = ConversationId(uuid::Uuid::nil());
        let cron = heartbeat_cron(30, &conv_id).unwrap();
        // nil UUID → 0 % 30 = 0 jitter → starts at minute 0
        assert_eq!(cron, "0 0/30 * * * * *");
    }

    #[test]
    fn heartbeat_cron_with_jitter() {
        // Use a specific UUID that produces non-zero jitter
        let uuid = uuid::Uuid::from_u128(17);
        let conv_id = ConversationId(uuid);
        let cron = heartbeat_cron(30, &conv_id).unwrap();
        // 17 % 30 = 17 → starts at minute 17
        assert_eq!(cron, "0 17/30 * * * * *");
    }

    #[test]
    fn heartbeat_cron_hourly() {
        let conv_id = ConversationId(uuid::Uuid::nil());
        let cron = heartbeat_cron(60, &conv_id).unwrap();
        // 60-min interval → hourly, 0 % 60 = 0
        assert_eq!(cron, "0 0 */1 * * * *");
    }

    #[test]
    fn heartbeat_cron_2_hours_with_jitter() {
        let uuid = uuid::Uuid::from_u128(75);
        let conv_id = ConversationId(uuid);
        let cron = heartbeat_cron(120, &conv_id).unwrap();
        // 120-min interval → 2-hourly, 75 % 60 = 15
        assert_eq!(cron, "0 15 */2 * * * *");
    }

    #[test]
    fn heartbeat_cron_15_minute_interval() {
        let uuid = uuid::Uuid::from_u128(7);
        let conv_id = ConversationId(uuid);
        let cron = heartbeat_cron(15, &conv_id).unwrap();
        // 7 % 15 = 7 → starts at minute 7
        assert_eq!(cron, "0 7/15 * * * * *");
    }

    #[test]
    fn heartbeat_cron_zero_interval_clamped_to_1() {
        let conv_id = ConversationId(uuid::Uuid::nil());
        let cron = heartbeat_cron(0, &conv_id).unwrap();
        // 0 is clamped to 1, jitter range = 1, 0 % 1 = 0
        assert_eq!(cron, "0 0/1 * * * * *");
    }

    #[test]
    fn heartbeat_cron_non_expressible_returns_none() {
        let conv_id = ConversationId(uuid::Uuid::nil());
        // 90 minutes: > 59 and not a multiple of 60
        assert!(heartbeat_cron(90, &conv_id).is_none());
        // 45 minutes: doesn't divide evenly into 60 (irregular cadence)
        assert!(heartbeat_cron(45, &conv_id).is_none());
        // 75 minutes: > 59 and not a multiple of 60
        assert!(heartbeat_cron(75, &conv_id).is_none());
        // 7 minutes: not a divisor of 60
        assert!(heartbeat_cron(7, &conv_id).is_none());
        // Valid divisors of 60
        assert!(heartbeat_cron(5, &conv_id).is_some());
        assert!(heartbeat_cron(10, &conv_id).is_some());
        assert!(heartbeat_cron(15, &conv_id).is_some());
        assert!(heartbeat_cron(20, &conv_id).is_some());
        assert!(heartbeat_cron(30, &conv_id).is_some());
    }

    #[test]
    fn find_heartbeat_for_conversation_found() {
        let conv_id = ConversationId::new();
        let mut task = ScheduledTask::new(
            "heartbeat".into(),
            "0 */30 * * * * *".into(),
            ScheduledAction::ResumeConversation {
                conversation_id: conv_id,
                prompt: String::new(),
            },
        )
        .unwrap();
        task.kind = TaskKind::Heartbeat;

        let tasks = vec![task.clone()];
        let found = find_heartbeat_for_conversation(&tasks, &conv_id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, task.id);
    }

    #[test]
    fn find_heartbeat_for_conversation_wrong_id() {
        let conv_id = ConversationId::new();
        let other_id = ConversationId::new();
        let mut task = ScheduledTask::new(
            "heartbeat".into(),
            "0 */30 * * * * *".into(),
            ScheduledAction::ResumeConversation {
                conversation_id: conv_id,
                prompt: String::new(),
            },
        )
        .unwrap();
        task.kind = TaskKind::Heartbeat;

        let tasks = vec![task];
        let found = find_heartbeat_for_conversation(&tasks, &other_id);
        assert!(found.is_none());
    }

    #[test]
    fn find_heartbeat_for_conversation_ignores_cron_tasks() {
        let conv_id = ConversationId::new();
        // A cron task (not heartbeat) with the same conversation_id
        let task = ScheduledTask::new(
            "not-heartbeat".into(),
            "0 */30 * * * * *".into(),
            ScheduledAction::ResumeConversation {
                conversation_id: conv_id,
                prompt: "do stuff".into(),
            },
        )
        .unwrap();
        // kind defaults to Cron

        let tasks = vec![task];
        let found = find_heartbeat_for_conversation(&tasks, &conv_id);
        assert!(found.is_none());
    }

    #[test]
    fn default_heartbeat_content_has_required_sections() {
        assert!(DEFAULT_HEARTBEAT_CONTENT.contains("# Heartbeat Instructions"));
        assert!(DEFAULT_HEARTBEAT_CONTENT.contains("## Pending Tasks"));
        assert!(DEFAULT_HEARTBEAT_CONTENT.contains("## Status Log"));
    }
}
