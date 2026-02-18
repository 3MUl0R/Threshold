//! Threshold — CLI binary and daemon entry point.
//!
//! Provides a clap-based CLI with:
//! - `threshold daemon` — Start the background daemon
//! - `threshold schedule <subcommand>` — Manage scheduled tasks (Milestone 6)

mod daemon_client;
mod gmail;
mod imagegen;
mod output;
mod schedule;

use clap::Parser;

/// Threshold — orchestrate Claude CLI sessions with Discord, scheduling, and tools.
#[derive(Parser)]
#[command(name = "threshold", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Start the Threshold daemon (Discord bot, scheduler, heartbeat)
    Daemon(DaemonArgs),
    /// Manage scheduled tasks (requires running daemon)
    Schedule {
        #[command(subcommand)]
        command: schedule::ScheduleCommands,
    },
    /// Gmail integration — read, search, and send email
    Gmail(threshold_gmail::GmailArgs),
    /// Image generation — create images from text descriptions
    Imagegen(threshold_imagegen::ImagegenArgs),
}

/// Arguments for the daemon subcommand.
#[derive(clap::Args)]
struct DaemonArgs {
    /// Path to the configuration file (overrides THRESHOLD_CONFIG env var)
    #[arg(short, long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon(args) => run_daemon(args).await,
        Commands::Schedule { command } => schedule::handle_schedule_command(command).await,
        Commands::Gmail(args) => gmail::handle_gmail_command(args).await,
        Commands::Imagegen(args) => imagegen::handle_imagegen_command(args).await,
    }
}

