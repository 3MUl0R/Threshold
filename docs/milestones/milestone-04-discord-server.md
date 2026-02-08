# Milestone 4 — Discord Portal + Server Binary

**Crates:** `discord`, `server`
**Complexity:** Large
**Dependencies:** Milestones 1–3 (core, cli-wrapper, conversation)

## What This Milestone Delivers

The Discord bot and the main server binary. At the end of this milestone,
Threshold is a **working Discord bot** that can hold conversations with a user,
switch modes, and maintain persistent sessions across restarts.

**This is the first runnable system.**

---

## Phase 4.1 — Discord Bot Setup (poise/serenity)

Build the Discord bot using the `poise` framework (which wraps `serenity`).

### `crates/discord/Cargo.toml` key deps

```toml
[dependencies]
poise = "0.6"
serenity = { version = "0.12", features = ["client", "gateway", "model"] }
threshold-core = { path = "../core" }
threshold-conversation = { path = "../conversation" }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
```

### `crates/discord/src/bot.rs`

```rust
pub struct BotData {
    pub engine: Arc<ConversationEngine>,
    pub config: DiscordConfig,
    pub outbound: Arc<DiscordOutbound>,
}

type Context<'a> = poise::Context<'a, BotData, ThresholdError>;
type FrameworkError<'a> = poise::FrameworkError<'a, BotData, ThresholdError>;

pub async fn build_and_run(
    engine: Arc<ConversationEngine>,
    config: DiscordConfig,
    token: &str,
) -> Result<()> {
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
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                // Initialize outbound
                let outbound = Arc::new(DiscordOutbound::new(ctx.http.clone()));
                Ok(BotData { engine, config, outbound })
            })
        })
        .build();

    let intents = serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT
        | serenity::GatewayIntents::DIRECT_MESSAGES;

    let mut client = serenity::Client::builder(token, intents)
        .framework(framework)
        .await?;

    client.start().await?;
    Ok(())
}
```

---

## Phase 4.2 — Security Middleware

Every message and command must pass the guild + user allowlist gate.

### `crates/discord/src/security.rs`

```rust
/// Check if a user is authorized to interact with the bot.
pub fn is_authorized(config: &DiscordConfig, guild_id: Option<u64>, user_id: u64) -> bool {
    // User must always be in the allowlist
    if !config.allowed_user_ids.contains(&user_id) {
        return false;
    }

    match guild_id {
        // Guild messages: must be the correct guild
        Some(gid) => gid == config.guild_id,
        // DMs: allowed if the user is in the allowlist (checked above)
        None => true,
    }
}
```

### Enforcement Points

1. **Message handler** — check before processing any message
2. **Poise pre_command** — check before executing any slash command
3. **DM handling** — DMs from allowlisted users are accepted (routed to
   General conversation); DMs from unknown users are silently ignored

Unauthorized messages are silently ignored (no error response to avoid leaking
that the bot exists to unauthorized users).

---

## Phase 4.3 — Message Handler

Listen for messages in Discord channels and route them through the
conversation engine.

### `crates/discord/src/handler.rs`

```rust
pub async fn event_handler(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    _framework: poise::FrameworkContext<'_, BotData, ThresholdError>,
    data: &BotData,
) -> Result<(), ThresholdError> {
    if let serenity::FullEvent::Message { new_message: msg } = event {
        handle_message(ctx, msg, data).await?;
    }
    Ok(())
}

async fn handle_message(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &BotData,
) -> Result<()> {
    // 1. Ignore bot messages (including our own)
    if msg.author.bot { return Ok(()); }

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
    ).await;

    // 4. Show typing indicator while processing
    let typing = msg.channel_id.start_typing(&ctx.http);

    // 5. Send message to conversation engine
    data.engine.handle_message(&portal_id, &msg.content).await?;

    // 6. Receive response from broadcast and send to Discord
    //    (handled by a background listener per portal — see Phase 4.6)

    Ok(())
}
```

### Response Delivery

A background task per active portal subscribes to the engine's broadcast
channel and sends responses back to Discord:

