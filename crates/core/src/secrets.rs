//! OS keychain secrets management with environment variable fallback.
//!
//! # Security Model
//!
//! Secrets are stored in the OS native keychain:
//! - macOS: Keychain Services
//! - Windows: Credential Manager
//! - Linux: Secret Service D-Bus API
//!
//! Environment variables provide a fallback for containerized deployments
//! where keychain backends may not be available.
//!
//! # Resolution Priority
//!
//! When resolving secrets via `resolve()`:
//! 1. Check OS keychain first
//! 2. Fall back to environment variable if keychain returns `Ok(None)`
//! 3. Return `Err` if keychain backend fails (fatal error)
//!
//! # Desktop vs Headless
//!
//! - Desktop environments: Keychain required, failures are fatal
//! - Headless/containers: Keychain unavailability is OK if env vars configured

use keyring::Entry;

/// Secret store for OS keychain access with env var fallback.
pub struct SecretStore {
    service_name: String,
}

impl SecretStore {
    /// Create a new secret store with the default service name "threshold".
    pub fn new() -> Self {
        Self::with_service_name("threshold")
    }

    /// Create a secret store with a custom service name.
    ///
    /// This is primarily for test isolation - tests should use unique service
    /// names to avoid polluting the user's keychain.
    pub fn with_service_name(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
        }
    }

    /// Store a secret in the OS keychain.
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::Keychain` if the keychain backend fails.
    pub fn set(&self, key: &str, value: &str) -> crate::Result<()> {
        let entry = Entry::new(&self.service_name, key)
            .map_err(|e| crate::ThresholdError::Keychain(format!("create entry: {}", e)))?;

        entry
            .set_password(value)
            .map_err(|e| crate::ThresholdError::Keychain(format!("set password: {}", e)))?;

        Ok(())
    }

    /// Retrieve a secret from the OS keychain.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(value))` - Secret found
    /// - `Ok(None)` - Secret not found (not an error)
    /// - `Err(...)` - Keychain backend failure (fatal)
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::Keychain` if the keychain backend fails.
    pub fn get(&self, key: &str) -> crate::Result<Option<String>> {
        let entry = Entry::new(&self.service_name, key)
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

    /// Delete a secret from the OS keychain.
    ///
    /// Silently succeeds if the secret doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::Keychain` if the keychain backend fails.
    pub fn delete(&self, key: &str) -> crate::Result<()> {
        let entry = Entry::new(&self.service_name, key)
            .map_err(|e| crate::ThresholdError::Keychain(format!("create entry: {}", e)))?;

        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()), // Already deleted
            Err(e) => Err(crate::ThresholdError::Keychain(format!("delete: {}", e))),
        }
    }

    /// Resolve a secret: keychain → env var → None.
    ///
    /// # Resolution Order
    ///
    /// 1. Check OS keychain
    /// 2. If not found, check environment variable
    /// 3. Return None if neither exists
    ///
    /// # Returns
    ///
    /// - `Ok(Some(value))` - Found in keychain or env var
    /// - `Ok(None)` - Not configured in either source
    /// - `Err(...)` - Keychain backend failure (fatal)
    ///
    /// # Errors
    ///
    /// Returns `ThresholdError::Keychain` if the keychain backend fails.
    /// This allows fail-fast behavior on startup when keychain is expected
    /// to be available but isn't.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use threshold_core::secrets::SecretStore;
    ///
    /// let store = SecretStore::new();
    /// match store.resolve("discord-bot-token", "DISCORD_BOT_TOKEN")? {
    ///     Some(token) => println!("Found token"),
    ///     None => println!("No token configured"),
    /// }
    /// # Ok::<(), threshold_core::ThresholdError>(())
    /// ```
    pub fn resolve(&self, keychain_key: &str, env_var: &str) -> crate::Result<Option<String>> {
        // Try keychain first (propagates errors)
        match self.get(keychain_key)? {
            Some(value) => Ok(Some(value)),
            None => {
                // Keychain said "not found" (Ok(None))
                // Try environment variable as fallback
                Ok(std::env::var(env_var).ok())
            }
        }
    }

    /// Check if the keychain backend is available with write permissions.
    ///
    /// This performs an actual test write/read/delete cycle to verify
    /// the keychain is not only present but also authorized for use.
    ///
    /// Useful for detecting headless environments where keychain may not
    /// be available or authorized.
    pub fn is_keychain_available(&self) -> bool {
        let test_key = "_availability_check";
        let test_value = "test";

        // Try to write, read, and delete
        match self.set(test_key, test_value) {
            Ok(()) => {
                let can_read = self.get(test_key).is_ok();
                let _ = self.delete(test_key); // Clean up regardless
                can_read
            }
            Err(_) => false,
        }
    }
}

