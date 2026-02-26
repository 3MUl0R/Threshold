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

    /// Daemon subcommand (default: start)
    #[command(subcommand)]
    action: Option<DaemonAction>,
}

/// Subcommands for `threshold daemon`.
///
/// If no subcommand is given, `Start` is the default (backward compat).
#[derive(clap::Subcommand)]
enum DaemonAction {
    /// Start the daemon (default when no subcommand given)
    Start,
    /// Show daemon status (PID, uptime, active work, scheduler info)
    Status {
        /// Override data directory for locating the daemon socket
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// Gracefully stop the running daemon
    Stop {
        /// Override data directory for locating the daemon socket/PID
        #[arg(long)]
        data_dir: Option<String>,
        /// Maximum seconds to wait for active work to drain (default: 30)
        #[arg(long, default_value = "30")]
        drain_timeout: u64,
    },
    /// Rebuild and restart the daemon
    Restart {
        /// Override data directory for locating the daemon socket/PID
        #[arg(long)]
        data_dir: Option<String>,
        /// Maximum seconds to wait for active work to drain (default: 120)
        #[arg(long, default_value = "120")]
        drain_timeout: u64,
        /// Skip `cargo build` before restart
        #[arg(long)]
        skip_build: bool,
        /// Conversation ID for a follow-on hook after restart
        #[arg(long)]
        follow_on_conversation: Option<String>,
        /// Prompt to inject into the follow-on conversation
        #[arg(long)]
        follow_on_prompt: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon(args) => {
            let action = args.action.unwrap_or(DaemonAction::Start);
            match action {
                DaemonAction::Start => run_daemon(args.config).await,
                DaemonAction::Status { data_dir } => run_daemon_status(data_dir).await,
                DaemonAction::Stop {
                    data_dir,
                    drain_timeout,
                } => run_daemon_stop(data_dir, drain_timeout).await,
                DaemonAction::Restart {
                    data_dir,
                    drain_timeout,
                    skip_build,
                    follow_on_conversation,
                    follow_on_prompt,
                } => {
                    run_daemon_restart(
                        data_dir,
                        drain_timeout,
                        skip_build,
                        follow_on_conversation,
                        follow_on_prompt,
                    )
                    .await
                }
            }
        }
        Commands::Schedule { command } => schedule::handle_schedule_command(command).await,
        Commands::Gmail(args) => gmail::handle_gmail_command(args).await,
        Commands::Imagegen(args) => imagegen::handle_imagegen_command(args).await,
    }
}

/// Run the Threshold daemon.
///
/// This contains the full daemon lifecycle extracted from the original main():
/// config loading, logging, Discord bot, heartbeat, scheduler, and graceful shutdown.
async fn run_daemon(config_arg: Option<String>) -> anyhow::Result<()> {
    use std::sync::Arc;
    use threshold_cli_wrapper::ClaudeClient;
    use threshold_conversation::ConversationEngine;
    use threshold_core::config::ThresholdConfig;
    use threshold_core::{init_logging, DaemonState, HealthConfig, SecretStore, ThresholdError};
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    // 1. Load config (from explicit path or default)
    let config_path = match &config_arg {
        Some(path) => std::path::PathBuf::from(path),
        None => std::env::var("THRESHOLD_CONFIG")
            .map(std::path::PathBuf::from)
            .unwrap_or(ThresholdConfig::default_config_path()?),
    };
    let config = match &config_arg {
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

    // 6a. Process restart hooks (follow-on prompts from previous restart)
    process_restart_hooks(&data_dir, &engine).await;

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
// Daemon management commands (status, stop, restart)
// ---------------------------------------------------------------------------

/// Show the status of the running daemon.
async fn run_daemon_status(data_dir_override: Option<String>) -> anyhow::Result<()> {
    let data_dir = daemon_client::resolve_data_dir(data_dir_override.as_deref())?;

    // Check PID file first
    let pid = read_pid_file(&data_dir);
    match pid {
        None => {
            println!("Threshold daemon: not running (no PID file)");
            return Ok(());
        }
        Some(pid) if !is_process_alive(pid) => {
            println!("Threshold daemon: not running (stale PID file, PID {})", pid);
            return Ok(());
        }
        _ => {}
    }

    // Try to connect and get health
    let client = daemon_client::DaemonClient::with_data_dir(&data_dir);
    match client.send_health_check().await {
        Ok(resp) => {
            if let Some(data) = &resp.data {
                let draining = data.get("draining").and_then(|v| v.as_bool()).unwrap_or(false);
                let status_str = if draining { "Draining" } else { "Running" };
                let pid = data.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
                let uptime = data.get("uptime_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                let version = data
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let active_work = data.get("active_work").and_then(|v| v.as_u64()).unwrap_or(0);
                let scheduler_task_count = data.get("scheduler_task_count");
                let scheduler_enabled_count = data.get("scheduler_enabled_count");

                println!("Threshold daemon: {}", status_str);
                println!("  PID:          {}", pid);
                println!("  Version:      {}", version);
                println!("  Uptime:       {}", format_uptime(uptime));
                println!("  Active work:  {} run(s)", active_work);

                match scheduler_task_count {
                    Some(serde_json::Value::Number(n)) => {
                        let total = n.as_u64().unwrap_or(0);
                        let enabled = scheduler_enabled_count
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        println!("  Scheduler:    {} task(s) ({} enabled)", total, enabled);
                    }
                    _ => {
                        println!("  Scheduler:    disabled");
                    }
                }
            } else {
                println!("Threshold daemon: running (PID {})", pid.unwrap_or(0));
            }
        }
        Err(e) => {
            let pid = pid.unwrap_or(0);
            println!(
                "Threshold daemon: running (PID {}) but socket unreachable: {}",
                pid, e
            );
        }
    }

    Ok(())
}

/// Format seconds into a human-readable uptime string.
fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if days > 0 {
        format!("{}d {}h {}m {}s", days, hours, minutes, seconds)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

/// Gracefully stop the running daemon.
///
/// 1. Send Drain command (reject new work)
/// 2. Poll Health until active_work == 0 or drain_timeout expires
/// 3. Send SIGTERM
/// 4. Wait for process exit
async fn run_daemon_stop(
    data_dir_override: Option<String>,
    drain_timeout: u64,
) -> anyhow::Result<()> {
    let data_dir = daemon_client::resolve_data_dir(data_dir_override.as_deref())?;

    // Check PID file
    let pid = read_pid_file(&data_dir)
        .ok_or_else(|| anyhow::anyhow!("Daemon is not running (no PID file)"))?;

    if !is_process_alive(pid) {
        // Clean up stale PID file
        let _ = std::fs::remove_file(data_dir.join("threshold.pid"));
        anyhow::bail!("Daemon is not running (stale PID file for PID {})", pid);
    }

    let client = daemon_client::DaemonClient::with_data_dir(&data_dir);

    // Try drain phase (skip if socket is unreachable)
    let drain_summary = match drain_and_wait(&client, drain_timeout).await {
        Ok(summary) => Some(summary),
        Err(e) => {
            eprintln!(
                "Warning: Could not drain daemon ({}). Proceeding with SIGTERM.",
                e
            );
            None
        }
    };

    // Send SIGTERM
    send_sigterm(pid)?;
    println!("SIGTERM sent to PID {}.", pid);

    // Wait for process exit (up to 10s)
    wait_for_process_exit(pid, 10).await;

    // Print summary
    if let Some(summary) = drain_summary {
        println!(
            "Drain complete: {} finished, {} aborted.",
            summary.finished, summary.aborted
        );
    }

    if !is_process_alive(pid) {
        println!("Daemon stopped.");
    } else {
        println!("Warning: daemon process {} may still be shutting down.", pid);
    }

    Ok(())
}

/// Rebuild and restart the daemon.
///
/// 1. Build first (fail-fast — never stop a running daemon without a good binary)
/// 2. Send Drain command
/// 3. Poll Health until drained or timeout
/// 4. Write restart hooks (if follow-on specified)
/// 5. Send SIGTERM
/// 6. Wait for exit
/// 7. Start new daemon
/// 8. Wait for healthy
async fn run_daemon_restart(
    data_dir_override: Option<String>,
    drain_timeout: u64,
    skip_build: bool,
    follow_on_conversation: Option<String>,
    follow_on_prompt: Option<String>,
) -> anyhow::Result<()> {
    use threshold_core::{ConversationId, DrainSummary, RestartHook};

    let data_dir = daemon_client::resolve_data_dir(data_dir_override.as_deref())?;

    // Check PID file
    let pid = read_pid_file(&data_dir)
        .ok_or_else(|| anyhow::anyhow!("Daemon is not running (no PID file)"))?;

    if !is_process_alive(pid) {
        let _ = std::fs::remove_file(data_dir.join("threshold.pid"));
        anyhow::bail!("Daemon is not running (stale PID file for PID {})", pid);
    }

    // Step 1: Build first (critical safety invariant)
    if !skip_build {
        println!("Building...");
        let repo_root = find_repo_root()?;
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", "threshold-server"])
            .current_dir(&repo_root)
            .status()?;

        if !status.success() {
            anyhow::bail!(
                "Build failed (exit {}). Restart aborted — daemon is still running.",
                status.code().unwrap_or(-1)
            );
        }
        println!("Build succeeded.");
    }

    let client = daemon_client::DaemonClient::with_data_dir(&data_dir);

    // Step 2-3: Drain
    let drain_summary = match drain_and_wait(&client, drain_timeout).await {
        Ok(summary) => Some(summary),
        Err(e) => {
            eprintln!(
                "Warning: Could not drain daemon ({}). Proceeding with SIGTERM.",
                e
            );
            None
        }
    };

    // Step 4: Write restart hooks (if follow-on specified)
    let hooks_path = data_dir.join("state").join("restart-hooks.json");
    if let (Some(conv_id_str), Some(prompt)) = (&follow_on_conversation, &follow_on_prompt) {
        let conv_id: uuid::Uuid = conv_id_str.parse().map_err(|e| {
            anyhow::anyhow!("Invalid conversation ID '{}': {}", conv_id_str, e)
        })?;

        // Prepend drain summary to the prompt
        let full_prompt = if let Some(summary) = &drain_summary {
            format!(
                "[Restart drain summary: {} task(s) finished, {} aborted]\n\n{}",
                summary.finished, summary.aborted, prompt
            )
        } else {
            prompt.clone()
        };

        let hook = RestartHook {
            conversation_id: ConversationId(conv_id),
            prompt: full_prompt,
            created_at: chrono::Utc::now(),
            requested_by: Some("cli".into()),
            drain_summary: drain_summary.as_ref().map(|s| DrainSummary {
                finished: s.finished,
                aborted: s.aborted,
            }),
        };

        if let Err(e) = write_hooks_atomic(&hooks_path, &[hook]) {
            // Rollback: undrain and clean up
            eprintln!("Error writing restart hooks: {}. Rolling back.", e);
            rollback_on_failure(&client, &hooks_path, &data_dir).await;
            anyhow::bail!("Restart aborted: failed to write hooks");
        }
    }

    // Step 5: Write restart-pending for supervised mode
    let supervised = detect_supervised(&data_dir);
    let restart_pending_path = data_dir.join("state").join("restart-pending.json");
    if supervised {
        let pending = serde_json::json!({
            "skip_build": skip_build,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });
        if let Err(e) = write_json_atomic(&restart_pending_path, &pending) {
            eprintln!("Error writing restart-pending: {}. Rolling back.", e);
            rollback_on_failure(&client, &hooks_path, &data_dir).await;
            let _ = std::fs::remove_file(&restart_pending_path);
            anyhow::bail!("Restart aborted: failed to write restart-pending");
        }
    }

    // Step 6: Send SIGTERM
    if let Err(e) = send_sigterm(pid) {
        eprintln!("Error sending SIGTERM: {}. Rolling back.", e);
        rollback_on_failure(&client, &hooks_path, &data_dir).await;
        if supervised {
            let _ = std::fs::remove_file(&restart_pending_path);
        }
        anyhow::bail!("Restart aborted: failed to send SIGTERM");
    }
    println!("SIGTERM sent to PID {}.", pid);

    // Step 7: Wait for exit
    wait_for_process_exit(pid, 10).await;

    if let Some(summary) = &drain_summary {
        println!(
            "Drain complete: {} finished, {} aborted.",
            summary.finished, summary.aborted
        );
    }

    if supervised {
        // In supervised mode, the wrapper handles restart
        println!("Supervised mode: wrapper will restart the daemon.");
        return Ok(());
    }

    // Step 8: Standalone restart — start new daemon
    println!("Starting new daemon...");
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("start");
    if let Ok(config_path) = std::env::var("THRESHOLD_CONFIG") {
        cmd.args(["--config", &config_path]);
    }
    // Spawn detached
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("Failed to start new daemon: {}", e))?;

    // Step 9: Wait for healthy
    println!("Waiting for daemon to become healthy...");
    match wait_for_healthy(&client, 30).await {
        Ok(()) => println!("Daemon restarted successfully."),
        Err(e) => eprintln!("Warning: health check failed after restart: {}", e),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Restart/stop helper functions
// ---------------------------------------------------------------------------

/// A snapshot of drain progress.
struct DrainProgress {
    finished: u32,
    aborted: u32,
}

/// Send Drain and poll Health until `active_work == 0` or timeout expires.
async fn drain_and_wait(
    client: &daemon_client::DaemonClient,
    timeout_secs: u64,
) -> anyhow::Result<DrainProgress> {
    // Send drain command
    let drain_resp = client.send_drain().await?;
    let initial_work = drain_resp
        .data
        .as_ref()
        .and_then(|d| d.get("active_work"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    if initial_work == 0 {
        return Ok(DrainProgress {
            finished: 0,
            aborted: 0,
        });
    }

    println!(
        "Draining... ({} active run(s), timeout {}s)",
        initial_work, timeout_secs
    );

    let deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(timeout_secs);

    let mut last_work = initial_work;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        if tokio::time::Instant::now() >= deadline {
            let aborted = last_work;
            let finished = initial_work.saturating_sub(aborted);
            eprintln!(
                "Drain timeout expired with {} active run(s) remaining.",
                aborted
            );
            return Ok(DrainProgress { finished, aborted });
        }

        match client.send_health_check().await {
            Ok(resp) => {
                let work = resp
                    .data
                    .as_ref()
                    .and_then(|d| d.get("active_work"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;

                if work == 0 {
                    return Ok(DrainProgress {
                        finished: initial_work,
                        aborted: 0,
                    });
                }

                if work != last_work {
                    println!("  {} active run(s) remaining...", work);
                    last_work = work;
                }
            }
            Err(_) => {
                // Socket may have gone away — treat as drained
                return Ok(DrainProgress {
                    finished: initial_work,
                    aborted: 0,
                });
            }
        }
    }
}

/// Send SIGTERM to a process.
fn send_sigterm(pid: u32) -> anyhow::Result<()> {
    // SAFETY: sending SIGTERM to a known PID.
    let ret = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("Failed to send SIGTERM to PID {}: {}", pid, err);
    }
    Ok(())
}

/// Wait for a process to exit, polling every 500ms up to `max_secs`.
async fn wait_for_process_exit(pid: u32, max_secs: u64) {
    let deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(max_secs);

    while tokio::time::Instant::now() < deadline {
        if !is_process_alive(pid) {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
}

/// Wait for the daemon to become healthy, polling every second.
async fn wait_for_healthy(
    client: &daemon_client::DaemonClient,
    max_secs: u64,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(max_secs);

    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        match client.send_health_check().await {
            Ok(resp) if resp.status == daemon_client::ResponseStatus::Ok => {
                return Ok(());
            }
            _ => continue,
        }
    }

    anyhow::bail!("Daemon did not become healthy within {}s", max_secs)
}

/// Find the repository root by walking up from the current exe to find Cargo.toml.
fn find_repo_root() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut dir = exe.parent().map(|p| p.to_path_buf());

    // Walk up from the executable location
    while let Some(d) = dir {
        if d.join("Cargo.toml").exists() && d.join("crates").is_dir() {
            return Ok(d);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    // Fallback: try current working directory
    let cwd = std::env::current_dir()?;
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join("Cargo.toml").exists() && d.join("crates").is_dir() {
            return Ok(d);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    anyhow::bail!(
        "Cannot find repository root (no Cargo.toml with crates/ directory found). \
         Use --skip-build or run from within the repository."
    )
}

/// Write restart hooks to disk atomically (write-then-rename).
fn write_hooks_atomic(
    path: &Path,
    hooks: &[threshold_core::RestartHook],
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(hooks)?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Write JSON data to disk atomically (write-then-rename).
fn write_json_atomic(path: &Path, data: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(data)?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Detect if the daemon is running under the supervised wrapper script.
///
/// Checks for a `$DATA_DIR/state/supervised` marker file containing the current
/// daemon's PID and a start timestamp that hasn't gone stale.
fn detect_supervised(data_dir: &Path) -> bool {
    let marker_path = data_dir.join("state").join("supervised");
    let content = match std::fs::read_to_string(&marker_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Parse "PID START_TIME" format
    let parts: Vec<&str> = content.trim().split_whitespace().collect();
    if parts.len() < 2 {
        let _ = std::fs::remove_file(&marker_path);
        return false;
    }

    let marker_pid: u32 = match parts[0].parse() {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_file(&marker_path);
            return false;
        }
    };

    // Check if the marker PID matches the running daemon
    if !is_process_alive(marker_pid) {
        let _ = std::fs::remove_file(&marker_path);
        return false;
    }

    // Check if the PID is actually a threshold process (handles PID reuse)
    if !is_threshold_process(marker_pid) {
        let _ = std::fs::remove_file(&marker_path);
        return false;
    }

    true
}

/// Roll back a failed restart: send Undrain and clean up control files.
async fn rollback_on_failure(
    client: &daemon_client::DaemonClient,
    hooks_path: &Path,
    data_dir: &Path,
) {
    // Try to undrain
    if let Err(e) = client.send_undrain().await {
        eprintln!("Warning: failed to undrain daemon: {}", e);
    }

    // Clean up hooks file
    let _ = std::fs::remove_file(hooks_path);

    // Clean up restart-pending
    let _ = std::fs::remove_file(data_dir.join("state").join("restart-pending.json"));
}

/// Process follow-on restart hooks on daemon startup.
///
/// Reads `$DATA_DIR/state/restart-hooks.json`, sends each hook's prompt to its
/// conversation via `send_to_conversation()`, and removes the hooks file.
/// Failed hooks are preserved in a rewritten hooks file.
async fn process_restart_hooks(
    data_dir: &Path,
    engine: &std::sync::Arc<threshold_conversation::ConversationEngine>,
) {
    let hooks_path = data_dir.join("state").join("restart-hooks.json");
    let content = match std::fs::read_to_string(&hooks_path) {
        Ok(c) => c,
        Err(_) => return, // No hooks file — nothing to do
    };

    let hooks: Vec<threshold_core::RestartHook> = match serde_json::from_str(&content) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("Failed to parse restart-hooks.json: {}", e);
            return;
        }
    };

    if hooks.is_empty() {
        let _ = std::fs::remove_file(&hooks_path);
        return;
    }

    tracing::info!("Processing {} restart hook(s)...", hooks.len());

    let mut failed_hooks = Vec::new();

    for hook in &hooks {
        tracing::info!(
            conversation_id = %hook.conversation_id.0,
            "Delivering restart hook"
        );

        match engine
            .send_to_conversation(&hook.conversation_id, &hook.prompt)
            .await
        {
            Ok(_) => {
                tracing::info!(
                    conversation_id = %hook.conversation_id.0,
                    "Restart hook delivered successfully"
                );
            }
            Err(e) => {
                tracing::warn!(
                    conversation_id = %hook.conversation_id.0,
                    error = %e,
                    "Failed to deliver restart hook"
                );
                failed_hooks.push(hook.clone());
            }
        }
    }

    if failed_hooks.is_empty() {
        let _ = std::fs::remove_file(&hooks_path);
    } else {
        tracing::warn!(
            "{} restart hook(s) failed, preserving in hooks file",
            failed_hooks.len()
        );
        if let Err(e) = write_hooks_atomic(&hooks_path, &failed_hooks) {
            tracing::error!("Failed to rewrite hooks file: {}", e);
        }
    }
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

    // ---- Phase 16C tests ----

    #[test]
    fn format_uptime_seconds() {
        assert_eq!(format_uptime(42), "42s");
    }

    #[test]
    fn format_uptime_minutes() {
        assert_eq!(format_uptime(125), "2m 5s");
    }

    #[test]
    fn format_uptime_hours() {
        assert_eq!(format_uptime(3661), "1h 1m 1s");
    }

    #[test]
    fn format_uptime_days() {
        assert_eq!(format_uptime(90061), "1d 1h 1m 1s");
    }

    #[test]
    fn write_hooks_atomic_round_trip() {
        use threshold_core::{ConversationId, RestartHook};

        let tmp = TempDir::new().unwrap();
        let hooks_path = tmp.path().join("state").join("restart-hooks.json");

        let hooks = vec![RestartHook {
            conversation_id: ConversationId::new(),
            prompt: "Test prompt".into(),
            created_at: chrono::Utc::now(),
            requested_by: Some("test".into()),
            drain_summary: None,
        }];

        write_hooks_atomic(&hooks_path, &hooks).unwrap();

        let content = fs::read_to_string(&hooks_path).unwrap();
        let restored: Vec<RestartHook> = serde_json::from_str(&content).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].prompt, "Test prompt");
        assert_eq!(restored[0].conversation_id, hooks[0].conversation_id);
    }

    #[test]
    fn write_hooks_atomic_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let hooks_path = tmp
            .path()
            .join("deeply")
            .join("nested")
            .join("restart-hooks.json");

        let hooks = vec![];
        write_hooks_atomic(&hooks_path, &hooks).unwrap();
        assert!(hooks_path.exists());
    }

    #[test]
    fn write_json_atomic_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state").join("test.json");

        let data = serde_json::json!({"key": "value", "count": 42});
        write_json_atomic(&path, &data).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let restored: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(restored["key"], "value");
        assert_eq!(restored["count"], 42);
    }

    #[test]
    fn detect_supervised_returns_false_without_marker() {
        let tmp = TempDir::new().unwrap();
        assert!(!detect_supervised(tmp.path()));
    }

    #[test]
    fn detect_supervised_returns_false_for_dead_pid() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // Write marker with non-existent PID
        fs::write(
            state_dir.join("supervised"),
            "99999999 2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert!(!detect_supervised(tmp.path()));
        // Marker should be cleaned up
        assert!(!state_dir.join("supervised").exists());
    }

    #[test]
    fn detect_supervised_returns_false_for_invalid_marker() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("supervised"), "garbage").unwrap();
        assert!(!detect_supervised(tmp.path()));
    }

    #[test]
    fn send_sigterm_to_nonexistent_fails() {
        assert!(send_sigterm(99999999).is_err());
    }
}