```rust
async fn portal_listener(
    portal_id: PortalId,
    conversation_id: ConversationId,
    channel_id: serenity::ChannelId,
    mut receiver: broadcast::Receiver<ConversationEvent>,
    http: Arc<serenity::Http>,
    outbound: Arc<DiscordOutbound>,
) {
    loop {
        let event = match receiver.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Receiver fell behind — log and continue, don't die
                tracing::warn!("Portal listener lagged, skipped {} events", n);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("Portal listener shutting down (channel closed)");
                break;
            }
        };

        match event {
            ConversationEvent::AssistantMessage {
                conversation_id: cid, content, artifacts, ..
            } if cid == conversation_id => {
                if artifacts.is_empty() {
                    for chunk in chunk_message(&content, 2000) {
                        channel_id.say(&http, &chunk).await.ok();
                    }
                } else {
                    // Send with file attachments (images, etc.)
                    let files: Vec<_> = artifacts.iter()
                        .map(|a| (a.name.clone(), a.data.clone()))
                        .collect();
                    outbound.send_with_attachments(
                        channel_id.get(), &content, files
                    ).await.ok();
                }
            }
            ConversationEvent::Error { conversation_id: cid, error, .. }
                if cid == conversation_id =>
            {
                channel_id.say(&http, format!("Error: {}", error)).await.ok();
            }
            _ => {} // Ignore events for other conversations
        }
    }
}
```

---

## Phase 4.4 — Message Chunking

Discord has a 2000-character message limit.

### `crates/discord/src/chunking.rs`

```rust
/// Split a message respecting Discord's character limit.
///
/// Split priorities:
/// 1. Paragraph boundary (double newline)
/// 2. Single newline
/// 3. Sentence boundary (. ! ?)
/// 4. Word boundary (space)
/// 5. Hard cut (last resort)
///
/// Never splits inside a markdown code block (``` ... ```).
pub fn chunk_message(content: &str, max_len: usize) -> Vec<String>;
```

### Rules

- Respect markdown code blocks — if a code block straddles a boundary, extend
  the chunk to include the closing ``` (or start a new code block in the next
  chunk)
- Trim whitespace from chunk boundaries
- Never produce empty chunks

---

## Phase 4.5 — Slash Commands

Poise slash commands for mode switching and conversation management.

### `crates/discord/src/commands.rs`

```rust
/// Switch to the General conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn general(ctx: Context<'_>) -> Result<(), ThresholdError> {
    let portal_id = resolve_portal(ctx).await;
    ctx.data().engine.switch_mode(&portal_id, ConversationMode::General).await?;
    ctx.say("Switched to **General** conversation.").await?;
    Ok(())
}

/// Start or resume a coding conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn coding(
    ctx: Context<'_>,
    #[description = "Project name"] project: String,
) -> Result<(), ThresholdError> {
    let portal_id = resolve_portal(ctx).await;
    let mode = ConversationMode::Coding { project: project.clone() };
    let conv_id = ctx.data().engine.switch_mode(&portal_id, mode).await?;
    ctx.say(format!("Switched to **Coding** conversation for `{}`.", project)).await?;
    Ok(())
}

/// Start or resume a research conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn research(
    ctx: Context<'_>,
    #[description = "Research topic"] topic: String,
) -> Result<(), ThresholdError> { /* similar pattern */ }

/// List all active conversations.
#[poise::command(slash_command, prefix_command)]
pub async fn conversations(ctx: Context<'_>) -> Result<(), ThresholdError> {
    let convs = ctx.data().engine.list_conversations().await;
    let mut msg = String::from("**Active Conversations:**\n");
    for c in &convs {
        msg.push_str(&format!(
            "- `{}` — {} (last active: {})\n",
            c.id.0, c.mode.key(), c.last_active.format("%Y-%m-%d %H:%M")
        ));
    }
    ctx.say(msg).await?;
    Ok(())
}

