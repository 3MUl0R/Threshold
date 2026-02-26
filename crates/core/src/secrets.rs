//! Secret store with pluggable backends (file or OS keychain).
//!
//! # Backends
//!
//! - **File** (default): Stores secrets in `<data_dir>/secrets.toml` with 0600 permissions.
//!   Suitable for development, headless servers, and most deployments.
//! - **Keychain**: Uses the OS native keychain (macOS Keychain, Windows Credential Manager,
//!   Linux Secret Service). Opt-in via `secret_backend = "keychain"` in config.
//!
//! # Resolution Priority
//!
//! When resolving secrets via `resolve()`:
//! 1. Check the configured backend first
//! 2. Fall back to environment variable if backend returns `Ok(None)`
//! 3. Return `Err` if backend fails (fatal error)

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use fs2::FileExt;
use keyring::Entry;

/// Which secret storage backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretBackend {
    /// File-based storage (`<data_dir>/secrets.toml`, chmod 600).
    File,
    /// OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret Service).
    Keychain,
}

impl Default for SecretBackend {
    fn default() -> Self {
        Self::File
    }
}

/// File-based secret storage.
///
/// Secrets are stored as TOML in a `[secrets]` table with 0600 permissions.
/// Uses `fs2` file locking via a separate `.lock` file for cross-process safety.
/// No in-memory cache — every operation re-reads from disk for consistency.
struct FileStore {
    path: PathBuf,
    lock_path: PathBuf,
}

/// TOML structure: `[secrets]` table containing key-value pairs.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SecretsFile {
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

impl FileStore {
    /// Create a new file-based secret store at the given path.
    ///
    /// - Rejects symlink paths (security: prevent symlink attacks)
    /// - Creates parent directories if needed
    /// - Auto-fixes permissions to 0600 if existing file has wrong perms (Unix)
    fn new(path: PathBuf) -> crate::Result<Self> {
        // Reject symlinks
        if path.is_symlink() {
            return Err(crate::ThresholdError::Config(format!(
                "Secrets file path is a symlink (security risk): {}",
                path.display()
            )));
        }

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to create secrets directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        // If file exists, fix permissions if needed
        #[cfg(unix)]
        if path.exists() {
            Self::ensure_permissions(&path)?;
        }

        let lock_path = path.with_extension("toml.lock");

        Ok(Self { path, lock_path })
    }

    /// Ensure file has 0600 permissions (Unix only).
    #[cfg(unix)]
    fn ensure_permissions(path: &std::path::Path) -> crate::Result<()> {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(path).map_err(|e| {
            crate::ThresholdError::Keychain(format!(
                "Failed to read metadata for {}: {}",
                path.display(),
                e
            ))
        })?;

        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            tracing::warn!(
                "Secrets file {} has permissions {:o}, fixing to 600",
                path.display(),
                mode
            );
            fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .map_err(|e| {
                    crate::ThresholdError::Keychain(format!(
                        "Failed to set permissions on {}: {}",
                        path.display(),
                        e
                    ))
                })?;
        }

