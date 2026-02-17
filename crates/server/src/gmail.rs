//! Gmail subcommand handler for the server binary.
//!
//! Thin wrapper that loads config and delegates to `threshold_gmail`.

use threshold_core::config::ThresholdConfig;

/// Handle the `threshold gmail` command.
pub async fn handle_gmail_command(args: threshold_gmail::GmailArgs) -> anyhow::Result<()> {
    let config = ThresholdConfig::load()?;

    let gmail_config = config.tools.gmail.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "Gmail is not configured. Add [tools.gmail] section to your config file."
        )
    })?;

    let audit_path = config
        .data_dir()
        .ok()
        .map(|d| d.join("audit").join("gmail.jsonl"));

    threshold_gmail::handle_gmail_command(
        args,
        gmail_config,
        audit_path.as_deref(),
    )
    .await
}
