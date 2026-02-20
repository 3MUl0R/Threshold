//! Streaming event model for Claude CLI `--output-format stream-json`.
//!
//! The Claude CLI emits JSONL on stdout with these event types:
//!   - `system`: init (session info, tools, model)
//!   - `assistant`: a full turn response with content blocks (text, tool_use)
//!   - `result`: final summary with aggregated usage
//!
//! Each `assistant` event contains a `message.content` array with content
//! blocks. We expand these into individual `StreamEvent` variants so the
//! engine can track tool use and text output for status updates.

use crate::response::Usage;
use serde_json::Value;

/// Events parsed from Claude CLI streaming output.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text content from the assistant (may be a full block, not a delta).
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

/// Parse a single JSONL line into zero or more `StreamEvent`s.
///
/// Returns an empty vec for lines that don't map to meaningful events
/// (e.g., `system` init). An `assistant` event can produce multiple
/// events (one per content block).
pub fn parse_stream_line(line: &str) -> Vec<StreamEvent> {
    let json: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let event_type = match json.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Vec::new(),
    };

    match event_type {
        "assistant" => parse_assistant_event(&json),
        "result" => vec![parse_result(&json)],
        _ => Vec::new(), // system, etc.
    }
}

/// Parse an `assistant` event — walk the content blocks and emit
/// TextDelta / ToolUse events for each one.
fn parse_assistant_event(json: &Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let content = match json
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    {
        Some(arr) => arr,
        None => return events,
    };

    for block in content {
        let block_type = match block.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };
        match block_type {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        events.push(StreamEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }
            "tool_use" => {
                let tool_name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                // input_preview is available but currently unused by the engine
                // (only tool_name is logged). Skip serialization to avoid
                // allocating large strings for big tool inputs.
                let input_preview = None;
                events.push(StreamEvent::ToolUse {
                    tool_name,
                    input_preview,
                });
            }
            "tool_result" => {
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                events.push(StreamEvent::ToolResult {
                    tool_name: "tool".to_string(),
                    is_error,
                });
            }
            _ => {} // server_tool_use, etc.
        }
    }

    events
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
    fn parse_assistant_text_only() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello World"}]}}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::TextDelta { text } => assert_eq!(text, "Hello World"),
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn parse_assistant_tool_use() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01","name":"Read","input":{"path":"/tmp/test"}}]}}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolUse {
                tool_name,
                input_preview,
            } => {
                assert_eq!(tool_name, "Read");
                // input_preview is intentionally None (unused, avoids large allocs)
                assert!(input_preview.is_none());
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn parse_assistant_mixed_content() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me read that file."},{"type":"tool_use","id":"toolu_01","name":"Read","input":{}}]}}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::TextDelta { .. }));
        assert!(matches!(&events[1], StreamEvent::ToolUse { .. }));
    }

    #[test]
    fn parse_result_success() {
        let line = r#"{"type":"result","subtype":"success","result":"Hello World","session_id":"abc-123","is_error":false,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Result {
                text,
                session_id,
                usage,
            } => {
                assert_eq!(text, "Hello World");
                assert_eq!(session_id, &Some("abc-123".to_string()));
                assert!(usage.is_some());
                assert_eq!(usage.as_ref().unwrap().input_tokens, Some(100));
            }
            other => panic!("expected Result, got {:?}", other),
        }
    }

    #[test]
    fn parse_result_error() {
        let line =
            r#"{"type":"result","is_error":true,"error":"Rate limited","session_id":"abc"}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { message } => assert_eq!(message, "Rate limited"),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn parse_assistant_tool_result() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"file contents","is_error":false}]}}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolResult { is_error, .. } => assert!(!*is_error),
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn parse_assistant_tool_result_error() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"error output","is_error":true}]}}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolResult { is_error, .. } => assert!(*is_error),
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn parse_system_init_ignored() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert!(parse_stream_line(line).is_empty());
    }

    #[test]
    fn parse_invalid_json_returns_empty() {
        assert!(parse_stream_line("not json").is_empty());
        assert!(parse_stream_line("").is_empty());
    }

    #[test]
    fn parse_result_without_usage() {
        let line =
            r#"{"type":"result","subtype":"success","result":"Done","is_error":false}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Result { text, usage, .. } => {
                assert_eq!(text, "Done");
                assert!(usage.is_none());
            }
            other => panic!("expected Result, got {:?}", other),
        }
    }

    #[test]
    fn parse_assistant_empty_text_skipped() {
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":""}]}}"#;
        let events = parse_stream_line(line);
        assert!(events.is_empty());
    }

    #[test]
    fn parse_tool_use_input_preview_always_none() {
        // input_preview is always None to avoid serializing large tool inputs
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"cmd":"echo hello"}}]}}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolUse {
                tool_name,
                input_preview,
            } => {
                assert_eq!(tool_name, "Bash");
                assert!(input_preview.is_none());
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn parse_real_cli_output() {
        // Actual format from Claude CLI --verbose --output-format stream-json
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_01","type":"message","role":"assistant","content":[{"type":"text","text":"Hello to you!"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":2}},"session_id":"abc-123"}"#;
        let events = parse_stream_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::TextDelta { text } => assert_eq!(text, "Hello to you!"),
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }
}
