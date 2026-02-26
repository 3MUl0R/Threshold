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

use std::path::{Path, PathBuf};

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
    use threshold_core::{init_logging, DaemonState, HealthConfig, SecretStore, ThresholdError};
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

    // 3. PID file: check for existing daemon, then write our PID
    let data_dir = config.data_dir()?;
    check_existing_daemon(&data_dir)?;
    let pid_path = write_pid_file(&data_dir)?;

    // 3b. Create shared daemon state for drain checks and active work tracking
    let daemon_state = Arc::new(DaemonState::new());
    let health_config = HealthConfig {
        pid: std::process::id(),
        started_at: chrono::Utc::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    // 3c. Export data dir and config path as env vars for child processes.
    // These are consumed by CLI subcommands (e.g. `threshold schedule list`)
    // connecting back to the daemon. We set them early, before any subsystem
    // spawns child processes.
    //
    // SAFETY: While tokio worker threads exist at this point, no code in
    // this process concurrently reads THRESHOLD_DATA_DIR or THRESHOLD_CONFIG.
    // They are only consumed by child processes (Command::new) spawned later.
    // No concurrent iterator over std::env::vars() is active either.
    unsafe {
        std::env::set_var("THRESHOLD_DATA_DIR", &data_dir);
        std::env::set_var("THRESHOLD_CONFIG", &config_path);
    }

    // 4. Initialize secret store
    let secrets = Arc::new(SecretStore::with_backend(
        config.secret_backend(),
        Some(data_dir.clone()),
    )?);
    tracing::info!("Secret store backend: {}", secrets.backend_name());
    if secrets.backend_name() == "file" {
        let secrets_path = data_dir.join("secrets.toml");
        if !secrets_path.exists() {
            tracing::info!(
                "No secrets.toml found. Set credentials via the web UI at /config/credentials \
                 or switch to keychain backend with secret_backend = \"keychain\" in config."
            );
        }
    }

    // 5. Create Claude CLI client
    //    SessionManager and ConversationLockMap are created here and shared
    //    with both ClaudeClient and the always-on cleanup listener.
    let session_manager = Arc::new(
        threshold_cli_wrapper::session::SessionManager::new(
            config.data_dir()?.join("cli-sessions").join("cli-sessions.json"),
        ),
    );
    let conversation_locks = Arc::new(threshold_cli_wrapper::ConversationLockMap::new());
    let process_tracker = Arc::new(threshold_cli_wrapper::ProcessTracker::new());
    let timeout_secs = config.cli.claude.timeout_seconds.unwrap_or(21600);
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
            timeout_secs,
            session_manager.clone(),
            conversation_locks.clone(),
            process_tracker.clone(),
        )
        .await?,
    );
    tracing::info!(
        timeout_secs,
        "Claude CLI client configured."
    );

    // 6. Build tool prompt and create conversation engine
    let tool_prompt = {
        let prompt = threshold_tools::build_tool_prompt(&config);
        if prompt.is_empty() { None } else { Some(prompt) }
    };
    let ack_enabled = config.cli.claude.ack_enabled.unwrap_or(true);
    let status_interval_secs = config
        .cli
        .claude
        .status_interval_seconds
        .unwrap_or(30);
    // HaikuClient is needed for acknowledgments and/or periodic status updates
    let needs_haiku = ack_enabled || status_interval_secs > 0;
    let haiku = if needs_haiku {
        let command = config
            .cli
            .claude
            .command
            .clone()
            .unwrap_or_else(|| "claude".to_string());
        Some(Arc::new(threshold_cli_wrapper::HaikuClient::new(command)))
    } else {
        None
    };
    if ack_enabled {
        tracing::info!("Haiku acknowledgment enabled.");
    }
    if status_interval_secs > 0 {
        tracing::info!(interval_secs = status_interval_secs, "Live status updates enabled.");
    }
    let active_conversations = Arc::new(threshold_core::ActiveConversations::new());
    let engine = Arc::new(
        ConversationEngine::new(
            &config,
            claude.clone(),
            tool_prompt,
            Some(active_conversations.clone()),
            haiku,
            ack_enabled,
            status_interval_secs,
            Some(daemon_state.clone()),
        )
        .await?,
    );
    tracing::info!("Conversation engine initialized.");

    // 6b. Startup migration warnings
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

    // 7. Shared cancellation token for graceful shutdown
    let cancel = CancellationToken::new();

    // 8. Create scheduler up front (if enabled) to get a SchedulerHandle
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
            Some(daemon_state.clone()),
        )
        .await;

        // Validate heartbeat tasks against conversation store (remove orphaned)
        sched.validate_heartbeat_tasks().await;

        (Some(sched), Some(handle))
    } else {
        (None, None)
    };

    // 9. Shared outbound handle — populated by Discord setup, used by
    //    heartbeat and scheduler. Wrapped in Arc<RwLock<Option<...>>> so
    //    it can be set after Discord connects.
    let discord_outbound: Arc<RwLock<Option<Arc<threshold_discord::DiscordOutbound>>>> =
        Arc::new(RwLock::new(None));

    // 10. Build all tasks as futures

    // Discord task
    let discord_handle = {
        let engine = engine.clone();
        let outbound_slot = discord_outbound.clone();
        let cancel = cancel.clone();
        let discord_config_opt = config.discord.clone();
        let scheduler_handle_for_discord = scheduler_cmd_handle.clone();
        let secrets = secrets.clone();
        let daemon_state = daemon_state.clone();
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
                    Some(daemon_state.clone()),
                )
                .await?;

                *outbound_slot.write().await = Some(outbound);
                tracing::info!("Discord bot ready.");
            }

            cancel.cancelled().await;
            Ok::<(), anyhow::Error>(())
        }
    };

    // Daemon API task — always runs (decoupled from scheduler).
    // Handles health checks, drain/undrain, and forwards scheduler commands
    // if the scheduler is enabled.
    let daemon_api_handle = {
        let cancel = cancel.clone();
        let data_dir = data_dir.clone();
        let scheduler_cmd_handle = scheduler_cmd_handle.clone();
        let health_config = health_config.clone();
        let daemon_state = daemon_state.clone();

        async move {
            let socket_path =
                threshold_scheduler::daemon_api::DaemonApi::default_socket_path(&data_dir);
            let daemon_api = threshold_scheduler::daemon_api::DaemonApi::new(
                scheduler_cmd_handle,
                health_config,
                daemon_state,
                socket_path,
            );

            if let Err(e) = daemon_api.run(cancel).await {
                tracing::error!("Daemon API error: {}", e);
            }

            Ok::<(), anyhow::Error>(())
        }
    };

    // Clone scheduler handle for downstream consumers before scheduler_task moves the original
    let scheduler_cmd_handle_for_cleanup = scheduler_cmd_handle.clone();
    let scheduler_cmd_handle_for_web = scheduler_cmd_handle.clone();

    // Scheduler task (only if enabled)
    let scheduler_task = {
        let cancel = cancel.clone();
        let discord_outbound_for_scheduler = discord_outbound.clone();

        async move {
            let (mut scheduler, _handle) = match (scheduler_instance, scheduler_cmd_handle) {
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

            Ok::<(), anyhow::Error>(())
        }
    };

    // 10a. Always-on cleanup listener for conversation deletion.
    //     Runs unconditionally (not gated on scheduler). Handles:
    //     - Scheduler task removal (if scheduler enabled)
    //     - CLI session mapping cleanup
    //     - Per-conversation lock cleanup
    //     - Periodic idle lock sweep
    {
        let mut event_rx = engine.subscribe();
        let cancel_clone = cancel.clone();
        let session_mgr = session_manager.clone();
        let conv_locks = conversation_locks.clone();
        let sched_handle = scheduler_cmd_handle_for_cleanup;
        tokio::spawn(async move {
            let mut sweep_interval =
                tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                tokio::select! {
                    event = event_rx.recv() => {
                        match event {
                            Ok(threshold_conversation::ConversationEvent::ConversationDeleted { conversation_id }) => {
                                // 1. Scheduler cleanup (only if scheduler is enabled)
                                if let Some(handle) = &sched_handle {
                                    if let Err(e) = handle.remove_tasks_for_conversation(conversation_id) {
                                        tracing::warn!(
                                            "Failed to remove scheduler tasks: {}",
                                            e
                                        );
                                    }
                                }
                                // 2. CLI session mapping cleanup
                                if let Err(e) = session_mgr.remove(conversation_id.0).await {
                                    tracing::warn!(
                                        "Failed to remove CLI session: {}",
                                        e
                                    );
                                }
                                // 3. Conversation lock cleanup
                                conv_locks.remove(conversation_id.0).await;
                            }
                            Ok(_) => {} // ignore other events
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("Cleanup listener lagged by {} events", n);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = sweep_interval.tick() => {
                        conv_locks.sweep_idle().await;
                    }
                    _ = cancel_clone.cancelled() => break,
                }
            }
        });
    }

    // 10b. Web interface task
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
        let daemon_state = daemon_state.clone();
        async move {
            if !web_enabled {
                cancel.cancelled().await;
                return Ok::<(), anyhow::Error>(());
            }
            let templates = threshold_web::templates::build_template_env();
            let state = threshold_web::AppState {
                engine,
                scheduler_handle: scheduler_cmd_handle_for_web,
                secret_store: secrets,
                config,
                config_path,
                data_dir,
                cancel: cancel.clone(),
                start_time: chrono::Utc::now(),
                templates,
                daemon_state: Some(daemon_state),
            };
            threshold_web::start_web_server(state).await?;
            Ok(())
        }
    };

    // 10c. Run all tasks concurrently, shut down on signal or error
    tokio::select! {
        r = discord_handle => {
            if let Err(e) = r {
                tracing::error!("Discord error: {}", e);
            }
        }
        r = daemon_api_handle => {
            if let Err(e) = r {
                tracing::error!("Daemon API error: {}", e);
            }
        }
        r = scheduler_task => {
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
            tracing::info!("Shutdown signal received (Ctrl+C).");
        }
        _ = sigterm_signal() => {
            tracing::info!("Shutdown signal received (SIGTERM).");
        }
    }

    // 11. Graceful shutdown
    cancel.cancel();
    engine.save_state().await?;
    remove_pid_file(&pid_path);
    tracing::info!("Threshold shut down cleanly.");

    Ok(())
}

