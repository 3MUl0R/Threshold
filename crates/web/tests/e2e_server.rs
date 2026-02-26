//! E2E test helper: starts a web server and prints the URL.
//! Used by the Playwright E2E test script.

use std::net::SocketAddr;
use std::sync::Arc;

use threshold_core::config::{ClaudeCliConfig, CliConfig, ThresholdConfig, WebConfig};
use threshold_web::{routes, state::AppState, templates};

#[tokio::main]
async fn main() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    // Create some sample data for richer testing
    std::fs::create_dir_all(data_dir.join("audit")).unwrap();
    std::fs::create_dir_all(data_dir.join("logs")).unwrap();

    let config = Arc::new(ThresholdConfig {
        data_dir: Some(data_dir.clone()),
        log_level: None,
        secret_backend: None,
        cli: CliConfig {
            claude: ClaudeCliConfig {
                command: Some("echo".into()),
                model: None,
                timeout_seconds: None,
                skip_permissions: None,
                ack_enabled: None,
                status_interval_seconds: None,
                extra_flags: vec![],
            },
        },
        discord: None,
        agents: vec![threshold_core::config::AgentConfigToml {
            id: "default".into(),
            name: "Default".into(),
            cli_provider: "claude".into(),
            model: None,
            system_prompt: None,
            system_prompt_file: None,
            tools: None,
        }],
        tools: Default::default(),
        heartbeat: None,
        scheduler: None,
        web: Some(WebConfig {
            enabled: true,
            bind: Some("127.0.0.1".into()),
            port: Some(0),
        }),
    });

    let sessions = Arc::new(threshold_cli_wrapper::session::SessionManager::new(
        data_dir.join("cli-sessions").join("cli-sessions.json"),
    ));
    let locks = Arc::new(threshold_cli_wrapper::ConversationLockMap::new());
    let tracker = Arc::new(threshold_cli_wrapper::ProcessTracker::new());
    let claude = Arc::new(
        threshold_cli_wrapper::ClaudeClient::new(
            "echo".into(),
            data_dir.join("cli-sessions"),
            false,
            300,
            sessions,
            locks,
            tracker,
        )
        .await
        .unwrap(),
    );

    let engine = Arc::new(
        threshold_conversation::ConversationEngine::new(
            &config, claude, None, None, None, false, 0, None,
        )
        .await
        .unwrap(),
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    let tpl = templates::build_template_env();

    // Write a sample config file
    let config_path = data_dir.join("config.toml");
    std::fs::write(&config_path, "[cli.claude]\ncommand = \"echo\"\n").unwrap();

    let state = AppState {
        engine,
        scheduler_handle: None,
        secret_store: Arc::new(
            threshold_core::SecretStore::with_file_backend(data_dir.join("secrets.toml")).unwrap(),
        ),
        config,
        config_path,
        data_dir,
        cancel: cancel.clone(),
        start_time: chrono::Utc::now(),
        templates: tpl,
        daemon_state: None,
    };

    let app = routes::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();

    // Print the URL for the test script to pick up
    println!("E2E_SERVER_URL=http://{addr}");

    // Flush stdout
    use std::io::Write;
    std::io::stdout().flush().unwrap();

    let cancel_clone = cancel.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(cancel_clone.cancelled_owned())
            .await
            .unwrap();
    });

    // Wait for SIGTERM or Ctrl+C
    tokio::signal::ctrl_c().await.ok();
    cancel.cancel();
    server.await.ok();
}
