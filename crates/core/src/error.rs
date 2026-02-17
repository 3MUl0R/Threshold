use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ThresholdError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Configuration file not found: {path}")]
    ConfigNotFound { path: PathBuf },

    #[error("Secret not found: {key}")]
    SecretNotFound { key: String },

    #[error("CLI error: {provider} exited with code {code}: {stderr}")]
    CliError {
        provider: String,
        code: i32,
        stderr: String,
    },

    #[error("CLI timeout after {timeout_ms}ms")]
    CliTimeout { timeout_ms: u64 },

    #[error("CLI not found: {command} — is it installed?")]
    CliNotFound { command: String },

    #[error("Discord error: {0}")]
    Discord(String),

    #[error("Tool error in '{tool}': {message}")]
    ToolError { tool: String, message: String },

    #[error("Tool not permitted: '{tool}' is not in the {profile:?} profile")]
    ToolNotPermitted { tool: String, profile: String },

    #[error("Conversation not found: {id}")]
    ConversationNotFound { id: uuid::Uuid },

    #[error("Portal not found: {id}")]
    PortalNotFound { id: uuid::Uuid },

    #[error("Audit trail write failed: {0}")]
    AuditWrite(#[source] std::io::Error),

    #[error("Audit trail read failed: {0}")]
    AuditRead(#[source] std::io::Error),

    #[error("Logging initialization failed: {0}")]
    LoggingInit(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Keychain error: {0}")]
    Keychain(String),

    #[error("Unauthorized access attempt")]
    Unauthorized,

    #[error("External service error: {0}")]
    External(String),

    #[error("Scheduler is shutting down")]
    SchedulerShutdown,

    #[error("I/O error at {path}: {message}")]
    IoError { path: String, message: String },

    #[error("Serialization error: {message}")]
    SerializationError { message: String },

    #[error("Not found: {message}")]
    NotFound { message: String },

    #[error("Invalid input: {message}")]
    InvalidInput { message: String },
}

pub type Result<T> = std::result::Result<T, ThresholdError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_displays_message() {
        let err = ThresholdError::Config("invalid value".into());
        assert_eq!(err.to_string(), "Configuration error: invalid value");
    }

    #[test]
    fn config_not_found_displays_path() {
        let err = ThresholdError::ConfigNotFound {
            path: PathBuf::from("/home/user/.threshold/config.toml"),
        };
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn cli_error_displays_all_fields() {
        let err = ThresholdError::CliError {
            provider: "claude".into(),
            code: 1,
            stderr: "something went wrong".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("claude"));
        assert!(msg.contains("1"));
        assert!(msg.contains("something went wrong"));
    }

    #[test]
    fn tool_error_displays_tool_and_message() {
        let err = ThresholdError::ToolError {
            tool: "exec".into(),
            message: "command failed".into(),
        };
        assert!(err.to_string().contains("exec"));
        assert!(err.to_string().contains("command failed"));
    }

    #[test]
    fn io_error_converts_via_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: ThresholdError = io_err.into();
        assert!(matches!(err, ThresholdError::Io(_)));
        assert!(err.to_string().contains("file missing"));
    }

    #[test]
    fn result_type_alias_works() {
        fn returns_ok() -> Result<i32> {
            Ok(42)
        }
        assert_eq!(returns_ok().unwrap(), 42);

        fn returns_err() -> Result<i32> {
            Err(ThresholdError::Config("bad".into()))
        }
        assert!(returns_err().is_err());
    }

    #[test]
    fn scheduler_shutdown_error_displays_message() {
        let err = ThresholdError::SchedulerShutdown;
        assert_eq!(err.to_string(), "Scheduler is shutting down");
    }

    #[test]
    fn io_error_displays_path_and_message() {
        let err = ThresholdError::IoError {
            path: "/tmp/schedules.json".into(),
            message: "Failed to write file".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/schedules.json"));
        assert!(msg.contains("Failed to write file"));
    }

    #[test]
    fn serialization_error_displays_message() {
        let err = ThresholdError::SerializationError {
            message: "Invalid JSON structure".into(),
        };
        assert!(err.to_string().contains("Invalid JSON structure"));
    }

    #[test]
    fn not_found_error_displays_message() {
        let err = ThresholdError::NotFound {
            message: "Task not found".into(),
        };
        assert!(err.to_string().contains("Task not found"));
    }

    #[test]
    fn invalid_input_error_displays_message() {
        let err = ThresholdError::InvalidInput {
            message: "Invalid cron expression".into(),
        };
        assert!(err.to_string().contains("Invalid cron expression"));
    }
}
