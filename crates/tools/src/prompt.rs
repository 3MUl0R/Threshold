//! System prompt assembly for Threshold CLI tools.
//!
//! Builds the tool-instruction section of the system prompt based on which
//! integrations are enabled in the configuration. The result is appended to
//! the agent's base system prompt before launching a Claude conversation.

use threshold_core::config::ThresholdConfig;

/// Build the tool-instruction portion of the system prompt.
///
/// Returns a string describing which `threshold` CLI subcommands are available
/// based on enabled integrations. Called once at engine setup time.
pub fn build_tool_prompt(config: &ThresholdConfig) -> String {
    let mut sections = Vec::new();

    sections.push(
        "## Additional Tools\n\n\
         You have access to the `threshold` CLI for capabilities beyond your \
         native tools. Run `threshold --help` for a full list. Use your shell \
         execution capability to invoke these commands."
            .to_string(),
    );

    if config.scheduler.as_ref().map_or(false, |s| s.enabled) {
        sections.push(
            "### Schedule Management\n\
             Create, list, and manage recurring tasks.\n\
             Run `threshold schedule --help` for full usage.\n\
             Example: `threshold schedule script --name \"nightly tests\" \
             --cron \"0 0 3 * * *\" --command \"cargo test\"`"
                .to_string(),
        );
    }

    if config.tools.gmail.as_ref().map_or(false, |g| g.enabled) {
        sections.push(
            "### Gmail\n\
             Read and send email.\n\
             Run `threshold gmail --help` for full usage."
                .to_string(),
        );
    }

    if config.tools.browser.as_ref().map_or(false, |b| b.enabled) {
        sections.push(
            "### Browser Automation\n\
             Control a web browser via Playwright.\n\
             Run `threshold browser --help` for full usage."
                .to_string(),
        );
    }

    if config.tools.image_gen.as_ref().map_or(false, |i| i.enabled) {
        sections.push(
            "### Image Generation\n\
             Generate images from text descriptions.\n\
             Run `threshold imagegen --help` for full usage."
                .to_string(),
        );
    }

    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use threshold_core::config::*;

    fn minimal_config() -> ThresholdConfig {
        ThresholdConfig {
            data_dir: None,
            log_level: None,
            secret_backend: None,
            cli: CliConfig {
                claude: ClaudeCliConfig {
                    command: None,
                    model: None,
                    timeout_seconds: None,
                    skip_permissions: None,
                    ack_enabled: None,
                    status_interval_seconds: None,
                    extra_flags: vec![],
                },
            },
            discord: None,
            agents: vec![],
            tools: ToolsConfig::default(),
            heartbeat: None,
            scheduler: None,
            web: None,
        }
    }

    #[test]
    fn empty_config_produces_header_only() {
        let prompt = build_tool_prompt(&minimal_config());
        assert!(prompt.contains("## Additional Tools"));
        assert!(prompt.contains("threshold"));
        assert!(!prompt.contains("### Schedule"));
        assert!(!prompt.contains("### Gmail"));
        assert!(!prompt.contains("### Browser"));
        assert!(!prompt.contains("### Image"));
    }

    #[test]
    fn enabled_tools_appear_in_prompt() {
        let mut config = minimal_config();
        config.scheduler = Some(SchedulerConfig {
            enabled: true,
            store_path: None,
        });
        config.tools.gmail = Some(GmailToolConfig {
            enabled: true,
            inboxes: None,
            allow_send: None,
        });
        config.tools.browser = Some(BrowserToolConfig {
            enabled: true,
            headless: None,
            allowed_origins: None,
            blocked_origins: None,
        });
        config.tools.image_gen = Some(ImageGenToolConfig { enabled: true });

        let prompt = build_tool_prompt(&config);
        assert!(prompt.contains("### Schedule Management"));
        assert!(prompt.contains("### Gmail"));
        assert!(prompt.contains("### Browser Automation"));
        assert!(prompt.contains("### Image Generation"));
    }

    #[test]
    fn disabled_tools_excluded_from_prompt() {
        let mut config = minimal_config();
        config.scheduler = Some(SchedulerConfig {
            enabled: false,
            store_path: None,
        });
        config.tools.gmail = Some(GmailToolConfig {
            enabled: false,
            inboxes: None,
            allow_send: None,
        });

        let prompt = build_tool_prompt(&config);
        assert!(prompt.contains("## Additional Tools"));
        assert!(!prompt.contains("### Schedule"));
        assert!(!prompt.contains("### Gmail"));
    }
}
