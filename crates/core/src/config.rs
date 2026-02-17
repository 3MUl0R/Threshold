use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct ThresholdConfig {
    pub data_dir: Option<PathBuf>,
    pub log_level: Option<String>,

    pub cli: CliConfig,
    pub discord: Option<DiscordConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfigToml>,
    #[serde(default)]
    pub tools: ToolsConfig,
    pub heartbeat: Option<HeartbeatConfig>,
    pub scheduler: Option<SchedulerConfig>,
}

// ── CLI ──

#[derive(Debug, Deserialize)]
pub struct CliConfig {
    pub claude: ClaudeCliConfig,
    // Future: pub codex: Option<CodexCliConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeCliConfig {
    pub command: Option<String>,
    pub model: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub skip_permissions: Option<bool>,
    #[serde(default)]
    pub extra_flags: Vec<String>,
}

// ── Discord ──

#[derive(Debug, Deserialize, Clone)]
pub struct DiscordConfig {
    pub guild_id: u64,
    pub allowed_user_ids: Vec<u64>,
    // bot_token resolved from keychain, NEVER stored here
}

// ── Agents ──

#[derive(Debug, Deserialize)]
pub struct AgentConfigToml {
    pub id: String,
    pub name: String,
    pub cli_provider: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub system_prompt_file: Option<String>,
    pub tools: Option<String>,
}

// ── Tools ──

#[derive(Debug, Default, Deserialize)]
pub struct ToolsConfig {
    pub permission_mode: Option<String>,
    pub browser: Option<BrowserToolConfig>,
    pub gmail: Option<GmailToolConfig>,
    pub image_gen: Option<ImageGenToolConfig>,
}

#[derive(Debug, Deserialize)]
pub struct BrowserToolConfig {
    pub enabled: bool,
    pub headless: Option<bool>,
    pub allowed_origins: Option<Vec<String>>,
    pub blocked_origins: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct GmailToolConfig {
    pub enabled: bool,
    pub inboxes: Option<Vec<String>>,
    pub allow_send: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ImageGenToolConfig {
    pub enabled: bool,
}

// ── Heartbeat ──

#[derive(Debug, Deserialize, Clone)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    pub interval_minutes: Option<u64>,
    pub instruction_file: Option<String>,
    pub handoff_notes_path: Option<String>,
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub skip_if_running: Option<bool>,
    pub notification_channel_id: Option<u64>,
}

// ── Scheduler ──

#[derive(Debug, Deserialize, Clone)]
pub struct SchedulerConfig {
    pub enabled: bool,
    pub store_path: Option<String>,
}

// ── Loading ──

impl ThresholdConfig {
    /// Load config from the default path or `THRESHOLD_CONFIG` env var.
    pub fn load() -> crate::Result<Self> {
        let path = std::env::var("THRESHOLD_CONFIG")
            .map(PathBuf::from)
            .ok()
            .map_or_else(Self::default_config_path, Ok)?;

        if !path.exists() {
            return Err(crate::ThresholdError::ConfigNotFound { path });
        }

        let content = std::fs::read_to_string(&path)?;
        let config: Self = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Default config path: `$HOME/.threshold/config.toml`
    pub fn default_config_path() -> crate::Result<PathBuf> {
        Ok(Self::default_root()?.join("config.toml"))
    }

    /// Resolved data directory — same as root unless overridden.
    pub fn data_dir(&self) -> crate::Result<PathBuf> {
        match &self.data_dir {
            Some(dir) => Ok(dir.clone()),
            None => Self::default_root(),
        }
    }

    /// The root directory: `$HOME/.threshold`
    fn default_root() -> crate::Result<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join(".threshold"))
            .ok_or_else(|| {
                crate::ThresholdError::Config("could not determine home directory".to_string())
            })
    }

