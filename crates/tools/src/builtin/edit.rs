//! File editing tool with find/replace functionality

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use threshold_core::{Result, ThresholdError};

use crate::{Tool, ToolContext, ToolResult};

/// EditTool - performs find/replace operations on files
pub struct EditTool;

#[derive(Debug, Deserialize)]
struct EditParams {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

const MAX_FILE_SIZE: usize = 10 * 1024 * 1024; // 10MB

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing text (find/replace)"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)"
                },
                "old_text": {
                    "type": "string",
                    "description": "Text to find and replace"
                },
                "new_text": {
                    "type": "string",
                    "description": "Text to replace with"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false, only first occurrence)",
                    "default": false
                }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let params: EditParams = serde_json::from_value(params)?;

        // Validate old_text is not empty
        if params.old_text.is_empty() {
            return Err(ThresholdError::InvalidInput {
                message: "old_text cannot be empty".to_string(),
            });
        }

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

        // Check it's a file
        if !path.is_file() {
            return Err(ThresholdError::InvalidInput {
                message: format!("Path is not a file: {}", path.display()),
            });
        }

        // Check file size BEFORE reading
        let metadata = tokio::fs::metadata(&path).await?;
        let file_size = metadata.len() as usize;
        if file_size > MAX_FILE_SIZE {
            return Err(ThresholdError::InvalidInput {
                message: format!(
                    "File size {} exceeds maximum of {} bytes",
                    file_size, MAX_FILE_SIZE
                ),
            });
        }

        // Read file contents
        let contents = tokio::fs::read_to_string(&path).await?;

        // Perform replacement
        let (new_contents, replacements) = if params.replace_all {
            let count = contents.matches(&params.old_text).count();
            let replaced = contents.replace(&params.old_text, &params.new_text);
            (replaced, count)
        } else {
            // Replace only first occurrence
            if let Some(index) = contents.find(&params.old_text) {
                let mut new_contents = String::with_capacity(contents.len());
                new_contents.push_str(&contents[..index]);
                new_contents.push_str(&params.new_text);
                new_contents.push_str(&contents[index + params.old_text.len()..]);
                (new_contents, 1)
            } else {
                (contents.clone(), 0)
            }
        };

        // Check if any replacements were made
        if replacements == 0 {
            return Err(ThresholdError::InvalidInput {
                message: "old_text not found in file".to_string(),
            });
        }

        // Write back to file
        tokio::fs::write(&path, new_contents.as_bytes()).await?;

        Ok(ToolResult::success(format!(
            "Replaced {} occurrence(s) in {}",
            replacements,
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
    async fn edit_tool_name_is_edit() {
        let tool = EditTool;
        assert_eq!(tool.name(), "edit");
    }

    #[tokio::test]
    async fn edit_tool_replaces_first_occurrence() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world, hello universe")
            .await
            .unwrap();

        let tool = EditTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "old_text": "hello",
            "new_text": "goodbye"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.content.contains("1 occurrence"));

        // Verify file was edited
        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "goodbye world, hello universe");
    }

    #[tokio::test]
    async fn edit_tool_replaces_all_occurrences() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world, hello universe")
            .await
            .unwrap();

        let tool = EditTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "old_text": "hello",
            "new_text": "goodbye",
            "replace_all": true
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.content.contains("2 occurrence"));

        // Verify file was edited
        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "goodbye world, goodbye universe");
    }

    #[tokio::test]
    async fn edit_tool_works_with_multiline_text() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "line 1\nline 2\nline 3")
            .await
            .unwrap();

        let tool = EditTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "old_text": "line 2",
            "new_text": "modified line 2"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);

        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "line 1\nmodified line 2\nline 3");
    }

    #[tokio::test]
    async fn edit_tool_fails_if_text_not_found() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = EditTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "old_text": "nonexistent",
            "new_text": "something"
        });

        let result = tool.execute(params, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn edit_tool_fails_for_empty_old_text() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = EditTool;
        let ctx = ToolContext::new("test-agent")
            .with_working_dir(dir.path().to_path_buf());

        let params = json!({
            "path": "test.txt",
            "old_text": "",
            "new_text": "something"
        });

        let result = tool.execute(params, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn edit_tool_fails_for_nonexistent_file() {
        let tool = EditTool;
        let ctx = ToolContext::new("test-agent");

        let params = json!({
            "path": "/nonexistent/file.txt",
            "old_text": "hello",
            "new_text": "goodbye"
        });

        let result = tool.execute(params, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::NotFound { .. }));
    }

    #[tokio::test]
    async fn edit_tool_works_with_absolute_path() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = EditTool;
        let ctx = ToolContext::new("test-agent");

        let params = json!({
            "path": file_path.to_str().unwrap(),
            "old_text": "world",
            "new_text": "rust"
        });

        let result = tool.execute(params, &ctx).await.unwrap();
        assert!(result.success);

        let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(contents, "hello rust");
    }
}
