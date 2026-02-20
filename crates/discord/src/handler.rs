//! Discord message event handler.

use crate::bot::BotData;
use crate::chunking::chunk_message;
use crate::portals::resolve_or_create_portal;
use crate::security::is_authorized;
use std::sync::Arc;
use std::collections::HashMap;
use threshold_conversation::ConversationEvent;
use threshold_core::{ConversationId, PortalId, ThresholdError};
use tokio::sync::RwLock;

/// Track active portal listeners
type PortalListeners = Arc<RwLock<HashMap<PortalId, tokio::task::JoinHandle<()>>>>;

lazy_static::lazy_static! {
    static ref PORTAL_LISTENERS: PortalListeners = Arc::new(RwLock::new(HashMap::new()));
}

/// Event handler for Discord events
pub async fn event_handler(
    ctx: &serenity::all::Context,
    event: &serenity::all::FullEvent,
    _framework: poise::FrameworkContext<'_, BotData, ThresholdError>,
    data: &BotData,
) -> Result<(), ThresholdError> {
    if let serenity::all::FullEvent::Message { new_message: msg } = event {
        handle_message(ctx, msg, data).await?;
    }
    Ok(())
}

/// Handle incoming Discord message
async fn handle_message(
    ctx: &serenity::all::Context,
    msg: &serenity::all::Message,
    data: &BotData,
) -> Result<(), ThresholdError> {
    // 1. Ignore bot messages (including our own)
    if msg.author.bot {
        return Ok(());
    }

    // 2. Authorization check
    let guild_id = msg.guild_id.map(|g| g.get());
    if !is_authorized(&data.config, guild_id, msg.author.id.get()) {
        return Ok(());
    }

    // 3. Find or create portal for this channel
    let portal_id = resolve_or_create_portal(
        &data.engine,
        guild_id.unwrap_or(0),
        msg.channel_id.get(),
    )
    .await;

    // 4. Ensure portal listener is running
    ensure_portal_listener(
        portal_id,
        msg.channel_id,
        ctx.http.clone(),
        data.engine.clone(),
        data.outbound.clone(),
    )
    .await;

    // 5. Spawn engine call as background task — return immediately so the
    //    handler is not blocked for the entire duration of the CLI invocation.
    //    The response reaches Discord through the portal listener (broadcast events).
    let engine = data.engine.clone();
    let content = msg.content.clone();
    let http = ctx.http.clone();
    let channel_id = msg.channel_id;
    tokio::spawn(async move {
        // Typing indicator lives inside the spawned task so it persists
        // for the entire duration of the CLI invocation.
        let _typing = channel_id.start_typing(&http);
        if let Err(e) = engine.handle_message(&portal_id, &content).await {
            tracing::error!(
                error = %e,
                portal_id = ?portal_id,
                "Background message handling failed"
            );
        }
    });

    Ok(())
}

/// Ensure a portal listener is running for this portal
async fn ensure_portal_listener(
    portal_id: PortalId,
    channel_id: serenity::all::ChannelId,
    http: Arc<serenity::all::Http>,
    engine: Arc<threshold_conversation::ConversationEngine>,
    outbound: Arc<crate::outbound::DiscordOutbound>,
) {
    let mut listeners = PORTAL_LISTENERS.write().await;

    // Check if listener already exists
    if listeners.contains_key(&portal_id) {
        return;
    }

    // Get initial conversation ID for this portal
    let conversation_id = {
        let portals_arc = engine.portals();
        let portals = portals_arc.read().await;
        portals
            .get(&portal_id)
            .map(|p| p.conversation_id)
            .expect("Portal should exist")
    };

    // Spawn listener task
    let receiver = engine.subscribe();
    let handle = tokio::spawn(portal_listener(
        portal_id,
        conversation_id,
        channel_id,
        receiver,
        http,
        outbound,
    ));

    listeners.insert(portal_id, handle);

    tracing::debug!(
        portal_id = ?portal_id,
        conversation_id = ?conversation_id,
        "Started portal listener"
    );
}

/// Background listener for a portal
///
/// Subscribes to engine events and sends responses back to Discord.
/// Dynamically tracks conversation_id via PortalAttached events.
async fn portal_listener(
    portal_id: PortalId,
    mut conversation_id: ConversationId,
    channel_id: serenity::all::ChannelId,
    mut receiver: tokio::sync::broadcast::Receiver<ConversationEvent>,
    http: Arc<serenity::all::Http>,
    outbound: Arc<crate::outbound::DiscordOutbound>,
) {
    loop {
        let event = match receiver.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Receiver fell behind — log and continue
                tracing::warn!(
                    portal_id = ?portal_id,
                    lagged = n,
                    "Portal listener lagged, skipped events"
                );
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!(
                    portal_id = ?portal_id,
                    "Portal listener shutting down (channel closed)"
                );
                break;
            }
        };

        match event {
            // Track conversation changes
            ConversationEvent::PortalAttached {
                portal_id: pid,
                conversation_id: cid,
            } if pid == portal_id => {
                conversation_id = cid;
                tracing::debug!(
                    portal_id = ?portal_id,
                    conversation_id = ?conversation_id,
                    "Portal switched conversation"
                );
            }

            // Send assistant messages
            ConversationEvent::AssistantMessage {
                conversation_id: cid,
                content,
                artifacts,
                ..
            } if cid == conversation_id => {
                if artifacts.is_empty() {
                    // Send as chunked text messages
                    for chunk in chunk_message(&content, 2000) {
                        if let Err(e) = channel_id.say(&http, &chunk).await {
                            tracing::error!(
                                error = %e,
                                portal_id = ?portal_id,
                                "Failed to send message chunk"
                            );
                        }
                    }
                } else {
                    // Send with file attachments
                    let files: Vec<(String, Vec<u8>)> = artifacts
                        .iter()
                        .map(|a| (a.name.clone(), a.data.clone()))
                        .collect();

                    if let Err(e) = outbound
                        .send_with_attachments(channel_id.get(), &content, files)
                        .await
                    {
                        tracing::error!(
                            error = %e,
                            portal_id = ?portal_id,
                            "Failed to send message with attachments"
                        );
                    }
                }
            }

            // Send errors
            ConversationEvent::Error {
                conversation_id: cid,
                error,
            } if cid == conversation_id => {
                let error_msg = format!("❌ Error: {}", error);
                if let Err(e) = channel_id.say(&http, &error_msg).await {
                    tracing::error!(
                        error = %e,
                        portal_id = ?portal_id,
                        "Failed to send error message"
                    );
                }
            }

            _ => {
                // Ignore events for other conversations/portals
            }
        }
    }
}
