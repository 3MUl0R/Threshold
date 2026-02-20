//! Streaming event model for Claude CLI `--output-format stream-json`.
//!
//! The Claude CLI with `--output-format stream-json` emits JSONL events on
//! stdout. Each line is a JSON object with a `type` field. We parse these
//! into a simplified `StreamEvent` enum for consumption by the engine.

use crate::response::Usage;
use serde_json::Value;

/// Events parsed from Claude CLI streaming output.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Partial text content from the assistant.
    TextDelta { text: String },

    /// The assistant is using a tool.
    ToolUse {
        tool_name: String,
        /// Tool input as raw JSON (for summarization).
        input_preview: Option<String>,
    },

    /// Tool execution completed.
    ToolResult {
        tool_name: String,
        is_error: bool,
    },

    /// Final result with complete response.
    Result {
        text: String,
        session_id: Option<String>,
        usage: Option<Usage>,
    },

    /// Error from the CLI.
    Error { message: String },
}

/// Parse a single JSONL line into a `StreamEvent`.
///
/// Returns `None` for lines that don't map to a meaningful event
/// (e.g., `system`, `message_start`, `content_block_stop`, etc.).
pub fn parse_stream_line(line: &str) -> Option<StreamEvent> {
    let json: Value = serde_json::from_str(line).ok()?;
    let event_type = json.get("type")?.as_str()?;

    match event_type {
        "content_block_delta" => parse_content_delta(&json),
        "content_block_start" => parse_content_block_start(&json),
        "result" => Some(parse_result(&json)),
        _ => None, // system, assistant, message_stop, content_block_stop, etc.
    }
}

/// Parse a content_block_delta event (text or tool input).
fn parse_content_delta(json: &Value) -> Option<StreamEvent> {
    let delta = json.get("delta")?;
    let delta_type = delta.get("type")?.as_str()?;

    match delta_type {
        "text_delta" => {
            let text = delta.get("text")?.as_str()?.to_string();
            Some(StreamEvent::TextDelta { text })
        }
        // Input JSON deltas for tool use — skip (we capture tool name at start)
        _ => None,
    }
}

/// Parse a content_block_start event (text block or tool use).
fn parse_content_block_start(json: &Value) -> Option<StreamEvent> {
    let block = json.get("content_block")?;
    let block_type = block.get("type")?.as_str()?;

    match block_type {
        "tool_use" => {
            let tool_name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(StreamEvent::ToolUse {
                tool_name,
                input_preview: None,
            })
        }
        "tool_result" => {
            // Tool results appear as content blocks; extract error status
            let is_error = block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Tool name isn't in the result block; use placeholder
            Some(StreamEvent::ToolResult {
                tool_name: "tool".to_string(),
                is_error,
            })
        }
        // "text" content_block_start is just a container — no data yet
        _ => None,
    }
}

/// Parse a result event (final output from the CLI).
fn parse_result(json: &Value) -> StreamEvent {
    // Check for error result
    let is_error = json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_error {
        let message = json
            .get("error")
            .or_else(|| json.get("result"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();
        return StreamEvent::Error { message };
    }

    let text = json
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let usage = json
        .get("usage")
        .and_then(|u| serde_json::from_value(u.clone()).ok());

    StreamEvent::Result {
        text,
        session_id,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta() {
        let line = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}"#;
        let event = parse_stream_line(line).unwrap();
        match event {
            StreamEvent::TextDelta { text } => assert_eq!(text, "Hello "),
            _ => panic!("expected TextDelta, got {:?}", event),
        }
    }

    #[test]
    fn parse_tool_use() {
        let line = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"Read","input":{}}}"#;
        let event = parse_stream_line(line).unwrap();
        match event {
            StreamEvent::ToolUse { tool_name, .. } => assert_eq!(tool_name, "Read"),
            _ => panic!("expected ToolUse, got {:?}", event),
        }
    }

    #[test]
    fn parse_result_success() {
        let line = r#"{"type":"result","subtype":"success","result":"Hello World","session_id":"abc-123","is_error":false,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let event = parse_stream_line(line).unwrap();
        match event {
            StreamEvent::Result {
                text,
                session_id,
                usage,
            } => {
                assert_eq!(text, "Hello World");
                assert_eq!(session_id, Some("abc-123".to_string()));
                assert!(usage.is_some());
                assert_eq!(usage.unwrap().input_tokens, Some(100));
            }
            _ => panic!("expected Result, got {:?}", event),
        }
    }

    #[test]
    fn parse_result_error() {
        let line =
            r#"{"type":"result","is_error":true,"error":"Rate limited","session_id":"abc"}"#;
        let event = parse_stream_line(line).unwrap();
        match event {
            StreamEvent::Error { message } => assert_eq!(message, "Rate limited"),
            _ => panic!("expected Error, got {:?}", event),
        }
    }

    #[test]
    fn parse_unknown_type_ignored() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn parse_message_stop_ignored() {
        let line = r#"{"type":"message_stop"}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_stream_line("not json").is_none());
        assert!(parse_stream_line("").is_none());
    }

    #[test]
    fn parse_tool_result_block() {
        let line = r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_result","tool_use_id":"toolu_01","content":"file contents"}}"#;
        let event = parse_stream_line(line).unwrap();
        match event {
            StreamEvent::ToolResult { is_error, .. } => assert!(!is_error),
            _ => panic!("expected ToolResult, got {:?}", event),
        }
    }

    #[test]
    fn parse_result_without_usage() {
        let line =
            r#"{"type":"result","subtype":"success","result":"Done","is_error":false}"#;
        let event = parse_stream_line(line).unwrap();
        match event {
            StreamEvent::Result { text, usage, .. } => {
                assert_eq!(text, "Done");
                assert!(usage.is_none());
            }
            _ => panic!("expected Result"),
        }
    }
}
