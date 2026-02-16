//! Threshold server - main binary that wires everything together.

use std::sync::Arc;
use threshold_cli_wrapper::ClaudeClient;
use threshold_conversation::ConversationEngine;
use threshold_core::config::ThresholdConfig;
use threshold_core::{init_logging, SecretStore, ThresholdError};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Load config
    let config = ThresholdConfig::load()?;

    // 2. Initialize logging
    init_logging(
        config.log_level.as_deref().unwrap_or("info"),
        &config.data_dir()?.join("logs"),
    )?;

    tracing::info!("Threshold starting...");

    // 3. Initialize secret store
    let secrets = Arc::new(SecretStore::new());

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

    // 5. Create conversation engine
    let engine = Arc::new(ConversationEngine::new(&config, claude.clone()).await?);
    tracing::info!("Conversation engine initialized.");

    // 6. Shared cancellation token for graceful shutdown
    let cancel = CancellationToken::new();

    // 7. Shared outbound handle — populated by Discord setup, used by
    //    heartbeat and scheduler. Wrapped in Arc<RwLock<Option<...>>> so
    //    it can be set after Discord connects.
    let discord_outbound: Arc<RwLock<Option<Arc<threshold_discord::DiscordOutbound>>>> =
        Arc::new(RwLock::new(None));

    // 8. Build all tasks as futures

    // Discord task
    let discord_handle = {
        let engine = engine.clone();
        let outbound_slot = discord_outbound.clone();
        let cancel = cancel.clone();
        let discord_config_opt: Option<threshold_core::config::DiscordConfig> = config.discord.clone();
        async move {
            if let Some(discord_config) = discord_config_opt {
                let token = secrets
                    .resolve("discord-bot-token", "DISCORD_BOT_TOKEN")?
                    .ok_or(ThresholdError::SecretNotFound {
                        key: "discord-bot-token".into(),
                    })?;

                tracing::info!("Starting Discord bot...");

                // build_and_start returns outbound immediately after setup,
                // then spawns event loop in background
                let outbound = threshold_discord::build_and_start(
                    engine,
                    discord_config.clone(),
                    &token,
                    cancel.clone(),
                )
                .await?;

                // Publish outbound for heartbeat/scheduler to use
                *outbound_slot.write().await = Some(outbound);

                tracing::info!("Discord bot ready.");
            }

            // Keep task alive until cancellation
            cancel.cancelled().await;
            Ok::<(), anyhow::Error>(())
        }
    };

    // Heartbeat task (Milestone 6 — no-op until implemented)
    let heartbeat_handle = {
        let cancel = cancel.clone();
        let _outbound = discord_outbound.clone();
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
            if let Err(e) = r {
                tracing::error!("Discord error: {}", e);
            }
        }
        r = heartbeat_handle => {
            if let Err(e) = r {
                tracing::error!("Heartbeat error: {}", e);
            }
        }
        r = scheduler_handle => {
            if let Err(e) = r {
                tracing::error!("Scheduler error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown signal received.");
        }
    }

    // 10. Graceful shutdown
    cancel.cancel(); // Signal all tasks to stop
    engine.save_state().await?;
    tracing::info!("Threshold shut down cleanly.");

    Ok(())
}