    /// Validate required fields and enum values.
    fn validate(&self) -> crate::Result<()> {
        if let Some(level) = &self.log_level {
            match level.as_str() {
                "trace" | "debug" | "info" | "warn" | "error" => {}
                _ => {
                    return Err(crate::ThresholdError::Config(format!(
                        "invalid log_level '{level}': expected trace, debug, info, warn, or error"
                    )));
                }
            }
        }

        if let Some(mode) = &self.tools.permission_mode {
            match mode.as_str() {
                "full-auto" | "approve-destructive" | "approve-all" => {}
                _ => {
                    return Err(crate::ThresholdError::Config(format!(
                        "invalid tools.permission_mode '{mode}': expected full-auto, approve-destructive, or approve-all"
                    )));
                }
            }
        }

        for agent in &self.agents {
            match agent.cli_provider.as_str() {
                "claude" => {}
                _ => {
                    return Err(crate::ThresholdError::Config(format!(
                        "unsupported cli_provider '{}' for agent '{}': expected 'claude'",
                        agent.cli_provider, agent.id
                    )));
                }
            }

            if let Some(tools) = &agent.tools {
                match tools.as_str() {
                    "minimal" | "standard" | "coding" | "full" => {}
                    _ => {
                        return Err(crate::ThresholdError::Config(format!(
                            "invalid tools '{}' for agent '{}': expected minimal, standard, or full",
                            tools, agent.id
                        )));
                    }
                }
            }
        }

        if let Some(discord) = &self.discord
            && discord.allowed_user_ids.is_empty()
        {
            return Err(crate::ThresholdError::Config(
                "discord.allowed_user_ids must not be empty".to_string(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_toml_string() {
        let toml = r#"
log_level = "info"

[cli.claude]
model = "sonnet"
timeout_seconds = 300
skip_permissions = true

[discord]
guild_id = 123456789012345678
allowed_user_ids = [987654321098765432]

[tools]
permission_mode = "full-auto"

[tools.browser]
enabled = false
headless = true

[tools.gmail]
enabled = false

[tools.image_gen]
enabled = false

[heartbeat]
enabled = true
interval_minutes = 30

[scheduler]
enabled = true

[[agents]]
id = "default"
name = "Assistant"
cli_provider = "claude"
tools = "full"

[[agents]]
id = "coder"
name = "Code Assistant"
cli_provider = "claude"
model = "opus"
system_prompt = "You are a coding assistant."
tools = "coding"
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();

        assert_eq!(config.log_level.as_deref(), Some("info"));
        assert_eq!(config.cli.claude.model.as_deref(), Some("sonnet"));
        assert_eq!(config.cli.claude.timeout_seconds, Some(300));
        assert_eq!(config.cli.claude.skip_permissions, Some(true));

        let discord = config.discord.as_ref().unwrap();
        assert_eq!(discord.guild_id, 123456789012345678);
        assert_eq!(discord.allowed_user_ids, vec![987654321098765432]);

        assert_eq!(config.tools.permission_mode.as_deref(), Some("full-auto"));
        assert!(!config.tools.browser.as_ref().unwrap().enabled);

        let heartbeat = config.heartbeat.as_ref().unwrap();
        assert!(heartbeat.enabled);
        assert_eq!(heartbeat.interval_minutes, Some(30));

        assert!(config.scheduler.as_ref().unwrap().enabled);

        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents[0].id, "default");
        assert_eq!(config.agents[1].id, "coder");
        assert_eq!(config.agents[1].model.as_deref(), Some("opus"));
        assert_eq!(
            config.agents[1].system_prompt.as_deref(),
            Some("You are a coding assistant.")
        );
    }

    #[test]
    fn minimal_config() {
        let toml = r#"
[cli.claude]
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();

        assert!(config.data_dir.is_none());
        assert!(config.log_level.is_none());
        assert!(config.discord.is_none());
        assert!(config.heartbeat.is_none());
        assert!(config.scheduler.is_none());
        assert!(config.cli.claude.command.is_none());
        assert!(config.cli.claude.model.is_none());
        assert!(config.cli.claude.timeout_seconds.is_none());
        assert!(config.cli.claude.skip_permissions.is_none());
        assert!(config.cli.claude.extra_flags.is_empty());
    }

    #[test]
    fn data_dir_default() {
        let toml = r#"
[cli.claude]
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        let expected = dirs::home_dir().unwrap().join(".threshold");
        assert_eq!(config.data_dir().unwrap(), expected);
    }

    #[test]
    fn data_dir_override() {
        let toml = r#"
data_dir = "/custom/data"

[cli.claude]
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.data_dir().unwrap(), PathBuf::from("/custom/data"));
    }

    #[test]
    fn agents_default_to_empty() {
        let toml = r#"
[cli.claude]
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        assert!(config.agents.is_empty());
    }

    #[test]
    fn tools_default() {
        let toml = r#"
[cli.claude]
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        assert!(config.tools.permission_mode.is_none());
        assert!(config.tools.browser.is_none());
        assert!(config.tools.gmail.is_none());
        assert!(config.tools.image_gen.is_none());
    }

    #[test]
    fn rejects_invalid_toml() {
        let bad_toml = "this is not valid [[[ toml";
        let result: Result<ThresholdConfig, _> = toml::from_str(bad_toml);
        assert!(result.is_err());
    }

    #[test]
    fn load_from_env_var() {
        let dir = std::env::temp_dir().join("threshold_test_load_env");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(&path, "[cli.claude]\n").unwrap();

        // SAFETY: test-only, single-threaded access to this env var.
        unsafe { std::env::set_var("THRESHOLD_CONFIG", &path) };
        let result = ThresholdConfig::load();
        unsafe { std::env::remove_var("THRESHOLD_CONFIG") };
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_ok());
    }

    #[test]
    fn load_missing_file_returns_config_not_found() {
        // SAFETY: test-only, single-threaded access to this env var.
        unsafe { std::env::set_var("THRESHOLD_CONFIG", "/nonexistent/path/config.toml") };
        let result = ThresholdConfig::load();
        unsafe { std::env::remove_var("THRESHOLD_CONFIG") };

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::ThresholdError::ConfigNotFound { .. }),
            "expected ConfigNotFound, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_invalid_log_level() {
        let toml = r#"
log_level = "verbose"

[cli.claude]
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("invalid log_level"));
    }

