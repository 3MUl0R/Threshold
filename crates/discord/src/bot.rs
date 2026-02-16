//! Discord bot setup and framework builder.

use crate::commands;
use crate::handler::event_handler;
use crate::outbound::DiscordOutbound;
use crate::security::is_authorized;
use std::sync::Arc;
use threshold_conversation::ConversationEngine;
use threshold_core::config::DiscordConfig;
use threshold_core::{Result, ThresholdError};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Bot data shared across all commands and handlers
pub struct BotData {
    pub engine: Arc<ConversationEngine>,
    pub config: DiscordConfig,
    pub outbound: Arc<DiscordOutbound>,
}

pub type Context<'a> = poise::Context<'a, BotData, ThresholdError>;

/// Build and start the Discord bot.
///
/// Returns the DiscordOutbound handle immediately after setup,
/// then spawns the event loop in a background task.
pub async fn build_and_start(
    engine: Arc<ConversationEngine>,
    config: DiscordConfig,
    token: &str,
    cancel: CancellationToken,
) -> Result<Arc<DiscordOutbound>> {
    // Shared slot for outbound handle - populated by setup closure
    let outbound_slot: Arc<RwLock<Option<Arc<DiscordOutbound>>>> = Arc::new(RwLock::new(None));
    let outbound_slot_setup = outbound_slot.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::general(),
                commands::coding(),
                commands::research(),
                commands::conversations(),
                commands::join(),
            ],
            event_handler: |ctx, event, framework, data| {
                Box::pin(event_handler(ctx, event, framework, data))
            },
            pre_command: |ctx| Box::pin(pre_command(ctx)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, _framework| {
            let outbound_slot_inner = outbound_slot_setup.clone();
            Box::pin(async move {
                // Register slash commands globally
                poise::builtins::register_globally(ctx, &_framework.options().commands)
                    .await
                    .map_err(|e| ThresholdError::Discord(e.to_string()))?;

                // Create outbound handle
                let outbound = Arc::new(DiscordOutbound::new(ctx.http.clone()));

                // Publish to shared slot
                *outbound_slot_inner.write().await = Some(outbound.clone());

                tracing::info!("Discord bot initialized, registering commands...");

                Ok(BotData {
                    engine,
                    config,
                    outbound,
                })
            })
        })
        .build();

    // Configure gateway intents
    let intents = serenity::all::GatewayIntents::GUILD_MESSAGES
        | serenity::all::GatewayIntents::MESSAGE_CONTENT
        | serenity::all::GatewayIntents::DIRECT_MESSAGES;

    // Build the client
    let mut client = serenity::Client::builder(token, intents)
        .framework(framework)
        .await
        .map_err(|e| ThresholdError::External(format!("Failed to create Discord client: {}", e)))?;

    // Start the client which will trigger setup and populate outbound_slot
    // We need to wait for setup to complete before extracting outbound
    tokio::spawn(async move {
        tokio::select! {
            result = client.start() => {
                if let Err(e) = result {
                    tracing::error!("Discord client error: {}", e);
                }
            }
            _ = cancel.cancelled() => {
                tracing::info!("Discord bot shutting down...");
            }
        }
    });

    // Wait for setup to populate outbound_slot (with timeout)
    let outbound = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            {
                let slot = outbound_slot.read().await;
                if let Some(outbound) = slot.as_ref() {
                    return outbound.clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| ThresholdError::External("Timeout waiting for Discord bot setup".to_string()))?;

    Ok(outbound)
}

/// Pre-command hook for authorization and logging
async fn pre_command(ctx: Context<'_>) {
    let guild_id = ctx.guild_id().map(|g| g.get());
    let user_id = ctx.author().id.get();

    // Authorization check
    if !is_authorized(&ctx.data().config, guild_id, user_id) {
        tracing::warn!(
            "Unauthorized command attempt from user {} in guild {:?}",
            user_id,
            guild_id
        );
        // Silently ignore unauthorized commands
        // Note: Command execution will be blocked by poise if we don't proceed
        return;
    }

    // Log command invocation
    tracing::info!(
        command = ctx.command().name,
        user_id = user_id,
        guild_id = ?guild_id,
        "Command invoked"
    );
}
