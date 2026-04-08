//! Threshold system tray application.
//!
//! Displays a status icon in the menu bar (macOS) or notification area (Windows)
//! that reflects daemon health and exposes common actions.
//!
//! Communicates with the daemon via the existing Unix socket API.
//! Runs as a separate binary from the daemon itself.

// Suppress console window on Windows release builds.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod icons;
mod menu;
mod poller;

use std::path::PathBuf;

use clap::Parser;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::TrayIconBuilder;

use icons::TrayState;

/// Threshold system tray — daemon status and quick actions.
#[derive(Parser)]
#[command(name = "threshold-tray", version, about)]
struct Args {
    /// Path to the data directory (default: ~/.threshold)
    #[arg(long)]
    data_dir: Option<String>,

    /// Path to the config file
    #[arg(long)]
    config: Option<String>,
}

/// Custom event type for the tray event loop.
#[derive(Debug)]
enum UserEvent {
    /// Daemon state changed.
    StateChanged(TrayState),
    /// A menu item was clicked — wake the event loop to process it.
    MenuClicked,
}

fn resolve_data_dir(override_path: Option<&str>) -> PathBuf {
    if let Some(p) = override_path {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("THRESHOLD_DATA_DIR") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".threshold")
}

fn resolve_web_port(config_path: Option<&str>) -> u16 {
    // Try to read the port from config file
    let path = config_path
        .map(PathBuf::from)
        .or_else(|| std::env::var("THRESHOLD_CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".threshold")
                .join("config.toml")
        });

    if let Ok(content) = std::fs::read_to_string(&path) {
        // Simple TOML parsing for port — avoid pulling in a full TOML crate
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("port") && trimmed.contains('=') {
                if let Some(val) = trimmed.split('=').nth(1) {
                    if let Ok(port) = val.trim().parse::<u16>() {
                        return port;
                    }
                }
            }
        }
    }

    3000 // default
}

fn main() {
    let args = Args::parse();

    // Set up file-based logging (no stderr on Windows release builds)
    let data_dir = resolve_data_dir(args.data_dir.as_deref());
    let log_dir = data_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let file_appender = tracing_appender::rolling::daily(&log_dir, "tray.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Threshold tray starting");

    let web_port = resolve_web_port(args.config.as_deref());
    let dashboard_url = format!("http://127.0.0.1:{}", web_port);

    // Build daemon CLI args for start/stop/restart commands
    let mut daemon_args: Vec<String> = Vec::new();
    if let Some(ref dir) = args.data_dir {
        daemon_args.extend(["--data-dir".to_string(), dir.clone()]);
    }
    if let Some(ref cfg) = args.config {
        daemon_args.extend(["--config".to_string(), cfg.clone()]);
    }

    // Start the daemon health poller (runs on a background thread with its own tokio runtime)
    let state_rx = poller::start_poller(data_dir.clone());

    // Build the event loop
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    let proxy = event_loop.create_proxy();

    // Watch for state changes and forward to event loop
    let mut watch_rx = state_rx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            loop {
                if watch_rx.changed().await.is_err() {
                    break;
                }
                let state = *watch_rx.borrow();
                let _ = proxy.send_event(UserEvent::StateChanged(state));
            }
        });
    });

    // Track current state for the event loop
    let mut current_state = TrayState::Stopped;
    let mut tray_icon: Option<tray_icon::TrayIcon> = None;
    let mut menu_items: Option<menu::MenuItems> = None;

    // Find the threshold binary for daemon commands
    let threshold_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("threshold")))
        .unwrap_or_else(|| PathBuf::from("threshold"));

    let dashboard_url_clone = dashboard_url.clone();
    let daemon_args_clone = daemon_args.clone();

    // Set up menu event handler
    let proxy2 = event_loop.create_proxy();
    muda::MenuEvent::set_event_handler(Some(move |_event: muda::MenuEvent| {
        // Wake the event loop to process the menu click
        let _ = proxy2.send_event(UserEvent::MenuClicked);
    }));

    event_loop
        .run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;

            match event {
                Event::NewEvents(StartCause::Init) => {
                    // Build tray icon on init (required for macOS main thread)
                    let items = menu::MenuItems::new();
                    items.update_for_state(current_state);

                    let icon = icons::icon_for_state(current_state);

                    let tray = TrayIconBuilder::new()
                        .with_icon(icon)
                        .with_tooltip(current_state.tooltip())
                        .with_menu(Box::new(items.menu.clone()))
                        .build()
                        .expect("Failed to create tray icon");

                    tray_icon = Some(tray);
                    menu_items = Some(items);

                    tracing::info!("Tray icon created");

                    // On macOS, tao's event loop handles the main thread
                    // run loop automatically — no manual CFRunLoopWakeUp needed.
                }

                Event::UserEvent(UserEvent::MenuClicked) => {
                    // Process menu click events
                    if let Ok(event) = muda::MenuEvent::receiver().try_recv() {
                        if let Some(ref items) = menu_items {
                            let id = event.id();
                            if id == items.open_dashboard.id() {
                                let _ = open::that(&dashboard_url_clone);
                            } else if id == items.start.id() {
                                spawn_daemon_command(
                                    &threshold_exe,
                                    &["daemon", "start"],
                                    &daemon_args_clone,
                                );
                            } else if id == items.restart.id() {
                                spawn_daemon_command(
                                    &threshold_exe,
                                    &["daemon", "restart"],
                                    &daemon_args_clone,
                                );
                            } else if id == items.stop.id() {
                                spawn_daemon_command(
                                    &threshold_exe,
                                    &["daemon", "stop"],
                                    &daemon_args_clone,
                                );
                            } else if id == items.quit.id() {
                                tracing::info!("Quit requested");
                                tray_icon.take(); // Drop tray icon before exit
                                *control_flow = ControlFlow::Exit;
                                return;
                            }
                        }
                    }
                }

                Event::UserEvent(UserEvent::StateChanged(new_state)) => {
                    // Update icon and menu if state changed
                    if new_state != current_state {
                        current_state = new_state;
                        tracing::info!(?current_state, "Daemon state changed");

                        if let Some(ref tray) = tray_icon {
                            let icon = icons::icon_for_state(current_state);
                            let _ = tray.set_icon(Some(icon));
                            let _ = tray.set_tooltip(Some(current_state.tooltip()));
                        }
                        if let Some(ref items) = menu_items {
                            items.update_for_state(current_state);
                        }
                    }
                }

                _ => {}
            }
        })
        ;
}

/// Spawn a daemon CLI command as a detached process.
fn spawn_daemon_command(exe: &std::path::Path, args: &[&str], extra_args: &[String]) {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    match cmd.spawn() {
        Ok(_) => tracing::info!(?args, "Spawned daemon command"),
        Err(e) => tracing::error!(?args, %e, "Failed to spawn daemon command"),
    }
}