    #[test]
    fn validate_rejects_invalid_permission_mode() {
        let toml = r#"
[cli.claude]

[tools]
permission_mode = "yolo"
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("permission_mode"));
    }

    #[test]
    fn validate_rejects_unknown_cli_provider() {
        let toml = r#"
[cli.claude]

[[agents]]
id = "bot"
name = "Bot"
cli_provider = "gemini"
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("unsupported cli_provider"));
    }

    #[test]
    fn validate_rejects_invalid_agent_tools() {
        let toml = r#"
[cli.claude]

[[agents]]
id = "bot"
name = "Bot"
cli_provider = "claude"
tools = "everything"
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("invalid tools"));
    }

    #[test]
    fn validate_rejects_empty_discord_allowed_users() {
        let toml = r#"
[cli.claude]

[discord]
guild_id = 123
allowed_user_ids = []
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("allowed_user_ids"));
    }

    #[test]
    fn validate_accepts_valid_config() {
        let toml = r#"
log_level = "debug"

[cli.claude]

[tools]
permission_mode = "approve-destructive"

[discord]
guild_id = 123
allowed_user_ids = [456]

[[agents]]
id = "a"
name = "A"
cli_provider = "claude"
tools = "coding"
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_accepts_standard_profile() {
        let toml = r#"
[cli.claude]

[[agents]]
id = "a"
name = "A"
cli_provider = "claude"
tools = "standard"
"#;
        let config: ThresholdConfig = toml::from_str(toml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_validates_on_read() {
        let dir = std::env::temp_dir().join("threshold_test_load_validate");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(&path, "log_level = \"banana\"\n\n[cli.claude]\n").unwrap();

        // SAFETY: test-only, single-threaded access to this env var.
        unsafe { std::env::set_var("THRESHOLD_CONFIG", &path) };
        let result = ThresholdConfig::load();
        unsafe { std::env::remove_var("THRESHOLD_CONFIG") };
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid log_level")
        );
    }
}
