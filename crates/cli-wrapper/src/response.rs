//! Claude CLI response parsing.
//!
//! Handles parsing JSON output from the Claude CLI with resilient field
//! extraction to handle various response formats.

use serde::{Deserialize, Serialize};
use threshold_core::Result;

/// Parsed response from Claude CLI
#[derive(Debug, Clone)]
pub struct ClaudeResponse {
    pub text: String,
    pub session_id: Option<String>,
    pub usage: Option<Usage>,
    pub raw_json: Option<serde_json::Value>,
}

/// Token usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl ClaudeResponse {
    /// Parse CLI stdout into a response
    ///
    /// Attempts JSON parsing first, falls back to raw text if parsing fails.
    /// Tries multiple field names for text and session_id to handle different
    /// CLI response formats.
    pub fn parse(stdout: &str) -> Result<Self> {
        // Try JSON parsing first
        match serde_json::from_str::<serde_json::Value>(stdout) {
            Ok(json) => Self::from_json(json),
            Err(_) => {
                // Fallback: treat as plain text
                tracing::warn!("CLI output is not valid JSON, using raw text");
                Ok(Self {
                    text: stdout.to_string(),
                    session_id: None,
                    usage: None,
                    raw_json: None,
                })
            }
        }
    }

    fn from_json(json: serde_json::Value) -> Result<Self> {
        // Extract text (try multiple fields)
        let text = Self::extract_text(&json).unwrap_or_else(|| json.to_string());

        // Extract session ID (try multiple field names)
        let session_id = Self::extract_session_id(&json);

        // Extract usage if present
        let usage = json
            .get("usage")
            .and_then(|u| serde_json::from_value(u.clone()).ok());

        Ok(Self {
            text,
            session_id,
            usage,
            raw_json: Some(json),
        })
    }

    fn extract_text(json: &serde_json::Value) -> Option<String> {
        // Try in priority order
        json.get("message")
            .or_else(|| json.get("content"))
            .or_else(|| json.get("result"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    fn extract_session_id(json: &serde_json::Value) -> Option<String> {
        json.get("session_id")
            .or_else(|| json.get("sessionId"))
            .or_else(|| json.get("conversation_id"))
            .or_else(|| json.get("conversationId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_message_field() {
        let json = r#"{"message": "Hello!", "session_id": "abc123"}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.text, "Hello!");
        assert_eq!(response.session_id, Some("abc123".to_string()));
    }

    #[test]
    fn parse_content_field_fallback() {
        let json = r#"{"content": "World", "sessionId": "xyz"}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.text, "World");
        assert_eq!(response.session_id, Some("xyz".to_string()));
    }

    #[test]
    fn parse_result_field_fallback() {
        let json = r#"{"result": "Test", "conversation_id": "789"}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.text, "Test");
        assert_eq!(response.session_id, Some("789".to_string()));
    }

    #[test]
    fn parse_conversation_id_fallback() {
        let json = r#"{"message": "Hi", "conversationId": "conv-123"}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.session_id, Some("conv-123".to_string()));
    }

    #[test]
    fn parse_malformed_json_uses_raw_text() {
        let text = "This is not JSON";
        let response = ClaudeResponse::parse(text).unwrap();

        assert_eq!(response.text, text);
        assert_eq!(response.session_id, None);
        assert!(response.raw_json.is_none());
    }

    #[test]
    fn parse_with_usage() {
        let json = r#"{
            "message": "Hi",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 20
            }
        }"#;
        let response = ClaudeResponse::parse(json).unwrap();

        assert!(response.usage.is_some());
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cache_read_input_tokens, Some(20));
    }

    #[test]
    fn parse_no_text_field_uses_whole_json() {
        let json = r#"{"foo": "bar", "baz": 123}"#;
        let response = ClaudeResponse::parse(json).unwrap();

        // Should stringify the whole JSON object
        assert!(response.text.contains("foo"));
        assert!(response.text.contains("bar"));
    }

    #[test]
    fn parse_empty_json_object() {
        let json = "{}";
        let response = ClaudeResponse::parse(json).unwrap();

        assert_eq!(response.text, "{}");
        assert_eq!(response.session_id, None);
    }
}