/// Run the Threshold daemon.
///
/// This contains the full daemon lifecycle extracted from the original main():
/// config loading, logging, Discord bot, heartbeat, scheduler, and graceful shutdown.
async fn run_daemon(args: DaemonArgs) -> anyhow::Result<()> {
    use std::sync::Arc;
    use threshold_cli_wrapper::ClaudeClient;
    use threshold_conversation::ConversationEngine;
    use threshold_core::config::ThresholdConfig;
    use threshold_core::{init_logging, SecretStore, ThresholdError};
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    // 1. Load config (from explicit path or default)
    let config_path = match &args.config {
        Some(path) => std::path::PathBuf::from(path),
        None => std::env::var("THRESHOLD_CONFIG")
            .map(std::path::PathBuf::from)
            .unwrap_or(ThresholdConfig::default_config_path()?),
    };
    let config = match &args.config {
        Some(path) => ThresholdConfig::load_from(std::path::Path::new(path))?,
        None => ThresholdConfig::load()?,
    };
    let config = Arc::new(config);

    // 2. Initialize logging (keep guard alive for entire program)
    let _log_guard = init_logging(
        config.log_level.as_deref().unwrap_or("info"),
        &config.data_dir()?.join("logs"),
    )?;

    tracing::info!("Threshold starting...");

    // 3. Initialize secret store
    let secrets = Arc::new(SecretStore::new()?);

    // 4. Create Claude CLI client
    let claude = Arc::new(
        ClaudeClient::new(
            config
                .cli
                .claude
                .command
                .clone()
                .unwrap_or_else(|| "claude".to_string()),
            config.data_dir()?.join("cli-sessions"),
            config.cli.claude.skip_permissions.unwrap_or(false),
        )
        .await?,
    );
    tracing::info!("Claude CLI client configured.");

    // 5. Build tool prompt and create conversation engine
    let tool_prompt = {
        let prompt = threshold_tools::build_tool_prompt(&config);
        if prompt.is_empty() { None } else { Some(prompt) }
    };
    let active_conversations = Arc::new(threshold_core::ActiveConversations::new());
    let engine = Arc::new(
        ConversationEngine::new(
            &config,
            claude.clone(),
            tool_prompt,
            Some(active_conversations.clone()),
        )
        .await?,
    );
    tracing::info!("Conversation engine initialized.");

    // 5b. Startup migration warnings
    {
        let data_dir_path = config.data_dir()?;
        // Warn if global heartbeat.md exists (legacy)
        let global_heartbeat = data_dir_path.join("heartbeat.md");
        if global_heartbeat.exists() {
            tracing::warn!(
                "Global heartbeat.md found at {}. Per-conversation heartbeats are now \
                 configured via /heartbeat enable. See docs for migration.",
                global_heartbeat.display()
            );
        }
    }

    // 6. Shared cancellation token for graceful shutdown
    let cancel = CancellationToken::new();

    // 7. Create scheduler up front (if enabled) to get a SchedulerHandle
    //    that Discord can use for /schedule commands.
    let data_dir = config.data_dir()?;
    let scheduler_enabled = config.scheduler.as_ref().is_some_and(|s| s.enabled);
    let store_path = config
        .scheduler
        .as_ref()
        .and_then(|s| s.store_path.as_ref())
        .map(|p| threshold_core::resolve_path(p, &data_dir))
        .unwrap_or_else(|| data_dir.join("state").join("schedules.json"));

    // Create scheduler (loads persisted tasks) — result sender wired later
    let (scheduler_instance, scheduler_cmd_handle) = if scheduler_enabled {
        let (mut sched, handle) = threshold_scheduler::Scheduler::new(
            store_path,
            claude.clone(),
            engine.clone(),
            None, // result sender wired after Discord starts
            active_conversations.clone(),
            cancel.clone(),
        )
        .await;

        // Validate heartbeat tasks against conversation store (remove orphaned)
        sched.validate_heartbeat_tasks().await;

        (Some(sched), Some(handle))
    } else {
        (None, None)
    };

    // 8. Shared outbound handle — populated by Discord setup, used by
    //    heartbeat and scheduler. Wrapped in Arc<RwLock<Option<...>>> so
    //    it can be set after Discord connects.
    let discord_outbound: Arc<RwLock<Option<Arc<threshold_discord::DiscordOutbound>>>> =
        Arc::new(RwLock::new(None));

    // 9. Build all tasks as futures

    // Discord task
    let discord_handle = {
        let engine = engine.clone();
        let outbound_slot = discord_outbound.clone();
        let cancel = cancel.clone();
        let discord_config_opt = config.discord.clone();
        let scheduler_handle_for_discord = scheduler_cmd_handle.clone();
        let secrets = secrets.clone();
        async move {
            if let Some(discord_config) = discord_config_opt {
                // Wrap synchronous keychain access in spawn_blocking to avoid
                // blocking the tokio runtime (macOS keychain can prompt/hang).
                let token = tokio::task::spawn_blocking({
                    let secrets = secrets.clone();
                    move || secrets.resolve("discord-bot-token", "DISCORD_BOT_TOKEN")
                })
                .await??
                .ok_or(ThresholdError::SecretNotFound {
                    key: "discord-bot-token".into(),
                })?;

                tracing::info!("Starting Discord bot...");

                let outbound = threshold_discord::build_and_start(
                    engine,
                    discord_config.clone(),
                    &token,
                    cancel.clone(),
                    scheduler_handle_for_discord,
                )
                .await?;

                *outbound_slot.write().await = Some(outbound);
                tracing::info!("Discord bot ready.");
            }

            cancel.cancelled().await;
            Ok::<(), anyhow::Error>(())
        }
    };

    // Scheduler + Daemon API task
    let scheduler_handle = {
        let cancel = cancel.clone();
        let data_dir = data_dir.clone();
        let scheduler_cmd_handle = scheduler_cmd_handle.clone();
        let discord_outbound_for_scheduler = discord_outbound.clone();

        async move {
            let (mut scheduler, handle) = match (scheduler_instance, scheduler_cmd_handle) {
                (Some(sched), Some(handle)) => (sched, handle),
                _ => {
                    cancel.cancelled().await;
                    return Ok::<(), anyhow::Error>(());
                }
            };

            // Wait briefly for Discord to connect, then wire result sender
            {
                let outbound = discord_outbound_for_scheduler.clone();
                let cancel_clone = cancel.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = cancel_clone.cancelled() => break,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                                let slot = outbound.read().await;
                                if slot.is_some() {
                                    tracing::info!("Discord outbound available for scheduler result delivery");
                                    break;
                                }
                            }
                        }
                    }
                });
            }

            // Heartbeat tasks are now per-conversation, created via /heartbeat enable.
            // No global heartbeat startup — see Phase 12B.

            // Start daemon API in parallel with scheduler
            let socket_path =
                threshold_scheduler::daemon_api::DaemonApi::default_socket_path(&data_dir);
            let daemon_api =
                threshold_scheduler::daemon_api::DaemonApi::new(handle, socket_path);

            let daemon_cancel = cancel.clone();
            let daemon_handle = tokio::spawn(async move {
                if let Err(e) = daemon_api.run(daemon_cancel).await {
                    tracing::error!("Daemon API error: {}", e);
                }
            });

            // Wire result_sender once Discord outbound is available
            {
                let slot = discord_outbound_for_scheduler.read().await;
                if let Some(outbound) = slot.as_ref() {
                    scheduler.set_result_sender(outbound.clone());
                    tracing::info!("Scheduler result sender wired to Discord outbound");
                }
            }

            // Run scheduler main loop
            scheduler.run().await;

            // Wait for daemon API to shut down
            daemon_handle.await.ok();

            Ok::<(), anyhow::Error>(())
        }
    };

    // 9a. Wire ConversationDeleted → scheduler cleanup listener
    if let Some(sched_handle) = scheduler_cmd_handle.clone() {
        let mut event_rx = engine.subscribe();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = event_rx.recv() => {
                        match event {
                            Ok(threshold_conversation::ConversationEvent::ConversationDeleted { conversation_id }) => {
                                if let Err(e) = sched_handle.remove_tasks_for_conversation(conversation_id) {
                                    tracing::warn!(
                                        "Failed to forward conversation deletion to scheduler: {}",
                                        e
                                    );
                                }
                            }
                            Ok(_) => {} // ignore other events
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("Scheduler deletion listener lagged by {} events", n);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = cancel_clone.cancelled() => break,
                }
            }
        });
    }

    // 9b. Web interface task
    let web_handle = {
        let web_enabled = config
            .web
            .as_ref()
            .map(|w| w.enabled)
            .unwrap_or(false);
        let config = config.clone();
        let engine = engine.clone();
        let cancel = cancel.clone();
        let data_dir = data_dir.clone();
        let config_path = config_path.clone();
        let secrets = secrets.clone();
        let scheduler_cmd_handle = scheduler_cmd_handle.clone();
        async move {
            if !web_enabled {
                cancel.cancelled().await;
                return Ok::<(), anyhow::Error>(());
            }
            let templates = threshold_web::templates::build_template_env();
            let state = threshold_web::AppState {
                engine,
                scheduler_handle: scheduler_cmd_handle,
                secret_store: secrets,
                config,
                config_path,
                data_dir,
                cancel: cancel.clone(),
                start_time: chrono::Utc::now(),
                templates,
            };
            threshold_web::start_web_server(state).await?;
            Ok(())
        }
    };

    // 9c. Run all tasks concurrently, shut down on signal or error
    tokio::select! {
        r = discord_handle => {
            if let Err(e) = r {
                tracing::error!("Discord error: {}", e);
            }
        }
        r = scheduler_handle => {
            if let Err(e) = r {
                tracing::error!("Scheduler error: {}", e);
            }
        }
        r = web_handle => {
            if let Err(e) = r {
                tracing::error!("Web interface error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown signal received.");
        }
    }

    // 10. Graceful shutdown
    cancel.cancel();
    engine.save_state().await?;
    tracing::info!("Threshold shut down cleanly.");

    Ok(())
}