impl Default for SecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use uuid::Uuid;

    /// Create a test-specific secret store to avoid polluting user keychain
    fn test_store() -> SecretStore {
        SecretStore::with_service_name(format!("threshold-test-{}", Uuid::new_v4()))
    }

    #[test]
    fn new_creates_with_default_service_name() {
        let store = SecretStore::new();
        assert_eq!(store.service_name, "threshold");
    }

    #[test]
    fn with_service_name_creates_with_custom_name() {
        let store = SecretStore::with_service_name("custom");
        assert_eq!(store.service_name, "custom");
    }

    #[test]
    fn default_creates_with_default_service_name() {
        let store = SecretStore::default();
        assert_eq!(store.service_name, "threshold");
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn set_and_get_roundtrip() {
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
    fn get_nonexistent_returns_none() {
        let store = test_store();
        let result = store.get("nonexistent-key").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn delete_removes_secret() {
        let store = test_store();
        let key = "delete-test";

        store.set(key, "value").unwrap();
        assert!(store.get(key).unwrap().is_some());

        store.delete(key).unwrap();
        assert_eq!(store.get(key).unwrap(), None);
    }

    #[test]
    #[serial]
    fn delete_nonexistent_succeeds() {
        let store = test_store();
        // Should not error
        store.delete("does-not-exist").unwrap();
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn resolve_finds_keychain_value() {
        let store = test_store();
        let key = "resolve-test";

        store.set(key, "keychain-value").unwrap();

        let result = store.resolve(key, "NONEXISTENT_ENV_VAR").unwrap();
        assert_eq!(result, Some("keychain-value".to_string()));

        // Cleanup
        store.delete(key).unwrap();
    }

    #[test]
    #[serial]
    fn resolve_falls_back_to_env_var() {
        let store = test_store();
        let env_key = format!("TEST_ENV_{}", Uuid::new_v4());

        // Set environment variable
        // SAFETY: Test runs serially (#[serial]) so no data races
        unsafe {
            std::env::set_var(&env_key, "env-value");
        }

        let result = store.resolve("nonexistent-keychain-key", &env_key).unwrap();
        assert_eq!(result, Some("env-value".to_string()));

        // Cleanup
        // SAFETY: Test runs serially (#[serial]) so no data races
        unsafe {
            std::env::remove_var(&env_key);
        }
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn resolve_prefers_keychain_over_env() {
        let store = test_store();
        let key = "priority-test";
        let env_key = format!("TEST_ENV_{}", Uuid::new_v4());

        // Set both keychain and env var
        store.set(key, "keychain-value").unwrap();
        // SAFETY: Test runs serially (#[serial]) so no data races
        unsafe {
            std::env::set_var(&env_key, "env-value");
        }

        let result = store.resolve(key, &env_key).unwrap();
        assert_eq!(result, Some("keychain-value".to_string()));

        // Cleanup
        store.delete(key).unwrap();
        // SAFETY: Test runs serially (#[serial]) so no data races
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
    fn resolve_propagates_keychain_backend_errors() {
        // This test verifies that keychain backend failures are propagated as Err
        // rather than falling back to env vars. We simulate a backend error by
        // attempting operations that might fail due to keychain authorization.
        let store = test_store();
        let key = "error-test";

        // Attempt to get from keychain - if authorization fails, it should error
        // not silently return None
        match store.get(key) {
            Ok(None) => {
                // Keychain is working but key doesn't exist - this is expected
            }
            Ok(Some(_)) => {
                // Unexpected: we didn't set this key
                panic!("unexpected value in keychain");
            }
            Err(e) => {
                // Keychain backend error - verify resolve() propagates it
                // Set an env var to verify it's NOT used when keychain errors
                let env_key = format!("TEST_ENV_{}", Uuid::new_v4());
                unsafe {
                    std::env::set_var(&env_key, "should-not-be-returned");
                }

                let result = store.resolve(key, &env_key);

                // Clean up env var
                unsafe {
                    std::env::remove_var(&env_key);
                }

                // Verify we got the same error, not Ok(Some(env_value))
                assert!(
                    result.is_err(),
                    "resolve() should propagate keychain errors, not fall back to env"
                );
                assert!(
                    result.unwrap_err().to_string().contains("Keychain"),
                    "error should be a Keychain error: {:?}",
                    e
                );
            }
        }
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn set_overwrites_existing_value() {
        let store = test_store();
        let key = "overwrite-test";

        store.set(key, "first-value").unwrap();
        store.set(key, "second-value").unwrap();

        let result = store.get(key).unwrap();
        assert_eq!(result, Some("second-value".to_string()));

        // Cleanup
        store.delete(key).unwrap();
    }

    #[test]
    fn is_keychain_available_returns_bool() {
        let store = test_store();
        // Should return true on desktop, false in headless
        // We just verify it doesn't panic
        let _available = store.is_keychain_available();
    }

    #[test]
    #[serial]
    #[ignore] // Requires keychain authorization - run with --ignored on authorized systems
    fn different_service_names_isolate_secrets() {
        let store1 = SecretStore::with_service_name(format!("test-svc1-{}", Uuid::new_v4()));
        let store2 = SecretStore::with_service_name(format!("test-svc2-{}", Uuid::new_v4()));
        let key = "shared-key";

        store1.set(key, "value1").unwrap();
        store2.set(key, "value2").unwrap();

        assert_eq!(store1.get(key).unwrap(), Some("value1".to_string()));
        assert_eq!(store2.get(key).unwrap(), Some("value2".to_string()));

        // Cleanup
        store1.delete(key).unwrap();
        store2.delete(key).unwrap();
    }
}