/// Join a specific conversation by ID.
#[poise::command(slash_command, prefix_command)]
pub async fn join(
    ctx: Context<'_>,
    #[description = "Conversation ID"] id: String,
) -> Result<(), ThresholdError> { /* parse UUID, call engine.join_conversation */ }
```

---

## Phase 4.6 — Channel-as-Portal Mapping

Each Discord channel automatically becomes a portal.

### `crates/discord/src/portals.rs`

```rust
/// Resolve an existing portal for this channel, or create a new one
/// attached to the General conversation.
pub async fn resolve_or_create_portal(
    engine: &ConversationEngine,
    guild_id: u64,
    channel_id: u64,
) -> PortalId {
    let portals = engine.portals().read().await;
    if let Some(portal) = portals.find_by_discord_channel(guild_id, channel_id) {
        return portal.id;
    }
    drop(portals);

    // Create new portal attached to General conversation
    engine.register_portal(PortalType::Discord { guild_id, channel_id }).await
}
```

New channels start attached to the General conversation. The user can switch
via `/coding`, `/research`, etc.

---

## Phase 4.7 — Agent-Initiated Discord Actions

Allow the system to push messages to Discord (for heartbeat, cron, etc.).

### `crates/discord/src/outbound.rs`

```rust
pub struct DiscordOutbound {
    http: Arc<serenity::Http>,
}

impl DiscordOutbound {
    pub fn new(http: Arc<serenity::Http>) -> Self;

    /// Send a text message to a channel.
    pub async fn send_to_channel(&self, channel_id: u64, content: &str) -> Result<()>;

    /// Send a DM to a user.
    pub async fn send_dm(&self, user_id: u64, content: &str) -> Result<()>;

    /// Create a new text channel in the guild.
    pub async fn create_channel(
        &self,
        guild_id: u64,
        name: &str,
        topic: &str,
    ) -> Result<u64>;

    /// Send a message with file attachments (images, etc.).
    pub async fn send_with_attachments(
        &self,
        channel_id: u64,
        content: &str,
        attachments: Vec<(String, Vec<u8>)>,  // (filename, data)
    ) -> Result<()>;
}
```

---

## Phase 4.8 — Server Binary

The `server` crate is the main binary that wires everything together.

### `crates/server/src/main.rs`

The server runs Discord, heartbeat, and scheduler as **concurrent tasks** via
`tokio::select!`. The `DiscordOutbound` is created during Discord setup and
shared with heartbeat/scheduler via `Arc`. This avoids the wiring problem
where later milestones need resources created inside the Discord bot setup.

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Load config
    let config = ThresholdConfig::load()?;

    // 2. Initialize logging
    init_logging(
        config.log_level.as_deref().unwrap_or("info"),
        &config.data_dir().join("logs"),
    )?;

    tracing::info!("Threshold starting...");

    // 3. Initialize secret store
    let secrets = Arc::new(SecretStore::new());

    // 4. Verify Claude CLI is installed
    let claude = Arc::new(ClaudeClient::new(&config.cli.claude)?);
    claude.health_check().await?;
    tracing::info!("Claude CLI verified.");

    // 5. Create conversation engine
    let engine = Arc::new(ConversationEngine::new(&config, claude.clone())?);
    tracing::info!("Conversation engine initialized.");

    // 6. Shared cancellation token for graceful shutdown
    let cancel = CancellationToken::new();

    // 7. Shared outbound handle — populated by Discord setup, used by
    //    heartbeat and scheduler. Wrapped in Arc<RwLock<Option<...>>> so
    //    it can be set after Discord connects.
    let discord_outbound: Arc<RwLock<Option<Arc<DiscordOutbound>>>> =
        Arc::new(RwLock::new(None));

    // 8. Build all tasks as futures

    // Discord task
    let discord_handle = {
        let engine = engine.clone();
        let outbound_slot = discord_outbound.clone();
        let cancel = cancel.clone();
        async move {
            if let Some(discord_config) = &config.discord {
                let token = secrets.resolve("discord-bot-token", "DISCORD_BOT_TOKEN")
                    .ok_or(ThresholdError::SecretNotFound {
                        key: "discord-bot-token".into()
                    })?;
                tracing::info!("Starting Discord bot...");

                // build_and_start returns the outbound handle and runs until cancelled
                let outbound = discord::build_and_start(
                    engine, discord_config.clone(), &token, cancel,
                ).await?;

                // Publish outbound for heartbeat/scheduler to use
                *outbound_slot.write().await = Some(outbound);
            }
            Ok::<(), anyhow::Error>(())
        }
    };

    // Heartbeat task (Milestone 6 — no-op until implemented)
    let heartbeat_handle = {
        let cancel = cancel.clone();
        let outbound = discord_outbound.clone();
        async move {
            // When milestone 6 is implemented:
            // let outbound = outbound.read().await.clone();
            // HeartbeatRunner::new(..., outbound).run(cancel).await;
            cancel.cancelled().await;
            Ok::<(), anyhow::Error>(())
        }
    };

    // Scheduler task (Milestone 7 — no-op until implemented)
    let scheduler_handle = {
        let cancel = cancel.clone();
        async move {
            cancel.cancelled().await;
            Ok::<(), anyhow::Error>(())
        }
    };

    // 9. Run all tasks concurrently, shut down on signal or error
    tokio::select! {
        r = discord_handle => {
            if let Err(e) = r { tracing::error!("Discord error: {}", e); }
        }
        r = heartbeat_handle => {
            if let Err(e) = r { tracing::error!("Heartbeat error: {}", e); }
        }
        r = scheduler_handle => {
            if let Err(e) = r { tracing::error!("Scheduler error: {}", e); }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown signal received.");
        }
    }

    // 10. Graceful shutdown
    cancel.cancel();  // Signal all tasks to stop
    engine.save_state().await?;
    tracing::info!("Threshold shut down cleanly.");

    Ok(())
}
```

