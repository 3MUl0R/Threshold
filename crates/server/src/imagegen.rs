//! Image generation subcommand handler for the server binary.
//!
//! Thin wrapper that loads config and delegates to `threshold_imagegen`.

use threshold_core::config::ThresholdConfig;

/// Handle the `threshold imagegen` command.
pub async fn handle_imagegen_command(
    args: threshold_imagegen::ImagegenArgs,
) -> anyhow::Result<()> {
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

    threshold_imagegen::handle_imagegen_command(args, imagegen_config, audit_path.as_deref()).await
}
