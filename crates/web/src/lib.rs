//! Threshold Web Interface — localhost-only management dashboard.
//!
//! Provides a web UI for monitoring and managing the Threshold system.
//! Built with Axum + minijinja + htmx, running as a concurrent task
//! inside `threshold daemon`.

pub mod csrf;
pub mod error;
pub mod flash;
pub mod helpers;
pub mod routes;
pub mod state;
pub mod templates;

pub use state::AppState;

use std::net::SocketAddr;

use threshold_core::config::is_loopback_address;

/// Start the web server. Returns a future that runs until cancellation.
///
/// Binds to the configured address (default: 127.0.0.1:3000).
/// Hard-blocks non-loopback addresses since no authentication exists.
pub async fn start_web_server(state: AppState) -> anyhow::Result<()> {
    let bind = state
        .config
        .web
        .as_ref()
        .and_then(|w| w.bind.as_deref())
        .unwrap_or("127.0.0.1");
    let port = state
        .config
        .web
        .as_ref()
        .and_then(|w| w.port)
        .unwrap_or(3000);

    // Hard-block non-loopback: no auth exists, so remote access = full admin
    if !is_loopback_address(bind) {
        anyhow::bail!(
            "Web bind address '{bind}' is not loopback. \
             The web interface has no authentication — refusing to start. \
             Use 127.0.0.1 or ::1."
        );
    }

    // Resolve bind address: localhost → 127.0.0.1, ::1 needs bracket notation
    let addr: SocketAddr = if bind == "localhost" {
        format!("127.0.0.1:{port}").parse()?
    } else if bind.contains(':') {
        // IPv6: needs [addr]:port notation
        format!("[{bind}]:{port}").parse()?
    } else {
        format!("{bind}:{port}").parse()?
    };
    tracing::info!("Web interface listening on http://{addr}");

    let app = routes::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(state.cancel.cancelled_owned())
        .await?;

    tracing::info!("Web interface shut down.");
    Ok(())
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::sync::Arc;

    /// Build a minimal AppState suitable for integration tests.
    /// Starts the web server on port 0 (OS-assigned) and returns the bound address.
    async fn start_test_server() -> (SocketAddr, tokio_util::sync::CancellationToken) {
        use threshold_core::config::{
            ClaudeCliConfig, CliConfig, ThresholdConfig, WebConfig,
        };

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();

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
                port: Some(0), // OS assigns port
            }),
        });

        let claude = Arc::new(
            threshold_cli_wrapper::ClaudeClient::new(
                "echo".into(),
                data_dir.join("cli-sessions"),
                false,
            )
            .await
            .unwrap(),
        );

        let engine = Arc::new(
            threshold_conversation::ConversationEngine::new(
                &config,
                claude,
                None,
                None,
            )
            .await
            .unwrap(),
        );

        let cancel = tokio_util::sync::CancellationToken::new();
        let templates = templates::build_template_env();

        let state = AppState {
            engine,
            scheduler_handle: None,
            secret_store: Arc::new(threshold_core::SecretStore::new().unwrap()),
            config,
            config_path: data_dir.join("config.toml"),
            data_dir,
            cancel: cancel.clone(),
            start_time: chrono::Utc::now(),
            templates,
        };

        let app = routes::build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(cancel_clone.cancelled_owned())
                .await
                .unwrap();
        });

        // Brief yield to let the server start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        (addr, cancel)
    }

    #[tokio::test]
    async fn dashboard_returns_200() {
        let (addr, cancel) = start_test_server().await;
        let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("Threshold"));
        cancel.cancel();
    }

    #[tokio::test]
    async fn status_json_returns_200() {
        let (addr, cancel) = start_test_server().await;
        let resp = reqwest::get(format!("http://{addr}/status")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert!(json.get("uptime").is_some());
        assert!(json.get("conversation_count").is_some());
        cancel.cancel();
    }

    #[tokio::test]
    async fn static_files_served() {
        let (addr, cancel) = start_test_server().await;
        let resp = reqwest::get(format!("http://{addr}/static/htmx.min.js"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let resp = reqwest::get(format!("http://{addr}/static/style.css"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        cancel.cancel();
    }

    #[tokio::test]
    async fn nonexistent_route_returns_404() {
        let (addr, cancel) = start_test_server().await;
        let resp = reqwest::get(format!("http://{addr}/nonexistent"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        cancel.cancel();
    }

    #[tokio::test]
    async fn conversations_list_returns_200() {
        let (addr, cancel) = start_test_server().await;
        let resp = reqwest::get(format!("http://{addr}/conversations"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("Conversations"));
        cancel.cancel();
    }

    #[tokio::test]
    async fn conversations_invalid_id_returns_404() {
        let (addr, cancel) = start_test_server().await;
        let resp = reqwest::get(format!("http://{addr}/conversations/not-a-uuid"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        cancel.cancel();
    }

    #[tokio::test]
    async fn conversations_nonexistent_id_returns_404() {
        let (addr, cancel) = start_test_server().await;
        let resp = reqwest::get(format!(
            "http://{addr}/conversations/00000000-0000-0000-0000-000000000000"
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 404);
        cancel.cancel();
    }

    #[tokio::test]
    async fn delete_without_csrf_returns_403() {
        let (addr, cancel) = start_test_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "http://{addr}/conversations/00000000-0000-0000-0000-000000000000/delete"
            ))
            .form(&[("_csrf", "invalid-token")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        cancel.cancel();
    }

    #[test]
    fn non_loopback_bind_rejected() {
        assert!(!threshold_core::config::is_loopback_address("0.0.0.0"));
        assert!(!threshold_core::config::is_loopback_address("::"));
        assert!(!threshold_core::config::is_loopback_address("192.168.1.1"));
        assert!(!threshold_core::config::is_loopback_address("10.0.0.1"));
    }

    #[test]
    fn loopback_bind_accepted() {
        assert!(threshold_core::config::is_loopback_address("127.0.0.1"));
        assert!(threshold_core::config::is_loopback_address("::1"));
        assert!(threshold_core::config::is_loopback_address("localhost"));
    }
}
