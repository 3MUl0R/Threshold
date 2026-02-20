//! Slash commands for mode switching and conversation management.

use crate::bot::Context;
use crate::portals::resolve_or_create_portal;
use threshold_core::{ConversationId, ConversationMode, PortalId, ThresholdError};

type Result = std::result::Result<(), ThresholdError>;

/// Helper function to resolve portal for current context
async fn resolve_portal(ctx: Context<'_>) -> PortalId {
    let guild_id = ctx.guild_id().map(|g| g.get()).unwrap_or(0);
    let channel_id = ctx.channel_id().get();
    resolve_or_create_portal(&ctx.data().engine, guild_id, channel_id).await
}

/// Switch to the General conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn general(ctx: Context<'_>) -> Result {
    let portal_id = resolve_portal(ctx).await;
    ctx.data()
        .engine
        .switch_mode(&portal_id, ConversationMode::General)
        .await?;

    ctx.say("Switched to **General** conversation.").await.ok();
    Ok(())
}

/// Start or resume a coding conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn coding(
    ctx: Context<'_>,
    #[description = "Project name"] project: String,
) -> Result {
    let portal_id = resolve_portal(ctx).await;
    let mode = ConversationMode::Coding {
        project: project.clone(),
    };

    ctx.data().engine.switch_mode(&portal_id, mode).await?;

    ctx.say(format!("Switched to **Coding** conversation for `{}`.", project))
        .await
        .ok();
    Ok(())
}

/// Start or resume a research conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn research(
    ctx: Context<'_>,
    #[description = "Research topic"] topic: String,
) -> Result {
    let portal_id = resolve_portal(ctx).await;
    let mode = ConversationMode::Research {
        topic: topic.clone(),
    };

    ctx.data().engine.switch_mode(&portal_id, mode).await?;

    ctx.say(format!("Switched to **Research** conversation for `{}`.", topic))
        .await
        .ok();
    Ok(())
}

/// List all active conversations.
#[poise::command(slash_command, prefix_command)]
pub async fn conversations(ctx: Context<'_>) -> Result {
    let convs = ctx.data().engine.list_conversations().await;

    if convs.is_empty() {
        ctx.say("No active conversations.").await.ok();
        return Ok(());
    }

    let mut msg = String::from("**Active Conversations:**\n");
    for c in &convs {
        msg.push_str(&format!(
            "- `{}` — {} (last active: {})\n",
            c.id.0,
            c.mode.key(),
            c.last_active.format("%Y-%m-%d %H:%M")
        ));
    }

    ctx.say(msg).await.ok();
    Ok(())
}

/// Abort the running task for this channel's conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn abort(ctx: Context<'_>) -> Result {
    let portal_id = resolve_portal(ctx).await;
    let conversation_id = ctx
        .data()
        .engine
        .get_portal_conversation(&portal_id)
        .await?;

    match ctx
        .data()
        .engine
        .claude()
        .tracker()
        .abort_conversation(&conversation_id)
        .await
    {
        Ok(run_id) => {
            ctx.say(format!("Aborting task {}...", run_id))
                .await
                .ok();
        }
        Err(_) => {
            ctx.say("Nothing to abort — no task is running for this conversation.")
                .await
                .ok();
        }
    }

    Ok(())
}

/// Join a specific conversation by ID.
#[poise::command(slash_command, prefix_command)]
pub async fn join(
    ctx: Context<'_>,
    #[description = "Conversation ID"] id: String,
) -> Result {
    // Parse UUID from string
    let conversation_id = id
        .parse::<uuid::Uuid>()
        .map(ConversationId)
        .map_err(|_| ThresholdError::Config(format!("Invalid conversation ID: {}", id)))?;

    let portal_id = resolve_portal(ctx).await;

    ctx.data()
        .engine
        .join_conversation(&portal_id, &conversation_id)
        .await?;

    ctx.say(format!("Joined conversation `{}`.", id)).await.ok();
    Ok(())
}
