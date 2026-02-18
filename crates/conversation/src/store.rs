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
    data_dir: PathBuf,
}

impl ConversationStore {
    /// Load conversations from disk
    pub async fn load(data_dir: &Path) -> Result<Self> {
        let state_path = data_dir.join("conversations.json");

        if !state_path.exists() {
            return Ok(Self {
                conversations: HashMap::new(),
                state_path,
                data_dir: data_dir.to_path_buf(),
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
            data_dir: data_dir.to_path_buf(),
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

    /// Create a new conversation and return its ID.
    ///
    /// Also creates the conversation directory (`conversations/{id}/`) and seeds
    /// `memory.md` with mode-appropriate defaults.
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
        let conv_mode = conversation.mode.clone();
        self.conversations.insert(id, conversation);

        // Create conversation directory and seed memory file
        self.ensure_conversation_dir(&id, &conv_mode);

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

    /// Remove a conversation from the store.
    ///
    /// Returns the removed conversation, or `None` if it didn't exist.
    /// Does not delete the conversation directory — that is the engine's responsibility.
    pub fn remove(&mut self, id: &ConversationId) -> Option<Conversation> {
        self.conversations.remove(id)
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
    /// Returns (conversation_id, was_created) tuple.
    /// Also ensures the conversation directory and memory file exist,
    /// even for pre-existing General conversations from before Milestone 12.
    pub fn get_or_create_general(
        &mut self,
        agent_id: String,
        cli_provider: CliProvider,
    ) -> (ConversationId, bool) {
        // Check if General already exists
        if let Some(conv) = self.find_by_mode(&ConversationMode::General) {
            let id = conv.id;
            // Ensure directory exists (backfill for pre-existing conversations)
            self.ensure_conversation_dir(&id, &ConversationMode::General);
            return (id, false);
        }

        // Create it (create() handles directory + seeding)
        let id = self.create(ConversationMode::General, cli_provider, agent_id);
        (id, true)
    }

    /// Create the conversation directory and seed memory.md if it doesn't exist.
    ///
    /// Safe to call multiple times — only creates if missing.
    /// Used for backfilling pre-Milestone-12 conversations that lack directories.
    pub(crate) fn ensure_conversation_dir(&self, id: &ConversationId, mode: &ConversationMode) {
        let conv_dir = self
            .data_dir
            .join("conversations")
            .join(id.0.to_string());

        if let Err(e) = std::fs::create_dir_all(&conv_dir) {
            tracing::warn!(
                conversation_id = %id.0,
                error = %e,
                "failed to create conversation directory"
            );
            return;
        }

        let memory_path = conv_dir.join("memory.md");
        if !memory_path.exists() {
            let content = Self::default_memory_content(mode);
            if let Err(e) = std::fs::write(&memory_path, content) {
                tracing::warn!(
                    conversation_id = %id.0,
                    error = %e,
                    "failed to seed memory.md"
                );
            }
        }
    }

    /// Generate default memory.md content based on conversation mode.
    pub fn default_memory_content(mode: &ConversationMode) -> String {
        match mode {
            ConversationMode::Coding { project } => format!(
                "# Conversation Memory\n\
                 \n\
                 ## Project\n\
                 {project}\n\
                 \n\
                 ## Tools & Workflows\n\
                 - **Codex CLI** — Use for all review cycles (planning, code, architecture). Run until all findings resolved.\n\
                 \x20 ```bash\n\
                 \x20 codex exec --full-auto \"your prompt\"\n\
                 \x20 codex exec resume <session-id> --full-auto \"follow-up prompt\"\n\
                 \x20 ```\n\
                 - **Playwright CLI** — Use for browser access, end-to-end testing, and tasks beyond normal web tools.\n\
                 \x20 ```bash\n\
                 \x20 playwright-cli --help\n\
                 \x20 ```\n\
                 \n\
                 ## Notes\n\
                 (Agent: update this section as you work. Record decisions, progress, blockers, and anything important to remember across sessions.)\n"
            ),
            ConversationMode::Research { topic } => format!(
                "# Conversation Memory\n\
                 \n\
                 ## Topic\n\
                 {topic}\n\
                 \n\
                 ## Notes\n\
                 (Agent: update this section as you work. Record findings, sources, key conclusions, and anything important to remember across sessions.)\n"
            ),
            ConversationMode::General => "# Conversation Memory\n\
                 \n\
                 ## Notes\n\
                 (Agent: update this section as you work. Record anything important to remember across sessions.)\n"
                .to_string(),
        }
    }

    /// Get the data directory path
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
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

    #[tokio::test]
    async fn create_creates_conversation_directory() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        let conv_dir = dir.path().join("conversations").join(id.0.to_string());
        assert!(conv_dir.exists(), "conversation directory should be created");
        assert!(conv_dir.is_dir());
    }

    #[tokio::test]
    async fn create_seeds_memory_file_general() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        let memory_path = dir
            .path()
            .join("conversations")
            .join(id.0.to_string())
            .join("memory.md");
        assert!(memory_path.exists(), "memory.md should be seeded");

        let content = std::fs::read_to_string(&memory_path).unwrap();
        assert!(content.contains("# Conversation Memory"));
        assert!(content.contains("## Notes"));
        // General mode should NOT have Project or Topic sections
        assert!(!content.contains("## Project"));
        assert!(!content.contains("## Topic"));
    }

    #[tokio::test]
    async fn create_seeds_memory_file_coding() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::Coding {
                project: "threshold".to_string(),
            },
            CliProvider::Claude {
                model: "opus".to_string(),
            },
            "coder".to_string(),
        );

        let memory_path = dir
            .path()
            .join("conversations")
            .join(id.0.to_string())
            .join("memory.md");
        let content = std::fs::read_to_string(&memory_path).unwrap();
        assert!(content.contains("## Project"));
        assert!(content.contains("threshold"));
        assert!(content.contains("Codex CLI"));
        assert!(content.contains("Playwright CLI"));
    }

    #[tokio::test]
    async fn create_seeds_memory_file_research() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::Research {
                topic: "quantum computing".to_string(),
            },
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        let memory_path = dir
            .path()
            .join("conversations")
            .join(id.0.to_string())
            .join("memory.md");
        let content = std::fs::read_to_string(&memory_path).unwrap();
        assert!(content.contains("## Topic"));
        assert!(content.contains("quantum computing"));
    }