// ---------------------------------------------------------------------------
// PID file management
// ---------------------------------------------------------------------------

/// Write the current process ID to `$DATA_DIR/threshold.pid`.
///
/// Uses `create_new(true)` (O_CREAT|O_EXCL) for atomic creation when possible.
/// Falls back to plain write if the file already exists (stale PID was already
/// removed by `check_existing_daemon`).
fn write_pid_file(data_dir: &Path) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    let pid_path = data_dir.join("threshold.pid");
    let pid_str = std::process::id().to_string();

    // Try atomic exclusive create first — prevents TOCTOU race between
    // check_existing_daemon and write_pid_file when two daemons start
    // simultaneously.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pid_path)
    {
        Ok(mut f) => {
            f.write_all(pid_str.as_bytes())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Stale file — check_existing_daemon already verified it's safe to overwrite
            std::fs::write(&pid_path, &pid_str)?;
        }
        Err(e) => return Err(e.into()),
    }

    tracing::info!(pid = std::process::id(), path = %pid_path.display(), "PID file written");
    Ok(pid_path)
}

/// Remove the PID file on shutdown. Non-fatal if it fails.
fn remove_pid_file(pid_path: &Path) {
    if let Err(e) = std::fs::remove_file(pid_path) {
        tracing::warn!(path = %pid_path.display(), error = %e, "Failed to remove PID file");
    }
}

