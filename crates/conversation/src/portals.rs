//! Portal registry - tracks which portals are connected and what conversation each is attached to.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use threshold_core::{ConversationId, Portal, PortalId, PortalType, Result, ThresholdError};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PortalMap {
    portals: HashMap<PortalId, Portal>,
}

/// Track which portals are connected and what conversation each is attached to.
pub struct PortalRegistry {
    portals: HashMap<PortalId, Portal>,
    state_path: PathBuf,
}

impl PortalRegistry {
    /// Load portals from disk
    pub async fn load(data_dir: &Path) -> Result<Self> {
        let state_path = data_dir.join("portals.json");

        if !state_path.exists() {
            return Ok(Self {
                portals: HashMap::new(),
                state_path,
            });
        }

        let content = tokio::fs::read_to_string(&state_path).await?;

        // Handle corruption gracefully
        let map: PortalMap = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = ?state_path,
                    "portal file corrupted, resetting to empty"
                );
                PortalMap {
                    portals: HashMap::new(),
                }
            }
        };

        tracing::info!(count = map.portals.len(), "loaded portals from disk");

        Ok(Self {
            portals: map.portals,
            state_path,
        })
    }

    /// Save portals to disk
    pub async fn save(&self) -> Result<()> {
        let map = PortalMap {
            portals: self.portals.clone(),
        };

        // Create parent directory
        if let Some(parent) = self.state_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let json = serde_json::to_string_pretty(&map)?;
        tokio::fs::write(&self.state_path, json).await?;

        // Set restrictive permissions (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&self.state_path).await?.permissions();
            perms.set_mode(0o600);
            tokio::fs::set_permissions(&self.state_path, perms).await?;
        }

        Ok(())
    }

    /// Register a new portal
    pub fn register(&mut self, portal_type: PortalType, conversation_id: ConversationId) -> Portal {
        let portal = Portal {
            id: PortalId::new(),
            portal_type,
            conversation_id,
            connected_at: Utc::now(),
        };

        let id = portal.id;
        self.portals.insert(id, portal.clone());
        portal
    }

    /// Remove a portal entirely
    pub fn unregister(&mut self, portal_id: &PortalId) {
        self.portals.remove(portal_id);
    }

    /// Move a portal to a new conversation
    pub fn attach(&mut self, portal_id: &PortalId, conversation_id: ConversationId) -> Result<()> {
        let portal = self
            .portals
            .get_mut(portal_id)
            .ok_or(ThresholdError::PortalNotFound { id: portal_id.0 })?;

        portal.conversation_id = conversation_id;
        Ok(())
    }

    /// Get a portal by ID
    pub fn get(&self, portal_id: &PortalId) -> Option<&Portal> {
        self.portals.get(portal_id)
    }

    /// Get which conversation a portal is in
    pub fn get_conversation(&self, portal_id: &PortalId) -> Option<&ConversationId> {
        self.portals.get(portal_id).map(|p| &p.conversation_id)
    }

    /// Get all portals in a conversation (for broadcasting)
    pub fn get_portals_for_conversation(&self, conversation_id: &ConversationId) -> Vec<Portal> {
        self.portals
            .values()
            .filter(|p| &p.conversation_id == conversation_id)
            .cloned()
            .collect()
    }

    /// Find a portal by Discord channel
    pub fn find_by_discord_channel(&self, guild_id: u64, channel_id: u64) -> Option<&Portal> {
        self.portals.values().find(|p| match &p.portal_type {
            PortalType::Discord {
                guild_id: g,
                channel_id: c,
            } => *g == guild_id && *c == channel_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn register_portal_returns_new_id() {
        let dir = tempdir().unwrap();
        let mut registry = PortalRegistry::load(dir.path()).await.unwrap();

        let portal = registry.register(
            PortalType::Discord {
                guild_id: 123,
                channel_id: 456,
            },
            ConversationId::new(),
        );

        assert!(registry.get(&portal.id).is_some());
    }

    #[tokio::test]
    async fn unregister_removes_portal() {
        let dir = tempdir().unwrap();
        let mut registry = PortalRegistry::load(dir.path()).await.unwrap();

        let portal = registry.register(
            PortalType::Discord {
                guild_id: 123,
                channel_id: 456,
            },
            ConversationId::new(),
        );

        registry.unregister(&portal.id);

        assert!(registry.get(&portal.id).is_none());
    }

    #[tokio::test]
    async fn attach_updates_conversation_id() {
        let dir = tempdir().unwrap();
        let mut registry = PortalRegistry::load(dir.path()).await.unwrap();

        let conv1 = ConversationId::new();
        let conv2 = ConversationId::new();

        let portal = registry.register(
            PortalType::Discord {
                guild_id: 123,
                channel_id: 456,
            },
            conv1,
        );

        registry.attach(&portal.id, conv2).unwrap();

        assert_eq!(registry.get_conversation(&portal.id), Some(&conv2));
    }

    #[tokio::test]
    async fn attach_nonexistent_portal_returns_error() {
        let dir = tempdir().unwrap();
        let mut registry = PortalRegistry::load(dir.path()).await.unwrap();

        let result = registry.attach(&PortalId::new(), ConversationId::new());

        assert!(result.is_err());
        match result.unwrap_err() {
            ThresholdError::PortalNotFound { .. } => {
                // Expected
            }
            _ => panic!("expected PortalNotFound error"),
        }
    }

    #[tokio::test]
    async fn get_conversation_returns_current() {
        let dir = tempdir().unwrap();
        let mut registry = PortalRegistry::load(dir.path()).await.unwrap();

        let conv_id = ConversationId::new();
        let portal = registry.register(
            PortalType::Discord {
                guild_id: 123,
                channel_id: 456,
            },
            conv_id,
        );

        assert_eq!(registry.get_conversation(&portal.id), Some(&conv_id));
    }

    #[tokio::test]
    async fn get_portals_for_conversation_filters_correctly() {
        let dir = tempdir().unwrap();
        let mut registry = PortalRegistry::load(dir.path()).await.unwrap();

        let conv1 = ConversationId::new();
        let conv2 = ConversationId::new();

        let portal1 = registry.register(
            PortalType::Discord {
                guild_id: 1,
                channel_id: 1,
            },
            conv1,
        );

        let portal2 = registry.register(
            PortalType::Discord {
                guild_id: 2,
                channel_id: 2,
            },
            conv1,
        );

        registry.register(
            PortalType::Discord {
                guild_id: 3,
                channel_id: 3,
            },
            conv2,
        );

        let portals = registry.get_portals_for_conversation(&conv1);
        assert_eq!(portals.len(), 2);
        assert!(portals.iter().any(|p| p.id == portal1.id));
        assert!(portals.iter().any(|p| p.id == portal2.id));
    }

    #[tokio::test]
    async fn find_by_discord_channel_match() {
        let dir = tempdir().unwrap();
        let mut registry = PortalRegistry::load(dir.path()).await.unwrap();

        let portal = registry.register(
            PortalType::Discord {
                guild_id: 123,
                channel_id: 456,
            },
            ConversationId::new(),
        );

        let found = registry.find_by_discord_channel(123, 456).unwrap();
        assert_eq!(found.id, portal.id);
    }

    #[tokio::test]
    async fn find_by_discord_channel_no_match_returns_none() {
        let dir = tempdir().unwrap();
        let registry = PortalRegistry::load(dir.path()).await.unwrap();

        let found = registry.find_by_discord_channel(999, 999);
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn save_and_load_persistence() {
        let dir = tempdir().unwrap();
        let path = dir.path();

        let portal_id = {
            let mut registry = PortalRegistry::load(path).await.unwrap();
            let portal = registry.register(
                PortalType::Discord {
                    guild_id: 123,
                    channel_id: 456,
                },
                ConversationId::new(),
            );
            registry.save().await.unwrap();
            portal.id
        };

        // Load in new instance
        let registry = PortalRegistry::load(path).await.unwrap();
        let portal = registry.get(&portal_id).unwrap();
        assert_eq!(portal.id, portal_id);
    }

    #[tokio::test]
    async fn load_corrupted_file_resets_to_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("portals.json");

        // Write invalid JSON
        tokio::fs::write(&path, "not valid json {[}").await.unwrap();

        let registry = PortalRegistry::load(dir.path()).await.unwrap();
        assert_eq!(registry.portals.len(), 0);
    }
}
