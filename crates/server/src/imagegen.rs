//! Image generation subcommand handler for the server binary.
//!
//! Thin wrapper that loads config and delegates to `threshold_imagegen`.

use std::sync::Arc;

use threshold_core::config::ThresholdConfig;
use threshold_core::{SecretBackend, SecretStore};

/// Handle the `threshold imagegen` command.
pub async fn handle_imagegen_command(args: threshold_imagegen::ImagegenArgs) -> anyhow::Result<()> {
    let config = ThresholdConfig::load()?;

    let imagegen_config = config.tools.image_gen.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "Image generation is not configured. Add [tools.image_gen] section to your config file."
        )
    })?;

    let audit_path = match config.data_dir() {
        Ok(d) => Some(d.join("audit").join("imagegen.jsonl")),
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

    threshold_imagegen::handle_imagegen_command(
        args,
        imagegen_config,
        audit_path.as_deref(),
        secrets,
    )
    .await
}
