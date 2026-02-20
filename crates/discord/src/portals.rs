//! Channel-as-portal mapping.

use threshold_conversation::ConversationEngine;
use threshold_core::{PortalId, PortalType};

/// Resolve an existing portal for this channel, or create a new one
/// attached to the General conversation.
pub async fn resolve_or_create_portal(
    engine: &ConversationEngine,
    guild_id: u64,
    channel_id: u64,
) -> PortalId {
    // Try to find existing portal
    {
        let portals_arc = engine.portals();
        let portals = portals_arc.read().await;
        if let Some(portal) = portals.find_by_discord_channel(guild_id, channel_id) {
            return portal.id;
        }
        // Drop read lock before creating new portal
    }

    // Create new portal attached to General conversation
    let portal_type = PortalType::Discord {
        guild_id,
        channel_id,
    };

    engine
        .register_portal(portal_type)
        .await
        .expect("Failed to register portal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use threshold_cli_wrapper::ClaudeClient;
    use threshold_core::config::{AgentConfigToml, ClaudeCliConfig, CliConfig, ThresholdConfig};

    fn test_config() -> ThresholdConfig {
        ThresholdConfig {
            data_dir: Some(std::env::temp_dir()),
            log_level: None,
            secret_backend: None,
            cli: CliConfig {
                claude: ClaudeCliConfig {
                    command: Some("claude".to_string()),
                    model: Some("sonnet".to_string()),
                    timeout_seconds: None,
                    skip_permissions: None,
                    extra_flags: vec![],
                },
            },
            discord: None,
            agents: vec![AgentConfigToml {
                id: "default".to_string(),
                name: "Default".to_string(),
                cli_provider: "claude".to_string(),
                model: None,
                system_prompt: None,
                system_prompt_file: None,
                tools: None,
            }],
            tools: Default::default(),
            heartbeat: None,
            scheduler: None,
            web: None,
        }
    }

    #[tokio::test]
    async fn resolve_creates_new_portal_for_new_channel() {
        let config = test_config();
        let sessions = Arc::new(
            threshold_cli_wrapper::session::SessionManager::new(
                config.data_dir().unwrap().join("cli-sessions").join("cli-sessions.json"),
            ),
        );
        let locks = Arc::new(threshold_cli_wrapper::ConversationLockMap::new());
        let tracker = Arc::new(threshold_cli_wrapper::ProcessTracker::new());
        let claude = Arc::new(
            ClaudeClient::new(
                config.cli.claude.command.clone().unwrap_or_else(|| "claude".to_string()),
                config.data_dir().unwrap().join("cli-sessions"),
                config.cli.claude.skip_permissions.unwrap_or(false),
                300,
                sessions,
                locks,
                tracker,
            )
            .await
            .unwrap(),
        );
        let engine = ConversationEngine::new(&config, claude, None, None).await.unwrap();

        let portal_id = resolve_or_create_portal(&engine, 123, 456).await;

        // Verify portal exists
        let portals_arc = engine.portals();
        let portals = portals_arc.read().await;
        let portal = portals.get(&portal_id).unwrap();
        assert_eq!(portal.id, portal_id);
    }

    #[tokio::test]
    async fn resolve_reuses_existing_portal() {
        let config = test_config();
        let sessions = Arc::new(
            threshold_cli_wrapper::session::SessionManager::new(
                config.data_dir().unwrap().join("cli-sessions").join("cli-sessions.json"),
            ),
        );
        let locks = Arc::new(threshold_cli_wrapper::ConversationLockMap::new());
        let tracker = Arc::new(threshold_cli_wrapper::ProcessTracker::new());
        let claude = Arc::new(
            ClaudeClient::new(
                config.cli.claude.command.clone().unwrap_or_else(|| "claude".to_string()),
                config.data_dir().unwrap().join("cli-sessions"),
                config.cli.claude.skip_permissions.unwrap_or(false),
                300,
                sessions,
                locks,
                tracker,
            )
            .await
            .unwrap(),
        );
        let engine = ConversationEngine::new(&config, claude, None, None).await.unwrap();

        let portal_id1 = resolve_or_create_portal(&engine, 123, 456).await;
        let portal_id2 = resolve_or_create_portal(&engine, 123, 456).await;

        assert_eq!(portal_id1, portal_id2);
    }
}
