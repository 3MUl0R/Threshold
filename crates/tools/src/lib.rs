//! Tool Framework - extensible tool system for Threshold
//!
//! Provides a trait-based system for executing tools (shell commands, file operations,
//! web access) with profile-based permission control and audit logging.

pub mod builtin;
pub mod prompt;
mod context;
mod profiles;
mod registry;

pub use context::ToolContext;
pub use profiles::ToolProfileExt;
pub use prompt::build_tool_prompt;
pub use registry::{ToolRegistry, ToolsConfig};

use async_trait::async_trait;
use serde_json::Value;
use threshold_core::Result;

/// Maximum size of tool result content (100KB)
pub const MAX_RESULT_SIZE: usize = 100 * 1024;

/// The Tool trait - implemented by all tools
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name (e.g., "exec", "read", "gmail").
    fn name(&self) -> &str;

    /// Human-readable description for the AI.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn schema(&self) -> Value;

    /// Execute the tool with the given parameters.
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult>;
}

/// Result of tool execution
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// Text output (max 100KB, truncated if larger)
    pub content: String,
    /// Files, images, etc.
    pub artifacts: Vec<Artifact>,
    /// Whether the tool execution was successful
    pub success: bool,
}

/// File or binary artifact produced by tools
#[derive(Debug, Clone)]
pub struct Artifact {
    /// Filename
    pub name: String,
    /// Raw bytes
    pub data: Vec<u8>,
    /// MIME type
    pub mime_type: String,
}

impl ToolResult {
    /// Create a successful result with content
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            artifacts: Vec::new(),
            success: true,
        }
    }

    /// Create a failed result with error message
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            artifacts: Vec::new(),
            success: false,
        }
    }

    /// Add an artifact to the result
    pub fn with_artifact(mut self, artifact: Artifact) -> Self {
        self.artifacts.push(artifact);
        self
    }

    /// Truncate content if it exceeds MAX_RESULT_SIZE
    pub fn truncate(mut self) -> Self {
        if self.content.len() > MAX_RESULT_SIZE {
            // Find valid UTF-8 boundary
            let mut boundary = MAX_RESULT_SIZE;
            while boundary > 0 && !self.content.is_char_boundary(boundary) {
                boundary -= 1;
            }
            self.content.truncate(boundary);
            self.content.push_str("\n\n... [output truncated at 100KB]");
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_success_creates_successful_result() {
        let result = ToolResult::success("test output");
        assert!(result.success);
        assert_eq!(result.content, "test output");
        assert!(result.artifacts.is_empty());
    }

    #[test]
    fn tool_result_failure_creates_failed_result() {
        let result = ToolResult::failure("error message");
        assert!(!result.success);
        assert_eq!(result.content, "error message");
        assert!(result.artifacts.is_empty());
    }

    #[test]
    fn tool_result_with_artifact_adds_artifact() {
        let artifact = Artifact {
            name: "test.txt".to_string(),
            data: vec![1, 2, 3],
            mime_type: "text/plain".to_string(),
        };
        let result = ToolResult::success("output").with_artifact(artifact);
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(result.artifacts[0].name, "test.txt");
    }

    #[test]
    fn tool_result_truncate_respects_max_size() {
        let large_content = "a".repeat(MAX_RESULT_SIZE + 1000);
        let result = ToolResult::success(large_content).truncate();
        assert!(result.content.len() <= MAX_RESULT_SIZE + 100); // +100 for truncation message
        assert!(result.content.ends_with("... [output truncated at 100KB]"));
    }

    #[test]
    fn tool_result_truncate_preserves_utf8_boundaries() {
        let mut content = "a".repeat(MAX_RESULT_SIZE - 10);
        content.push_str("😀"); // 4-byte emoji
        let result = ToolResult::success(content).truncate();
        assert!(result.content.is_char_boundary(result.content.len()));
    }

    #[test]
    fn tool_result_truncate_leaves_small_content_unchanged() {
        let small_content = "small output";
        let result = ToolResult::success(small_content).truncate();
        assert_eq!(result.content, "small output");
    }
}