    #[tokio::test]
    async fn get_or_create_general_backfills_directory() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        // First call creates
        let (id, _) = store.get_or_create_general(
            "default".to_string(),
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
        );

        // Delete the directory to simulate pre-M12 conversation
        let conv_dir = dir.path().join("conversations").join(id.0.to_string());
        std::fs::remove_dir_all(&conv_dir).unwrap();
        assert!(!conv_dir.exists());

        // Second call should backfill
        let (id2, was_created) = store.get_or_create_general(
            "default".to_string(),
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
        );
        assert!(!was_created);
        assert_eq!(id, id2);
        assert!(conv_dir.exists(), "directory should be backfilled");
        assert!(conv_dir.join("memory.md").exists());
    }

    #[tokio::test]
    async fn create_does_not_overwrite_existing_memory() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        // Write custom content to memory file
        let memory_path = dir
            .path()
            .join("conversations")
            .join(id.0.to_string())
            .join("memory.md");
        std::fs::write(&memory_path, "# Custom memory content").unwrap();

        // Calling get_or_create_general should NOT overwrite
        let (_, _) = store.get_or_create_general(
            "default".to_string(),
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
        );

        let content = std::fs::read_to_string(&memory_path).unwrap();
        assert_eq!(content, "# Custom memory content");
    }

    #[tokio::test]
    async fn default_memory_content_coding_includes_project() {
        let content = ConversationStore::default_memory_content(&ConversationMode::Coding {
            project: "my-app".to_string(),
        });
        assert!(content.starts_with("# Conversation Memory"));
        assert!(content.contains("my-app"));
        assert!(content.contains("codex exec"));
        assert!(content.contains("playwright-cli"));
    }

    #[tokio::test]
    async fn default_memory_content_research_includes_topic() {
        let content = ConversationStore::default_memory_content(&ConversationMode::Research {
            topic: "AI safety".to_string(),
        });
        assert!(content.contains("AI safety"));
        assert!(content.contains("## Topic"));
    }

    #[tokio::test]
    async fn remove_returns_conversation_and_deletes_from_store() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let id = store.create(
            ConversationMode::General,
            CliProvider::Claude {
                model: "sonnet".to_string(),
            },
            "default".to_string(),
        );

        assert_eq!(store.list().len(), 1);

        let removed = store.remove(&id);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id, id);
        assert_eq!(store.list().len(), 0);
        assert!(store.get(&id).is_none());
    }

    #[tokio::test]
    async fn remove_nonexistent_returns_none() {
        let dir = tempdir().unwrap();
        let mut store = ConversationStore::load(dir.path()).await.unwrap();

        let fake_id = threshold_core::ConversationId::new();
        assert!(store.remove(&fake_id).is_none());
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
