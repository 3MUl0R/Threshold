//! Output formatting for CLI commands.

/// Output format for CLI command results.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    /// JSON output (machine-readable)
    Json,
    /// Table output (human-readable)
    Table,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Json
    }
}