        Ok(())
    }

    /// Read secrets from the TOML file, with shared lock.
    fn read_file(&self) -> crate::Result<BTreeMap<String, String>> {
        // Acquire shared lock first to avoid TOCTOU race on existence check
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to open lock file {}: {}",
                    self.lock_path.display(),
                    e
                ))
            })?;
        lock_file.lock_shared().map_err(|e| {
            crate::ThresholdError::Keychain(format!("Failed to acquire shared lock: {}", e))
        })?;

        // Check existence under lock
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }

        let contents = fs::read_to_string(&self.path).map_err(|e| {
            crate::ThresholdError::Keychain(format!(
                "Failed to read secrets file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        // Release lock (drop)
        drop(lock_file);

        if contents.trim().is_empty() {
            return Ok(BTreeMap::new());
        }

        let secrets_file: SecretsFile = toml::from_str(&contents).map_err(|e| {
            crate::ThresholdError::Keychain(format!(
                "Failed to parse secrets file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        Ok(secrets_file.secrets)
    }

    fn get(&self, key: &str) -> crate::Result<Option<String>> {
        let secrets = self.read_file()?;
        Ok(secrets.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> crate::Result<()> {
        // Acquire exclusive lock, re-read, modify, write back
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to open lock file {}: {}",
                    self.lock_path.display(),
                    e
                ))
            })?;
        lock_file.lock_exclusive().map_err(|e| {
            crate::ThresholdError::Keychain(format!("Failed to acquire exclusive lock: {}", e))
        })?;

        // Read current contents (under lock)
        let mut secrets = if self.path.exists() {
            let contents = fs::read_to_string(&self.path).map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to read secrets file {}: {}",
                    self.path.display(),
                    e
                ))
            })?;
            if contents.trim().is_empty() {
                BTreeMap::new()
            } else {
                let sf: SecretsFile = toml::from_str(&contents).map_err(|e| {
                    crate::ThresholdError::Keychain(format!(
                        "Failed to parse secrets file {}: {}",
                        self.path.display(),
                        e
                    ))
                })?;
                sf.secrets
            }
        } else {
            BTreeMap::new()
        };

        secrets.insert(key.to_string(), value.to_string());

        // Write back atomically
        let secrets_file = SecretsFile { secrets };
        let toml_string = toml::to_string_pretty(&secrets_file).map_err(|e| {
            crate::ThresholdError::Keychain(format!("Failed to serialize secrets: {}", e))
        })?;

        let tmp_path = self.path.with_extension("toml.tmp");
        {
            let mut opts = OpenOptions::new();
            opts.create(true).write(true).truncate(true);

            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }

            let mut file = opts.open(&tmp_path).map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to write tmp secrets file {}: {}",
                    tmp_path.display(),
                    e
                ))
            })?;

            file.write_all(toml_string.as_bytes()).map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to write to tmp secrets file: {}",
                    e
                ))
            })?;

            file.flush().map_err(|e| {
                crate::ThresholdError::Keychain(format!("Failed to flush tmp secrets file: {}", e))
            })?;
        }

        fs::rename(&tmp_path, &self.path).map_err(|e| {
            crate::ThresholdError::Keychain(format!(
                "Failed to rename tmp secrets file to {}: {}",
                self.path.display(),
                e
            ))
        })?;

        // Lock released on drop
        drop(lock_file);

        Ok(())
    }

    fn delete(&self, key: &str) -> crate::Result<()> {
        // Acquire exclusive lock first to avoid TOCTOU race on existence check
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to open lock file {}: {}",
                    self.lock_path.display(),
                    e
                ))
            })?;
        lock_file.lock_exclusive().map_err(|e| {
            crate::ThresholdError::Keychain(format!("Failed to acquire exclusive lock: {}", e))
        })?;

        // Check existence under lock
        if !self.path.exists() {
            return Ok(());
        }

        let contents = fs::read_to_string(&self.path).map_err(|e| {
            crate::ThresholdError::Keychain(format!(
                "Failed to read secrets file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        if contents.trim().is_empty() {
            drop(lock_file);
            return Ok(());
        }

        let mut secrets_file: SecretsFile = toml::from_str(&contents).map_err(|e| {
            crate::ThresholdError::Keychain(format!(
                "Failed to parse secrets file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        secrets_file.secrets.remove(key);

        let toml_string = toml::to_string_pretty(&secrets_file).map_err(|e| {
            crate::ThresholdError::Keychain(format!("Failed to serialize secrets: {}", e))
        })?;

        let tmp_path = self.path.with_extension("toml.tmp");
        {
            let mut opts = OpenOptions::new();
            opts.create(true).write(true).truncate(true);

            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }

            let mut file = opts.open(&tmp_path).map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to write tmp secrets file {}: {}",
                    tmp_path.display(),
                    e
                ))
            })?;

            file.write_all(toml_string.as_bytes()).map_err(|e| {
                crate::ThresholdError::Keychain(format!(
                    "Failed to write to tmp secrets file: {}",
                    e
                ))
            })?;

            file.flush().map_err(|e| {
                crate::ThresholdError::Keychain(format!("Failed to flush tmp secrets file: {}", e))
            })?;
        }

        fs::rename(&tmp_path, &self.path).map_err(|e| {
            crate::ThresholdError::Keychain(format!(
                "Failed to rename tmp secrets file to {}: {}",
                self.path.display(),
                e
            ))
        })?;

        drop(lock_file);

        Ok(())
    }
}