### Key Design Decision: Shared `DiscordOutbound`

The `DiscordOutbound` (which wraps serenity's `Http` client) is created
during Discord bot setup and published into an `Arc<RwLock<Option<...>>>`.
The heartbeat and scheduler read from this slot. If Discord isn't configured,
the slot stays `None` and those subsystems skip Discord delivery.

This avoids the circular dependency where heartbeat/scheduler need
`DiscordOutbound`, but `DiscordOutbound` is only created inside the Discord
setup closure.

### Startup Verification Sequence

```
1. Load config                          → fail fast if config is missing/invalid
2. Init logging                         → get logs ASAP
3. Init secret store                    → fail fast if keychain unavailable
4. Verify CLI: claude --version         → fail fast if not installed
5. Create conversation engine           → load persisted state
6. Resolve Discord token from secrets   → fail fast if token missing
7. Spawn concurrent tasks               → Discord, heartbeat, scheduler
8. Log readiness                        → "Threshold is ready."
```

---

## Crate Module Structures

### `crates/discord/src/`
```
lib.rs            — re-exports build_and_run, DiscordOutbound
bot.rs            — bot setup, framework builder
security.rs       — authorization check
handler.rs        — message event handler
commands.rs       — slash commands (/general, /coding, /research, etc.)
chunking.rs       — message chunking for 2000-char limit
portals.rs        — channel-to-portal mapping
outbound.rs       — agent-initiated Discord actions
```

### `crates/server/src/`
```
main.rs           — entry point, wiring, shutdown
```

---

## Verification Checklist

- [ ] `cargo run --bin threshold` starts and connects to Discord
- [ ] Bot responds to messages from allowlisted users in the correct guild
- [ ] Bot ignores messages from unauthorized users (wrong guild or not in list)
- [ ] Bot ignores its own messages and other bot messages
- [ ] Typing indicator shows while processing
- [ ] `/coding myproject` switches conversation, subsequent messages use coding session
- [ ] `/general` switches back, coding session persists (can return to it)
- [ ] `/conversations` lists all active conversations with IDs
- [ ] `/join <id>` attaches the channel to a specific conversation
- [ ] Long responses (>2000 chars) are split into multiple messages correctly
- [ ] Code blocks are preserved across message chunks
- [ ] State persists across server restarts (conversations, portals, sessions)
- [ ] Graceful shutdown saves all state
- [ ] Server fails fast with clear error if config is missing
- [ ] Server fails fast with clear error if Discord token is missing
- [ ] Server fails fast with clear error if Claude CLI is not installed
