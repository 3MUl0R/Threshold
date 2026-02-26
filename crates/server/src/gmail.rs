//! Gmail subcommand handler for the server binary.
//!
//! Thin wrapper that loads config and delegates to `threshold_gmail`.

use std::sync::Arc;

use threshold_core::config::ThresholdConfig;
use threshold_core::{SecretBackend, SecretStore};

/// Handle the `threshold gmail` command.
pub async fn handle_gmail_command(args: threshold_gmail::GmailArgs) -> anyhow::Result<()> {
    let config = ThresholdConfig::load()?;

    let gmail_config = config.tools.gmail.as_ref().ok_or_else(|| {
        anyhow::anyhow!("Gmail is not configured. Add [tools.gmail] section to your config file.")
    })?;

    let audit_path = match config.data_dir() {
        Ok(d) => Some(d.join("audit").join("gmail.jsonl")),
        Err(e) => {
            tracing::warn!("Could not resolve data_dir for audit logging: {}", e);
            None
        }
    };

    let backend = config.secret_backend();
    let data_dir = if backend == SecretBackend::File {
        Some(config.data_dir()?)
    } else {
        None
    };
    let secrets = Arc::new(SecretStore::with_backend(backend, data_dir)?);

    threshold_gmail::handle_gmail_command(args, gmail_config, audit_path.as_deref(), secrets).await
}