/// Internal backend dispatch — not exposed publicly.
enum SecretBackendInner {
    File(FileStore),
    Keychain(String), // service_name
}

/// Secret store for accessing secrets with env var fallback.
///
/// Supports file-based storage (default) and OS keychain backends.
///
/// # Example
///
/// ```no_run
/// use threshold_core::secrets::SecretStore;
///
/// let store = SecretStore::new()?;
/// match store.resolve("discord-bot-token", "DISCORD_BOT_TOKEN")? {
///     Some(token) => println!("Found token"),
///     None => println!("No token configured"),
/// }
/// # Ok::<(), threshold_core::ThresholdError>(())
/// ```
pub struct SecretStore {
    inner: SecretBackendInner,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("backend", &self.backend_name())
            .finish()
    }
}

impl SecretStore {
    /// Create a new secret store with the file backend at the default path
    /// (`~/.threshold/secrets.toml`).
    ///
    /// # Errors
    ///
    /// Returns an error if the default data directory cannot be determined
    /// or the file store cannot be initialized.
    pub fn new() -> crate::Result<Self> {
        let data_dir = dirs::home_dir()
            .ok_or_else(|| {
                crate::ThresholdError::Config("Cannot determine home directory".to_string())
            })?
            .join(".threshold");
        Self::with_file_backend(data_dir.join("secrets.toml"))
    }

    /// Create a secret store with the file backend at a specific path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file store cannot be initialized (e.g., symlink,
    /// permission issues, parent directory creation failure).
    pub fn with_file_backend(path: PathBuf) -> crate::Result<Self> {
        let file_store = FileStore::new(path)?;
        Ok(Self {
            inner: SecretBackendInner::File(file_store),
        })
    }

    /// Create a secret store with the keychain backend using the given service name.
    ///
    /// This is infallible — no I/O is performed at construction time.
    pub fn with_keychain_backend(service_name: impl Into<String>) -> Self {
        Self {
            inner: SecretBackendInner::Keychain(service_name.into()),
        }
    }

    /// Create a secret store using the specified backend.
    ///
    /// - `File` backend: uses `<data_dir>/secrets.toml`. `data_dir` must be `Some`.
    /// - `Keychain` backend: uses default service name "threshold". `data_dir` is ignored.
    ///
    /// # Errors
    ///
    /// Returns an error if `backend` is `File` and `data_dir` is `None`, or if
    /// file store initialization fails.
    pub fn with_backend(backend: SecretBackend, data_dir: Option<PathBuf>) -> crate::Result<Self> {
        match backend {
            SecretBackend::File => {
                let dir = data_dir.ok_or_else(|| {
                    crate::ThresholdError::Config(
                        "data_dir required for file secret backend".to_string(),
                    )
                })?;
                Self::with_file_backend(dir.join("secrets.toml"))
            }
            SecretBackend::Keychain => Ok(Self::with_keychain_backend("threshold")),
        }
    }

    /// Create a secret store with a custom keychain service name.
    ///
    /// This is primarily for test isolation — tests should use unique service
    /// names to avoid polluting the user's keychain.
    pub fn with_service_name(service_name: impl Into<String>) -> Self {
        Self::with_keychain_backend(service_name)
    }

