//! CLI session state management.
//!
//! Tracks CLI session IDs and persists them to disk for session continuity
//! across server restarts.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use threshold_core::Result;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMap {
    sessions: HashMap<Uuid, String>,
}

/// Manages CLI session IDs and their persistence
pub struct SessionManager {
    sessions: RwLock<HashMap<Uuid, String>>,
    state_path: PathBuf,
}

impl SessionManager {
    /// Create a new session manager
    pub fn new(state_path: PathBuf) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            state_path,
        }
    }

    /// Load sessions from disk
    pub async fn load(&self) -> Result<()> {
        if !self.state_path.exists() {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(&self.state_path).await?;

        // Handle corruption gracefully - reset to empty map on parse error
        let map: SessionMap = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = ?self.state_path,
                    "session file corrupted, resetting to empty"
                );
                SessionMap {
                    sessions: HashMap::new(),
                }
            }
        };

        let mut sessions = self.sessions.write().await;
        *sessions = map.sessions;

        tracing::info!(count = sessions.len(), "loaded CLI sessions from disk");
        Ok(())
    }

    /// Save sessions to disk
    pub async fn save(&self) -> Result<()> {
        let sessions = self.sessions.read().await;
        let map = SessionMap {
            sessions: sessions.clone(),
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

    /// Get the CLI session ID for a conversation
    pub async fn get(&self, conversation_id: Uuid) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions.get(&conversation_id).cloned()
    }

    /// Store a CLI session ID for a conversation
    pub async fn set(&self, conversation_id: Uuid, session_id: String) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.insert(conversation_id, session_id);
        drop(sessions);

        // Persist to disk
        self.save().await?;
        Ok(())
    }

    /// Remove a session (e.g., on error/reset)
    pub async fn remove(&self, conversation_id: Uuid) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.remove(&conversation_id);
        drop(sessions);

        self.save().await?;
        Ok(())
    }

    /// Get count of stored sessions (for diagnostics)
    pub async fn count(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn new_session_manager_is_empty() {
        let dir = tempdir().unwrap();
        let manager = SessionManager::new(dir.path().join("sessions.json"));

        assert_eq!(manager.count().await, 0);
    }

    #[tokio::test]
    async fn set_and_get_session() {
        let dir = tempdir().unwrap();
        let manager = SessionManager::new(dir.path().join("sessions.json"));

        let conv_id = Uuid::new_v4();
        let session_id = "test-session-123".to_string();

        manager.set(conv_id, session_id.clone()).await.unwrap();

        let retrieved = manager.get(conv_id).await;
        assert_eq!(retrieved, Some(session_id));
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let dir = tempdir().unwrap();
        let manager = SessionManager::new(dir.path().join("sessions.json"));

        let conv_id = Uuid::new_v4();
        let retrieved = manager.get(conv_id).await;

        assert_eq!(retrieved, None);
    }

    #[tokio::test]
    async fn save_and_load_persists_sessions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let conv_id = Uuid::new_v4();
        let session_id = "persisted-session".to_string();

        // Create manager, set session, save
        {
            let manager = SessionManager::new(path.clone());
            manager.set(conv_id, session_id.clone()).await.unwrap();
        }

        // Create new manager, load
        {
            let manager = SessionManager::new(path);
            manager.load().await.unwrap();

            let retrieved = manager.get(conv_id).await;
            assert_eq!(retrieved, Some(session_id));
        }
    }

    #[tokio::test]
    async fn remove_deletes_session() {
        let dir = tempdir().unwrap();
        let manager = SessionManager::new(dir.path().join("sessions.json"));

        let conv_id = Uuid::new_v4();
        manager
            .set(conv_id, "session-123".to_string())
            .await
            .unwrap();

        assert!(manager.get(conv_id).await.is_some());

        manager.remove(conv_id).await.unwrap();

        assert_eq!(manager.get(conv_id).await, None);
    }

    #[tokio::test]
    async fn load_nonexistent_file_succeeds() {
        let dir = tempdir().unwrap();
        let manager = SessionManager::new(dir.path().join("nonexistent.json"));

        // Should not error
        manager.load().await.unwrap();
        assert_eq!(manager.count().await, 0);
    }

    #[tokio::test]
    async fn load_corrupted_file_resets_to_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupted.json");

        // Write invalid JSON
        tokio::fs::write(&path, "not valid json {[}").await.unwrap();

        let manager = SessionManager::new(path);
        manager.load().await.unwrap();

        // Should have reset to empty
        assert_eq!(manager.count().await, 0);
    }

    #[tokio::test]
    async fn multiple_sessions_tracked() {
        let dir = tempdir().unwrap();
        let manager = SessionManager::new(dir.path().join("sessions.json"));

        let conv1 = Uuid::new_v4();
        let conv2 = Uuid::new_v4();
        let conv3 = Uuid::new_v4();

        manager.set(conv1, "session-1".to_string()).await.unwrap();
        manager.set(conv2, "session-2".to_string()).await.unwrap();
        manager.set(conv3, "session-3".to_string()).await.unwrap();

        assert_eq!(manager.count().await, 3);
        assert_eq!(manager.get(conv1).await, Some("session-1".to_string()));
        assert_eq!(manager.get(conv2).await, Some("session-2".to_string()));
        assert_eq!(manager.get(conv3).await, Some("session-3".to_string()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_file_has_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let manager = SessionManager::new(path.clone());

        manager
            .set(Uuid::new_v4(), "test".to_string())
            .await
            .unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        let permissions = metadata.permissions();

        assert_eq!(permissions.mode() & 0o777, 0o600);
    }
}
