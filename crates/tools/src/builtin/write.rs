//! File writing tool

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use threshold_core::{Result, ThresholdError};

use crate::{Tool, ToolContext, ToolResult};

/// WriteTool - writes content to files
pub struct WriteTool;

#[derive(Debug, Deserialize)]
struct WriteParams {
    path: String,
    content: String,
    #[serde(default)]
    create_parents: bool,
}

const MAX_WRITE_SIZE: usize = 10 * 1024 * 1024; // 10MB

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it if it doesn't exist"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                },
                "create_parents": {
                    "type": "boolean",
                    "description": "Create parent directories if they don't exist (default: false)",
                    "default": false
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let params: WriteParams = serde_json::from_value(params)?;

        // Check size limit
        if params.content.len() > MAX_WRITE_SIZE {
            return Err(ThresholdError::InvalidInput {
                message: format!(
                    "Content size exceeds maximum of {} bytes",
                    MAX_WRITE_SIZE
                ),
            });
        }

        // Resolve path relative to working directory
        let path = if PathBuf::from(&params.path).is_absolute() {
            PathBuf::from(&params.path)
        } else {
            ctx.working_dir.join(&params.path)
        };

        // Create parent directories if requested
        if params.create_parents {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
        } else {
            // Check parent directory exists
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    return Err(ThresholdError::NotFound {
                        message: format!("Parent directory does not exist: {}", parent.display()),
                    });
                }
            }
        }

        // Write the file
        tokio::fs::write(&path, params.content.as_bytes()).await?;

        Ok(ToolResult::success(format!(
            "Wrote {} bytes to {}",
            params.content.len(),
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn write_tool_name_is_write() {
        let tool = WriteTool;
        assert_eq!(tool.name(), "write");
    }

    #[tokio::test]
    async fn write_tool_writes_file() {
        let dir = tempdir().unwrap();

        let tool = WriteTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "content": "hello world"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.content.contains("11 bytes"));

        // Verify file was written
        let file_path = dir.path().join("test.txt");
        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "hello world");
    }

    #[tokio::test]
    async fn write_tool_overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "old content").await.unwrap();

        let tool = WriteTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "content": "new content"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);

        // Verify file was overwritten
        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "new content");
    }

    #[tokio::test]
    async fn write_tool_writes_absolute_path() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");

        let tool = WriteTool;
        let ctx = ToolContext::new("test-agent");

        let params = json!({
            "path": file_path.to_str().unwrap(),
            "content": "absolute path"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);

        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "absolute path");
    }

    #[tokio::test]
    async fn write_tool_creates_parent_directories() {
        let dir = tempdir().unwrap();

        let tool = WriteTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "nested/deep/test.txt",
            "content": "nested file",
            "create_parents": true
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);

        // Verify file was written
        let file_path = dir.path().join("nested/deep/test.txt");
        assert!(file_path.exists());
        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "nested file");
    }

    #[tokio::test]
    async fn write_tool_fails_without_parent_directories() {
        let dir = tempdir().unwrap();

        let tool = WriteTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "nonexistent/test.txt",
            "content": "test"
        });

        let result = tool.execute(params, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::NotFound { .. }));
    }

    #[tokio::test]
    async fn write_tool_rejects_oversized_content() {
        let tool = WriteTool;
        let ctx = ToolContext::new("test-agent");

        // Create content larger than 10MB
        let large_content = "x".repeat(MAX_WRITE_SIZE + 1);

        let params = json!({
            "path": "/tmp/test.txt",
            "content": large_content
        });

        let result = tool.execute(params, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::InvalidInput { .. }));
    }
}