/// Read a PID from the PID file, returning `None` if missing or unparseable.
fn read_pid_file(data_dir: &Path) -> Option<u32> {
    let pid_path = data_dir.join("threshold.pid");
    std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Check if a process with the given PID is alive (signal 0 = existence check).
fn is_process_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 performs an existence check without sending a signal.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Check if a process is a Threshold daemon by inspecting its command name.
fn is_threshold_process(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
    {
        Ok(output) => {
            let name = String::from_utf8_lossy(&output.stdout);
            name.trim().ends_with("threshold")
        }
        Err(_) => false,
    }
}

/// Verify no other Threshold daemon is running. Stale PID files are removed
/// so that `write_pid_file` can use atomic exclusive create. Live Threshold
/// processes cause a `DaemonAlreadyRunning` error.
fn check_existing_daemon(data_dir: &Path) -> anyhow::Result<()> {
    use threshold_core::ThresholdError;

    if let Some(pid) = read_pid_file(data_dir) {
        if is_process_alive(pid) {
            if is_threshold_process(pid) {
                return Err(ThresholdError::DaemonAlreadyRunning { pid }.into());
            }
            tracing::warn!(pid, "PID file exists for non-Threshold process, removing stale file");
        } else {
            tracing::info!(pid, "Stale PID file found, removing");
        }
        // Remove stale file so write_pid_file can use atomic O_CREAT|O_EXCL
        let pid_path = data_dir.join("threshold.pid");
        let _ = std::fs::remove_file(&pid_path);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Wait for a SIGTERM signal (Unix only).
async fn sigterm_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
    sigterm.recv().await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn pid_file_write_and_read() {
        let tmp = TempDir::new().unwrap();
        let pid_path = write_pid_file(tmp.path()).unwrap();
        assert!(pid_path.exists());

        let read_pid = read_pid_file(tmp.path());
        assert_eq!(read_pid, Some(std::process::id()));
    }

    #[test]
    fn pid_file_remove() {
        let tmp = TempDir::new().unwrap();
        let pid_path = write_pid_file(tmp.path()).unwrap();
        assert!(pid_path.exists());

        remove_pid_file(&pid_path);
        assert!(!pid_path.exists());
    }

    #[test]
    fn pid_file_remove_nonexistent_is_ok() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("threshold.pid");
        // Should not panic
        remove_pid_file(&path);
    }

    #[test]
    fn read_pid_file_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read_pid_file(tmp.path()), None);
    }

    #[test]
    fn read_pid_file_returns_none_for_garbage() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("threshold.pid"), "not-a-number").unwrap();
        assert_eq!(read_pid_file(tmp.path()), None);
    }

    #[test]
    fn stale_pid_file_allows_startup() {
        let tmp = TempDir::new().unwrap();
        // Write a PID that doesn't exist (99999999 is almost certainly unused)
        fs::write(tmp.path().join("threshold.pid"), "99999999").unwrap();
        // Should succeed — stale PID
        assert!(check_existing_daemon(tmp.path()).is_ok());
    }

    #[test]
    fn no_pid_file_allows_startup() {
        let tmp = TempDir::new().unwrap();
        assert!(check_existing_daemon(tmp.path()).is_ok());
    }

    #[test]
    fn is_process_alive_for_current_process() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn is_process_alive_for_nonexistent() {
        // PID 99999999 should not exist
        assert!(!is_process_alive(99999999));
    }

    #[test]
    fn is_threshold_process_for_nonexistent() {
        assert!(!is_threshold_process(99999999));
    }
}
