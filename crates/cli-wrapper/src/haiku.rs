//! HaikuClient — lightweight, session-less Claude Haiku invocations.
//!
//! Used for fast acknowledgment messages (Phase 14C). Each call spawns a
//! fresh CLI process with no session state, no per-conversation lock, and
//! a short timeout.

use crate::process::CliProcess;
use crate::response::ClaudeResponse;
use threshold_core::Result;

/// Lightweight client for single-shot Haiku invocations.
pub struct HaikuClient {
    process: CliProcess,
}

impl HaikuClient {
    /// Create a new HaikuClient with the given CLI command and a 30-second timeout.
    pub fn new(command: String) -> Self {
        Self {
            process: CliProcess::new(&command).with_timeout(30),
        }
    }

    /// Generate text with Haiku. No sessions, no locks.
    pub async fn generate(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "json".into(),
            "--model".into(),
            "haiku".into(),
        ];
        if let Some(sys) = system {
            args.push("--append-system-prompt".into());
            args.push(sys.into());
        }
        args.push(prompt.into());

        let output = self.process.run(&args, None, None).await?;
        let response = ClaudeResponse::parse(&output.stdout)?;
        Ok(response.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Requires claude CLI installed
    async fn generate_returns_text() {
        let client = HaikuClient::new("claude".to_string());
        let result = client.generate("Say hello in one word", None).await;
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }
}