    /// Returns the name of the active backend ("file" or "keychain").
    pub fn backend_name(&self) -> &'static str {
        match &self.inner {
            SecretBackendInner::File(_) => "file",
            SecretBackendInner::Keychain(_) => "keychain",
        }
    }

    /// Store a secret in the backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend operation fails.
    pub fn set(&self, key: &str, value: &str) -> crate::Result<()> {
        match &self.inner {
            SecretBackendInner::File(store) => store.set(key, value),
            SecretBackendInner::Keychain(service_name) => {
                let entry = Entry::new(service_name, key)
                    .map_err(|e| crate::ThresholdError::Keychain(format!("create entry: {}", e)))?;
                entry
                    .set_password(value)
                    .map_err(|e| crate::ThresholdError::Keychain(format!("set password: {}", e)))?;
                Ok(())
            }
        }
    }

    /// Retrieve a secret from the backend.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(value))` — Secret found
    /// - `Ok(None)` — Secret not found (not an error)
    /// - `Err(...)` — Backend failure (fatal)
    pub fn get(&self, key: &str) -> crate::Result<Option<String>> {
        match &self.inner {
            SecretBackendInner::File(store) => store.get(key),
            SecretBackendInner::Keychain(service_name) => {
                let entry = Entry::new(service_name, key)
                    .map_err(|e| crate::ThresholdError::Keychain(format!("create entry: {}", e)))?;
                match entry.get_password() {
                    Ok(password) => Ok(Some(password)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(e) => Err(crate::ThresholdError::Keychain(format!(
                        "get password: {}",
                        e
                    ))),
                }
            }
        }
    }

    /// Delete a secret from the backend.
    ///
    /// Silently succeeds if the secret doesn't exist.
    pub fn delete(&self, key: &str) -> crate::Result<()> {
        match &self.inner {
            SecretBackendInner::File(store) => store.delete(key),
            SecretBackendInner::Keychain(service_name) => {
                let entry = Entry::new(service_name, key)
                    .map_err(|e| crate::ThresholdError::Keychain(format!("create entry: {}", e)))?;
                match entry.delete_credential() {
                    Ok(()) => Ok(()),
                    Err(keyring::Error::NoEntry) => Ok(()),
                    Err(e) => Err(crate::ThresholdError::Keychain(format!("delete: {}", e))),
                }
            }
        }
    }

    /// Resolve a secret: backend → env var → None.
    ///
    /// 1. Check the configured backend
    /// 2. If not found, check environment variable
    /// 3. Return None if neither exists
    pub fn resolve(&self, backend_key: &str, env_var: &str) -> crate::Result<Option<String>> {
        match self.get(backend_key)? {
            Some(value) => Ok(Some(value)),
            None => Ok(std::env::var(env_var).ok()),
        }
    }

    /// Check if the keychain backend is available with write permissions.
    ///
    /// Returns `false` for the file backend (keychain is not in use).
    /// For keychain backend, performs a test write/read/delete cycle.
    pub fn is_keychain_available(&self) -> bool {
        match &self.inner {
            SecretBackendInner::File(_) => false,
            SecretBackendInner::Keychain(_) => {
                let test_key = "_availability_check";
                let test_value = "test";
                match self.set(test_key, test_value) {
                    Ok(()) => {
                        let can_read = self.get(test_key).is_ok();
                        let _ = self.delete(test_key);
                        can_read
                    }
                    Err(_) => false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use uuid::Uuid;

    /// Create a test-specific secret store (keychain) to avoid polluting user keychain
    fn test_store() -> SecretStore {
        SecretStore::with_service_name(format!("threshold-test-{}", Uuid::new_v4()))
    }

    // ── Existing keychain tests (all require authorization) ──

    #[test]
    fn new_creates_with_file_backend() {
        // Verify that with_file_backend produces a file backend
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::with_file_backend(dir.path().join("secrets.toml")).unwrap();
        assert_eq!(store.backend_name(), "file");
    }

    #[test]
    fn with_service_name_creates_keychain_backend() {
        let store = SecretStore::with_service_name("custom");
        assert_eq!(store.backend_name(), "keychain");
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn keychain_set_and_get_roundtrip() {
        let store = test_store();
        let key = "test-key";
        let value = "test-value";

        store.set(key, value).unwrap();
        let retrieved = store.get(key).unwrap();

        assert_eq!(retrieved, Some(value.to_string()));

        // Cleanup
        store.delete(key).unwrap();
    }

    #[test]
    #[serial]
    fn keychain_get_nonexistent_returns_none() {
        let store = test_store();
        let result = store.get("nonexistent-key").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn keychain_delete_removes_secret() {
        let store = test_store();
        let key = "delete-test";

        store.set(key, "value").unwrap();
        assert!(store.get(key).unwrap().is_some());

        store.delete(key).unwrap();
        assert_eq!(store.get(key).unwrap(), None);
    }

    #[test]
    #[serial]
    fn keychain_delete_nonexistent_succeeds() {
        let store = test_store();
        store.delete("does-not-exist").unwrap();
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn keychain_resolve_finds_value() {
        let store = test_store();
        let key = "resolve-test";

        store.set(key, "keychain-value").unwrap();

        let result = store.resolve(key, "NONEXISTENT_ENV_VAR").unwrap();
        assert_eq!(result, Some("keychain-value".to_string()));

        store.delete(key).unwrap();
    }

    #[test]
    #[serial]
    fn resolve_falls_back_to_env_var() {
        let store = test_store();
        let env_key = format!("TEST_ENV_{}", Uuid::new_v4());

        // SAFETY: Test runs serially (#[serial]) so no data races
        unsafe {
            std::env::set_var(&env_key, "env-value");
        }

        let result = store.resolve("nonexistent-keychain-key", &env_key).unwrap();
        assert_eq!(result, Some("env-value".to_string()));

        unsafe {
            std::env::remove_var(&env_key);
        }
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn keychain_resolve_prefers_backend_over_env() {
        let store = test_store();
        let key = "priority-test";
        let env_key = format!("TEST_ENV_{}", Uuid::new_v4());

        store.set(key, "keychain-value").unwrap();
        unsafe {
            std::env::set_var(&env_key, "env-value");
        }

        let result = store.resolve(key, &env_key).unwrap();
        assert_eq!(result, Some("keychain-value".to_string()));

        store.delete(key).unwrap();
        unsafe {
            std::env::remove_var(&env_key);
        }
    }

    #[test]
    #[serial]
    fn resolve_returns_none_when_neither_exists() {
        let store = test_store();
        let result = store
            .resolve("nonexistent-key", "NONEXISTENT_ENV_VAR")
            .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn keychain_resolve_propagates_backend_errors() {
        let store = test_store();
        let key = "error-test";

        match store.get(key) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("unexpected value in keychain"),
            Err(e) => {
                let env_key = format!("TEST_ENV_{}", Uuid::new_v4());
                unsafe {
                    std::env::set_var(&env_key, "should-not-be-returned");
                }

                let result = store.resolve(key, &env_key);

                unsafe {
                    std::env::remove_var(&env_key);
                }

                assert!(
                    result.is_err(),
                    "resolve() should propagate backend errors, not fall back to env"
                );
                assert!(
                    result.unwrap_err().to_string().contains("Secret store"),
                    "error should be a secret store error: {:?}",
                    e
                );
            }
        }
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn keychain_set_overwrites_existing_value() {
        let store = test_store();
        let key = "overwrite-test";

        store.set(key, "first-value").unwrap();
        store.set(key, "second-value").unwrap();

        let result = store.get(key).unwrap();
        assert_eq!(result, Some("second-value".to_string()));

        store.delete(key).unwrap();
    }

    #[test]
    fn is_keychain_available_returns_bool() {
        let store = test_store();
        let _available = store.is_keychain_available();
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn keychain_different_service_names_isolate_secrets() {
        let store1 = SecretStore::with_service_name(format!("test-svc1-{}", Uuid::new_v4()));
        let store2 = SecretStore::with_service_name(format!("test-svc2-{}", Uuid::new_v4()));
        let key = "shared-key";

        store1.set(key, "value1").unwrap();
        store2.set(key, "value2").unwrap();

        assert_eq!(store1.get(key).unwrap(), Some("value1".to_string()));
        assert_eq!(store2.get(key).unwrap(), Some("value2".to_string()));

        store1.delete(key).unwrap();
        store2.delete(key).unwrap();
    }

    // ── File backend tests (no keychain, no #[ignore]) ──

    mod file_backend {
        use super::*;

        fn temp_store() -> (tempfile::TempDir, SecretStore) {
            let dir = tempfile::tempdir().unwrap();
            let store = SecretStore::with_file_backend(dir.path().join("secrets.toml")).unwrap();
            (dir, store)
        }

        #[test]
        fn set_and_get_roundtrip() {
            let (_dir, store) = temp_store();
            store.set("my-key", "my-value").unwrap();
            assert_eq!(store.get("my-key").unwrap(), Some("my-value".to_string()));
        }

        #[test]
        fn get_nonexistent_returns_none() {
            let (_dir, store) = temp_store();
            assert_eq!(store.get("nonexistent").unwrap(), None);
        }

        #[test]
        fn delete_removes_secret() {
            let (_dir, store) = temp_store();
            store.set("key", "value").unwrap();
            assert!(store.get("key").unwrap().is_some());

            store.delete("key").unwrap();
            assert_eq!(store.get("key").unwrap(), None);
        }

        #[test]
        fn delete_nonexistent_succeeds() {
            let (_dir, store) = temp_store();
            store.delete("does-not-exist").unwrap();
        }

        #[test]
        fn overwrite_updates_value() {
            let (_dir, store) = temp_store();
            store.set("key", "first").unwrap();
            store.set("key", "second").unwrap();
            assert_eq!(store.get("key").unwrap(), Some("second".to_string()));
        }

        #[test]
        fn persists_across_instances() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.toml");

            // First instance writes
            {
                let store = SecretStore::with_file_backend(path.clone()).unwrap();
                store.set("persistent-key", "persistent-value").unwrap();
            }

            // Second instance reads
            {
                let store = SecretStore::with_file_backend(path).unwrap();
                assert_eq!(
                    store.get("persistent-key").unwrap(),
                    Some("persistent-value".to_string())
                );
            }
        }

        #[test]
        #[serial]
        fn resolve_with_env_fallback() {
            let (_dir, store) = temp_store();
            let env_key = format!("TEST_FILE_ENV_{}", Uuid::new_v4());

            // Secret not in file, env var set → should fall back
            unsafe {
                std::env::set_var(&env_key, "env-value");
            }

            let result = store.resolve("nonexistent", &env_key).unwrap();
            assert_eq!(result, Some("env-value".to_string()));

            unsafe {
                std::env::remove_var(&env_key);
            }
        }

        #[test]
        fn backend_name_is_file() {
            let (_dir, store) = temp_store();
            assert_eq!(store.backend_name(), "file");
        }

        #[test]
        fn is_keychain_available_returns_false() {
            let (_dir, store) = temp_store();
            assert!(!store.is_keychain_available());
        }

        #[test]
        #[cfg(unix)]
        fn file_has_600_permissions() {
            use std::os::unix::fs::MetadataExt as _;

            let (_dir, store) = temp_store();
            store.set("key", "value").unwrap();

            let path = match &store.inner {
                SecretBackendInner::File(fs) => &fs.path,
                _ => panic!("expected file backend"),
            };

            let mode = std::fs::metadata(path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600, "secrets file should have 0600 permissions");
        }

        #[test]
        fn empty_file_loads_ok() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.toml");

            // Create empty file
            std::fs::write(&path, "").unwrap();

            let store = SecretStore::with_file_backend(path).unwrap();
            assert_eq!(store.get("anything").unwrap(), None);
        }

        #[test]
        fn toml_format_is_correct() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.toml");

            let store = SecretStore::with_file_backend(path.clone()).unwrap();
            store.set("beta-key", "beta-value").unwrap();
            store.set("alpha-key", "alpha-value").unwrap();

            let contents = std::fs::read_to_string(&path).unwrap();

            // BTreeMap ensures alphabetical ordering
            assert!(contents.contains("[secrets]"));
            let alpha_pos = contents.find("alpha-key").unwrap();
            let beta_pos = contents.find("beta-key").unwrap();
            assert!(
                alpha_pos < beta_pos,
                "keys should be in alphabetical order (BTreeMap)"
            );
        }

        #[test]
        fn special_chars_in_values() {
            let (_dir, store) = temp_store();

            // Values with special characters
            store.set("key1", "value@with.special").unwrap();
            assert_eq!(
                store.get("key1").unwrap(),
                Some("value@with.special".to_string())
            );

            store.set("key2", "value with \"quotes\"").unwrap();
            assert_eq!(
                store.get("key2").unwrap(),
                Some("value with \"quotes\"".to_string())
            );

            store.set("key3", "value\nwith\nnewlines").unwrap();
            assert_eq!(
                store.get("key3").unwrap(),
                Some("value\nwith\nnewlines".to_string())
            );
        }

        #[test]
        fn special_chars_in_keys() {
            let (_dir, store) = temp_store();

            // Keys with special characters (like gmail oauth tokens)
            store
                .set("gmail-oauth-refresh-token-alice@gmail.com", "token123")
                .unwrap();
            assert_eq!(
                store
                    .get("gmail-oauth-refresh-token-alice@gmail.com")
                    .unwrap(),
                Some("token123".to_string())
            );

            store.set("key.with.dots", "dotted").unwrap();
            assert_eq!(
                store.get("key.with.dots").unwrap(),
                Some("dotted".to_string())
            );
        }

        #[test]
        fn flush_failure_leaves_file_unchanged() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.toml");

            // Set an initial value
            let store = SecretStore::with_file_backend(path.clone()).unwrap();
            store.set("key", "original").unwrap();
            drop(store);

            // Create a read-only directory to prevent tmp file creation
            let ro_dir = dir.path().join("readonly");
            std::fs::create_dir(&ro_dir).unwrap();
            let ro_path = ro_dir.join("secrets.toml");
            std::fs::copy(&path, &ro_path).unwrap();

            // Make directory read-only
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&ro_dir, PermissionsExt::from_mode(0o555)).unwrap();
            }

            let store = SecretStore::with_file_backend(ro_path.clone()).unwrap();

            // This should fail because we can't write the tmp file
            let result = store.set("key", "modified");

            #[cfg(unix)]
            {
                assert!(result.is_err(), "set should fail in read-only directory");
                // Original value should be preserved
                let contents = std::fs::read_to_string(&ro_path).unwrap();
                assert!(contents.contains("original"));
            }

            // Restore permissions for cleanup
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&ro_dir, PermissionsExt::from_mode(0o755)).unwrap();
            }

            // Suppress unused variable warning on non-unix
            let _ = result;
        }

        #[test]
        fn rejects_symlink_path() {
            let dir = tempfile::tempdir().unwrap();
            let real_path = dir.path().join("real-secrets.toml");
            let link_path = dir.path().join("secrets.toml");

            // Create a real file and a symlink to it
            std::fs::write(&real_path, "").unwrap();

            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real_path, &link_path).unwrap();
                let result = SecretStore::with_file_backend(link_path);
                assert!(result.is_err());
                assert!(
                    result.unwrap_err().to_string().contains("symlink"),
                    "error should mention symlink"
                );
            }

            // On non-unix, symlinks may not be available — test is a no-op
            #[cfg(not(unix))]
            let _ = (real_path, link_path);
        }

        #[test]
        fn concurrent_instances_dont_corrupt() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.toml");

            let handles: Vec<_> = (0..10)
                .map(|i| {
                    let p = path.clone();
                    std::thread::spawn(move || {
                        let store = SecretStore::with_file_backend(p).unwrap();
                        store
                            .set(&format!("key-{}", i), &format!("value-{}", i))
                            .unwrap();
                    })
                })
                .collect();

            for h in handles {
                h.join().unwrap();
            }

            // Verify all values are present (no corruption)
            let store = SecretStore::with_file_backend(path).unwrap();
            for i in 0..10 {
                assert_eq!(
                    store.get(&format!("key-{}", i)).unwrap(),
                    Some(format!("value-{}", i)),
                    "key-{} should have been written",
                    i
                );
            }
        }

        #[test]
        #[cfg(unix)]
        fn auto_chmod_fixes_permissions() {
            use std::os::unix::fs::MetadataExt as _;
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.toml");

            // Create file with wrong permissions
            std::fs::write(&path, "[secrets]\n").unwrap();
            std::fs::set_permissions(&path, PermissionsExt::from_mode(0o644)).unwrap();

            // Opening FileStore should fix permissions
            let _store = SecretStore::with_file_backend(path.clone()).unwrap();

            let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600, "permissions should be auto-fixed to 0600");
        }

        #[test]
        fn with_backend_file_requires_data_dir() {
            let result = SecretStore::with_backend(SecretBackend::File, None);
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("data_dir required")
            );
        }

        #[test]
        fn with_backend_keychain_ignores_data_dir() {
            let store = SecretStore::with_backend(SecretBackend::Keychain, None).unwrap();
            assert_eq!(store.backend_name(), "keychain");
        }

        #[test]
        fn with_backend_file_with_data_dir() {
            let dir = tempfile::tempdir().unwrap();
            let store =
                SecretStore::with_backend(SecretBackend::File, Some(dir.path().to_path_buf()))
                    .unwrap();
            assert_eq!(store.backend_name(), "file");
        }

        #[test]
        fn secret_backend_default_is_file() {
            assert_eq!(SecretBackend::default(), SecretBackend::File);
        }
    }
}
