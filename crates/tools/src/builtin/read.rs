//! File reading tool

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use threshold_core::{Result, ThresholdError};

use crate::{Tool, ToolContext, ToolResult};

/// ReadTool - reads file contents with size limits
pub struct ReadTool;

#[derive(Debug, Deserialize)]
struct ReadParams {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

const MAX_READ_SIZE: usize = 1024 * 1024; // 1MB

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read file contents with optional offset and limit"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Byte offset to start reading from (optional)",
                    "minimum": 0
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum bytes to read (optional, max 1MB)",
                    "minimum": 1,
                    "maximum": MAX_READ_SIZE
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let params: ReadParams = serde_json::from_value(params)?;

        // Resolve path relative to working directory
        let path = if PathBuf::from(&params.path).is_absolute() {
            PathBuf::from(&params.path)
        } else {
            ctx.working_dir.join(&params.path)
        };

        // Check file exists
        if !path.exists() {
            return Err(ThresholdError::NotFound {
                message: format!("File not found: {}", path.display()),
            });
        }

        // Check it's a file, not a directory
        if !path.is_file() {
            return Err(ThresholdError::InvalidInput {
                message: format!("Path is not a file: {}", path.display()),
            });
        }

        // Read file contents
        let contents = tokio::fs::read(&path).await?;

        // Apply offset and limit
        let start = params.offset.unwrap_or(0);
        let end = if let Some(limit) = params.limit {
            std::cmp::min(start + limit, contents.len())
        } else {
            contents.len()
        };

        // Check read size limit
        if end - start > MAX_READ_SIZE {
            return Err(ThresholdError::InvalidInput {
                message: format!(
                    "Read size exceeds maximum of {} bytes",
                    MAX_READ_SIZE
                ),
            });
        }

        // Extract slice and convert to string
        let slice = &contents[start..end];
        let content = String::from_utf8_lossy(slice).to_string();

        Ok(ToolResult::success(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn read_tool_name_is_read() {
        let tool = ReadTool;
        assert_eq!(tool.name(), "read");
    }

    #[tokio::test]
    async fn read_tool_reads_file_contents() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = ReadTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "hello world");
    }

    #[tokio::test]
    async fn read_tool_reads_absolute_path() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "absolute path test").await.unwrap();

        let tool = ReadTool;
        let ctx = ToolContext::new("test-agent");

        let params = json!({
            "path": file_path.to_str().unwrap()
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "absolute path test");
    }

    #[tokio::test]
    async fn read_tool_reads_with_offset() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "0123456789").await.unwrap();

        let tool = ReadTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "offset": 5
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "56789");
    }

    #[tokio::test]
    async fn read_tool_reads_with_limit() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "0123456789").await.unwrap();

        let tool = ReadTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "limit": 5
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "01234");
    }

    #[tokio::test]
    async fn read_tool_reads_with_offset_and_limit() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "0123456789").await.unwrap();

        let tool = ReadTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "offset": 3,
            "limit": 4
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "3456");
    }

    #[tokio::test]
    async fn read_tool_fails_for_nonexistent_file() {
        let tool = ReadTool;
        let ctx = ToolContext::new("test-agent");

        let params = json!({
            "path": "/nonexistent/file.txt"
        });

        let result = tool.execute(params, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::NotFound { .. }));
    }

    #[tokio::test]
    async fn read_tool_fails_for_directory() {
        let dir = tempdir().unwrap();

        let tool = ReadTool;
        let ctx = ToolContext::new("test-agent");

        let params = json!({
            "path": dir.path().to_str().unwrap()
        });

        let result = tool.execute(params, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::InvalidInput { .. }));
    }
}
