//! Slash commands for mode switching and conversation management.

use crate::bot::{BotData, Context};
use threshold_core::{ConversationMode, ThresholdError};

type Result = std::result::Result<(), ThresholdError>;

/// Switch to the General conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn general(ctx: Context<'_>) -> Result {
    // TODO: Phase 4.5 implementation
    ctx.say("Not implemented yet").await.ok();
    Ok(())
}

/// Start or resume a coding conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn coding(
    ctx: Context<'_>,
    #[description = "Project name"] project: String,
) -> Result {
    // TODO: Phase 4.5 implementation
    ctx.say("Not implemented yet").await.ok();
    Ok(())
}

/// Start or resume a research conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn research(
    ctx: Context<'_>,
    #[description = "Research topic"] topic: String,
) -> Result {
    // TODO: Phase 4.5 implementation
    ctx.say("Not implemented yet").await.ok();
    Ok(())
}

/// List all active conversations.
#[poise::command(slash_command, prefix_command)]
pub async fn conversations(ctx: Context<'_>) -> Result {
    // TODO: Phase 4.5 implementation
    ctx.say("Not implemented yet").await.ok();
    Ok(())
}

/// Join a specific conversation by ID.
#[poise::command(slash_command, prefix_command)]
pub async fn join(
    ctx: Context<'_>,
    #[description = "Conversation ID"] id: String,
) -> Result {
    // TODO: Phase 4.5 implementation
    ctx.say("Not implemented yet").await.ok();
    Ok(())
}
