//! Persistent storage for conversation metadata.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use threshold_core::{CliProvider, Conversation, ConversationId, ConversationMode, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConversationMap {
    conversations: HashMap<ConversationId, Conversation>,
}

/// Persistent storage for conversation metadata.
pub struct ConversationStore {
    conversations: HashMap<ConversationId, Conversation>,
    state_path: PathBuf,
}

impl ConversationStore {
    /// Load conversations from disk
    pub async fn load(data_dir: &Path) -> Result<Self> {
        let state_path = data_dir.join("conversations.json");

        if !state_path.exists() {
            return Ok(Self {
                conversations: HashMap::new(),
                state_path,
            });
        }

        let content = tokio::fs::read_to_string(&state_path).await?;

        // Handle corruption gracefully
        let map: ConversationMap = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = ?state_path,
                    "conversation file corrupted, resetting to empty"
                );
                ConversationMap {
                    conversations: HashMap::new(),
                }
            }
        };

        tracing::info!(
            count = map.conversations.len(),
            "loaded conversations from disk"
        );

        Ok(Self {
            conversations: map.conversations,
            state_path,
        })
    }

    /// Save conversations to disk
    ///
    /// Note: This clones the entire conversation map for serialization.
    /// Called on every message to ensure durability of last_active updates.
    /// For high-throughput scenarios, consider implementing debouncing.
    pub async fn save(&self) -> Result<()> {
        let map = ConversationMap {
            conversations: self.conversations.clone(),
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
            perms.set_mode(0o600); // rw------- (owner only)
            tokio::fs::set_permissions(&self.state_path, perms).await?;
        }

        Ok(())
    }

    /// Create a new conversation and return its ID
    pub fn create(
        &mut self,
        mode: ConversationMode,
        cli_provider: CliProvider,
        agent_id: String,
    ) -> ConversationId {
        let now = Utc::now();
        let conversation = Conversation {
            id: ConversationId::new(),
            mode,
            cli_provider,
            agent_id,
            created_at: now,
            last_active: now,
        };

        let id = conversation.id;
        self.conversations.insert(id, conversation);
        id
    }

    /// Get a conversation by ID
    pub fn get(&self, id: &ConversationId) -> Option<&Conversation> {
        self.conversations.get(id)
    }

    /// Get a mutable reference to a conversation
    pub fn get_mut(&mut self, id: &ConversationId) -> Option<&mut Conversation> {
        self.conversations.get_mut(id)
    }

    /// List all conversations
    pub fn list(&self) -> Vec<&Conversation> {
        self.conversations.values().collect()
    }

    /// Find a conversation by its mode key
    pub fn find_by_mode(&self, mode: &ConversationMode) -> Option<&Conversation> {
        let key = mode.key();
        self.conversations.values().find(|c| c.mode.key() == key)
    }

    /// Get or create the singleton General conversation
    ///
    /// Returns (conversation_id, was_created) tuple
    pub fn get_or_create_general(
        &mut self,
        agent_id: String,
        cli_provider: CliProvider,
    ) -> (ConversationId, bool) {
        // Check if General already exists
        if let Some(conv) = self.find_by_mode(&ConversationMode::General) {
            return (conv.id, false);
        }

        // Create it
        let id = self.create(ConversationMode::General, cli_provider, agent_id);
        (id, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use threshold_core::CliProvider;

    #[tokio::test]
    async fn load_nonexistent_creates_empty() {
        let dir = tempdir().unwrap();
        let store = ConversationStore::load(dir.path()).await.unwrap();

        assert_eq!(store.list().len(), 0);
    }

    #[tokio::test]
    async fn create_and_get_conversation() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        let conv = store.get(&id).unwrap();
        assert_eq!(conv.id, id);
        assert_eq!(conv.agent_id, "default");
    }

    #[tokio::test]
    async fn list_returns_all_conversations() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        store.create(
            ConversationMode::Coding {
                project: "test".to_string(),
            },
            CliProvider::Claude {
                model: "opus".to_string(),
            },
            "coder".to_string(),
        );

        assert_eq!(store.list().len(), 2);
    }

    #[tokio::test]
    async fn find_by_mode_general() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        let found = store.find_by_mode(&ConversationMode::General).unwrap();
        assert_eq!(found.id, id);
    }

    #[tokio::test]
    async fn find_by_mode_coding_case_insensitive() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::Coding {
                project: "MyProject".to_string(),
            },
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "coder".to_string(),
        );

        // Search with different case
        let found = store
            .find_by_mode(&ConversationMode::Coding {
                project: "myproject".to_string(),
            })
            .unwrap();
        assert_eq!(found.id, id);
    }

    #[tokio::test]
    async fn get_or_create_general_singleton() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let (id1, was_created1) = store.get_or_create_general(
            "default".to_string(),
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
        );

        let (id2, was_created2) = store.get_or_create_general(
            "default".to_string(),
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
        );

        assert!(was_created1);
        assert!(!was_created2);
        assert_eq!(id1, id2);
    }

    #[tokio::test]
    async fn save_and_load_persistence() {
        let dir = tempdir().unwrap();
        let path = dir.path();

        let id = {
            let mut store = ConversationStore::load(path).await.unwrap();
            let id = store.create(
                ConversationMode::General,
                CliProvider::Claude {
                    model: "sonnet".to_string(),
                },
                "default".to_string(),
            );
            store.save().await.unwrap();
            id
        };

        // Load in new instance
        let store = ConversationStore::load(path).await.unwrap();
        let conv = store.get(&id).unwrap();
        assert_eq!(conv.id, id);
        assert_eq!(conv.agent_id, "default");
    }

    #[tokio::test]
    async fn load_corrupted_file_resets_to_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("conversations.json");

        // Write invalid JSON
        tokio::fs::write(&path, "not valid json {[}").await.unwrap();

        let store = ConversationStore::load(dir.path()).await.unwrap();
        assert_eq!(store.list().len(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_permissions_are_0o600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );
        store.save().await.unwrap();

        let path = dir.path().join("conversations.json");
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        let permissions = metadata.permissions();

        assert_eq!(permissions.mode() & 0o777, 0o600);
    }
}
