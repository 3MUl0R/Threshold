//! Google Gmail API client.
//!
//! Wraps the Gmail REST API v1 for listing, reading, searching, sending,
//! and replying to email messages.

use std::sync::Arc;

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use threshold_core::SecretStore;

use crate::auth::{AuthError, GmailAuth};
use crate::types::{AttachmentInfo, EmailMessage, MessageSummary};

/// Validate that a message ID contains only safe characters (alphanumeric).
/// Gmail message IDs are hex strings — reject anything else to prevent URL injection.
fn validate_message_id(id: &str) -> Result<&str, GmailApiError> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(GmailApiError::ParseError(format!(
            "Invalid message ID: '{}'",
            id
        )));
    }
    Ok(id)
}

/// Strip CR/LF from header values to prevent header injection.
fn sanitize_header_value(value: &str) -> String {
    value.replace(['\r', '\n'], "")
}

/// Decode base64url data tolerantly (handles both padded and unpadded).
fn decode_base64url(data: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(data)
        .or_else(|_| URL_SAFE.decode(data))
        .ok()
}

/// Base URL for the Gmail API v1.
const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// Gmail API client bound to a specific inbox.
pub struct GmailClient {
    http: reqwest::Client,
    auth: GmailAuth,
}

impl GmailClient {
    /// Create a new client for the given inbox.
    pub fn new(secret_store: Arc<SecretStore>, inbox: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            auth: GmailAuth::new(secret_store, inbox),
        }
    }

    /// List recent messages, optionally filtered by query.
    pub async fn list_messages(
        &self,
        query: Option<&str>,
        max_results: u32,
    ) -> Result<Vec<MessageSummary>, GmailApiError> {
        let token = self.get_token().await?;

        let mut url = format!("{}/messages?maxResults={}", GMAIL_API_BASE, max_results);
        if let Some(q) = query {
            url.push_str(&format!("&q={}", urlencoded(q)));
        }

        let response: MessageListResponse = self.get_json(&url, &token).await?;

        let mut summaries = Vec::new();
        for msg_ref in response.messages.unwrap_or_default() {
            match self.get_message_metadata(&msg_ref.id, &token).await {
                Ok(summary) => summaries.push(summary),
                Err(e) => {
                    tracing::warn!("Failed to fetch message {}: {}", msg_ref.id, e);
                }
            }
        }

        Ok(summaries)
    }

    /// Get the full content of a message by ID.
    pub async fn get_message(&self, message_id: &str) -> Result<EmailMessage, GmailApiError> {
        let id = validate_message_id(message_id)?;
        let token = self.get_token().await?;
        let url = format!("{}/messages/{}?format=full", GMAIL_API_BASE, id);
        let raw: RawMessage = self.get_json(&url, &token).await?;
        parse_full_message(raw)
    }

    /// Search messages using Gmail search syntax.
    pub async fn search(
        &self,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<MessageSummary>, GmailApiError> {
        self.list_messages(Some(query), max_results).await
    }

    /// Send a new email.
    pub async fn send(
        &self,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<(), GmailApiError> {
        let token = self.get_token().await?;
        let from = self.auth.inbox();

        let raw_message = build_rfc2822_message(from, to, subject, body, None);
        let encoded = URL_SAFE_NO_PAD.encode(raw_message.as_bytes());

        let url = format!("{}/messages/send", GMAIL_API_BASE);
        let payload = serde_json::json!({ "raw": encoded });

        self.post_with_retry(&url, &payload, &token).await
    }

    /// Reply to an existing message.
    pub async fn reply(
        &self,
        message_id: &str,
        body: &str,
    ) -> Result<(), GmailApiError> {
        let id = validate_message_id(message_id)?;
        let token = self.get_token().await?;

        // Fetch original message to get threading headers (include Reply-To)
        let url = format!(
            "{}/messages/{}?format=metadata&metadataHeaders=Subject&metadataHeaders=From&metadataHeaders=Message-ID&metadataHeaders=Reply-To",
            GMAIL_API_BASE, id
        );
        let original: RawMessage = self.get_json(&url, &token).await?;

        let headers = extract_headers(&original);
        // Prefer Reply-To over From for the reply recipient
        let reply_to = headers
            .get("reply-to")
            .or_else(|| headers.get("from"))
            .cloned()
            .unwrap_or_default();
        let original_subject = headers.get("subject").cloned().unwrap_or_default();
        let original_message_id = headers.get("message-id").cloned();

        let reply_subject = if original_subject.starts_with("Re: ") {
            original_subject
        } else {
            format!("Re: {}", original_subject)
        };

        let from = self.auth.inbox();
        let raw_message =
            build_rfc2822_message(from, &reply_to, &reply_subject, body, original_message_id.as_deref());

        let encoded = URL_SAFE_NO_PAD.encode(raw_message.as_bytes());

        let send_url = format!("{}/messages/send", GMAIL_API_BASE);
        // Only include threadId if present (avoid sending null)
        let payload = if let Some(ref thread_id) = original.thread_id {
            serde_json::json!({
                "raw": encoded,
                "threadId": thread_id
            })
        } else {
            serde_json::json!({ "raw": encoded })
        };

        self.post_with_retry(&send_url, &payload, &token).await
    }

    /// Get an access token, retrying with refresh on 401.
    async fn get_token(&self) -> Result<String, GmailApiError> {
        self.auth
            .get_access_token()
            .await
            .map_err(|e| GmailApiError::AuthError(e.to_string()))
    }

    /// Fetch message metadata (for list/search results).
    async fn get_message_metadata(
        &self,
        id: &str,
        token: &str,
    ) -> Result<MessageSummary, GmailApiError> {
        let id = validate_message_id(id)?;
        let url = format!(
            "{}/messages/{}?format=metadata&metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date",
            GMAIL_API_BASE, id
        );
        let raw: RawMessage = self.get_json(&url, token).await?;

        let headers = extract_headers(&raw);
        let date = parse_date(&raw, &headers);
        let labels = raw.label_ids.unwrap_or_default();

        Ok(MessageSummary {
            id: raw.id,
            from: headers.get("from").cloned().unwrap_or_default(),
            subject: headers.get("subject").cloned().unwrap_or_default(),
            snippet: raw.snippet.unwrap_or_default(),
            date,
            labels: labels.clone(),
            is_unread: labels.contains(&"UNREAD".to_string()),
        })
    }

    /// Make an authenticated POST request with 401 retry.
    async fn post_with_retry(
        &self,
        url: &str,
        payload: &serde_json::Value,
        token: &str,
    ) -> Result<(), GmailApiError> {
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(payload)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| GmailApiError::HttpError(e.to_string()))?;

        if response.status() == 401 {
            // Token expired — try refresh and retry
            let new_token = self
                .auth
                .refresh_access_token()
                .await
                .map_err(|e| GmailApiError::AuthError(e.to_string()))?;

            let response = self
                .http
                .post(url)
                .bearer_auth(&new_token)
                .json(payload)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| GmailApiError::HttpError(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let error_body = response.text().await.unwrap_or_default();
                return Err(GmailApiError::ApiError {
                    status: status.as_u16(),
                    message: error_body,
                });
            }

            return Ok(());
        }

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(GmailApiError::ApiError {
                status: status.as_u16(),
                message: error_body,
            });
        }

        Ok(())
    }

    /// Make an authenticated GET request and deserialize JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
    ) -> Result<T, GmailApiError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| GmailApiError::HttpError(e.to_string()))?;

        if response.status() == 401 {
            // Token expired — try refresh
            let new_token = self
                .auth
                .refresh_access_token()
                .await
                .map_err(|e| GmailApiError::AuthError(e.to_string()))?;

            let response = self
                .http
                .get(url)
                .bearer_auth(&new_token)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| GmailApiError::HttpError(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(GmailApiError::ApiError {
                    status: status.as_u16(),
                    message: body,
                });
            }

            return response
                .json::<T>()
                .await
                .map_err(|e| GmailApiError::ParseError(e.to_string()));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(GmailApiError::ApiError {
                status: status.as_u16(),
                message: body,
            });
        }

        response
            .json::<T>()
            .await
            .map_err(|e| GmailApiError::ParseError(e.to_string()))
    }
}

// ── RFC 2822 message construction ──

/// Build an RFC 2822 email message.
///
/// All header values are sanitized to prevent header injection (CR/LF stripped).
pub fn build_rfc2822_message(
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
) -> String {
    let mut message = String::new();
    message.push_str(&format!("From: {}\r\n", sanitize_header_value(from)));
    message.push_str(&format!("To: {}\r\n", sanitize_header_value(to)));
    message.push_str(&format!("Subject: {}\r\n", sanitize_header_value(subject)));
    message.push_str("MIME-Version: 1.0\r\n");
    message.push_str("Content-Type: text/plain; charset=utf-8\r\n");

    if let Some(reply_id) = in_reply_to {
        message.push_str(&format!(
            "In-Reply-To: {}\r\n",
            sanitize_header_value(reply_id)
        ));
        message.push_str(&format!(
            "References: {}\r\n",
            sanitize_header_value(reply_id)
        ));
    }

    message.push_str("\r\n");
    message.push_str(body);
    message
}

// ── Gmail API response types ──

#[derive(Debug, Deserialize)]
struct MessageListResponse {
    messages: Option<Vec<MessageRef>>,
    #[allow(dead_code)]
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    id: String,
    #[serde(rename = "threadId")]
    thread_id: Option<String>,
    #[serde(rename = "labelIds")]
    label_ids: Option<Vec<String>>,
    snippet: Option<String>,
    payload: Option<MessagePayload>,
    #[allow(dead_code)]
    #[serde(rename = "internalDate")]
    internal_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessagePayload {
    headers: Option<Vec<Header>>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    body: Option<MessageBody>,
    parts: Option<Vec<MessagePart>>,
}

#[derive(Debug, Deserialize)]
struct Header {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    #[allow(dead_code)]
    #[serde(rename = "attachmentId")]
    attachment_id: Option<String>,
    size: Option<u64>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessagePart {
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    filename: Option<String>,
    #[allow(dead_code)]
    headers: Option<Vec<Header>>,
    body: Option<MessageBody>,
    parts: Option<Vec<MessagePart>>,
}

// ── Parsing helpers ──

/// Extract headers into a case-insensitive map (all keys lowercased).
fn extract_headers(raw: &RawMessage) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Some(ref payload) = raw.payload
        && let Some(ref headers) = payload.headers
    {
        for h in headers {
            map.insert(h.name.to_lowercase(), h.value.clone());
        }
    }
    map
}

/// Parse a date, trying the Date header first, then internalDate (epoch millis),
/// and falling back to Utc::now() as last resort.
fn parse_date(raw: &RawMessage, headers: &std::collections::HashMap<String, String>) -> DateTime<Utc> {
    // Try Date header (RFC 2822)
    if let Some(date_str) = headers.get("date")
        && let Ok(dt) = DateTime::parse_from_rfc2822(date_str)
    {
        return dt.with_timezone(&Utc);
    }
    // Fallback to internalDate (epoch millis from Gmail API)
    if let Some(ref ms) = raw.internal_date
        && let Ok(millis) = ms.parse::<i64>()
        && let Some(dt) = DateTime::from_timestamp_millis(millis)
    {
        return dt;
    }
    Utc::now()
}

/// Parse a full message response into an `EmailMessage`.
fn parse_full_message(raw: RawMessage) -> Result<EmailMessage, GmailApiError> {
    let headers = extract_headers(&raw);

    let mut body_text = String::new();
    let mut body_html = None;
    let mut attachments = Vec::new();

    if let Some(ref payload) = raw.payload {
        extract_body_parts(payload, &mut body_text, &mut body_html, &mut attachments);
    }

    Ok(EmailMessage {
        id: raw.id.clone(),
        from: headers.get("from").cloned().unwrap_or_default(),
        to: parse_address_list(headers.get("to").map(|s| s.as_str())),
        cc: parse_address_list(headers.get("cc").map(|s| s.as_str())),
        subject: headers.get("subject").cloned().unwrap_or_default(),
        body_text,
        body_html,
        date: parse_date(&raw, &headers),
        labels: raw.label_ids.unwrap_or_default(),
        attachments,
    })
}

/// Recursively extract body text, HTML, and attachments from MIME parts.
fn extract_body_parts(
    payload: &MessagePayload,
    text: &mut String,
    html: &mut Option<String>,
    attachments: &mut Vec<AttachmentInfo>,
) {
    // Check if this payload has a direct body
    if let Some(ref body) = payload.body
        && let Some(ref data) = body.data
        && let Some(decoded) = decode_base64url(data)
        && let Ok(content) = String::from_utf8(decoded)
    {
        match payload.mime_type.as_deref() {
            Some("text/plain") if text.is_empty() => *text = content,
            Some("text/html") if html.is_none() => *html = Some(content),
            _ => {}
        }
    }

    // Recurse into MIME parts
    if let Some(ref parts) = payload.parts {
        for part in parts {
            extract_part(part, text, html, attachments);
        }
    }
}

fn extract_part(
    part: &MessagePart,
    text: &mut String,
    html: &mut Option<String>,
    attachments: &mut Vec<AttachmentInfo>,
) {
    // Check for attachment
    if let Some(ref filename) = part.filename
        && !filename.is_empty()
    {
        if let Some(ref body) = part.body {
            attachments.push(AttachmentInfo {
                filename: filename.clone(),
                mime_type: part.mime_type.clone().unwrap_or_default(),
                size_bytes: body.size.unwrap_or(0),
            });
        }
        return;
    }

    // Decode body data
    if let Some(ref body) = part.body
        && let Some(ref data) = body.data
        && let Some(decoded) = decode_base64url(data)
        && let Ok(content) = String::from_utf8(decoded)
    {
        match part.mime_type.as_deref() {
            Some("text/plain") if text.is_empty() => *text = content,
            Some("text/html") if html.is_none() => *html = Some(content),
            _ => {}
        }
    }

    // Recurse into nested parts
    if let Some(ref sub_parts) = part.parts {
        for sub in sub_parts {
            extract_part(sub, text, html, attachments);
        }
    }
}

fn parse_address_list(header: Option<&str>) -> Vec<String> {
    header
        .map(|s| s.split(',').map(|a| a.trim().to_string()).collect())
        .unwrap_or_default()
}

fn urlencoded(s: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("", s)
        .finish()
        .trim_start_matches('=')
        .to_string()
}

/// Errors from the Gmail API client.
#[derive(Debug, thiserror::Error)]
pub enum GmailApiError {
    #[error("Authentication error: {0}")]
    AuthError(String),

    #[error("HTTP error: {0}")]
    HttpError(String),

    #[error("Gmail API error (HTTP {status}): {message}")]
    ApiError { status: u16, message: String },

    #[error("Response parse error: {0}")]
    ParseError(String),
}

impl From<AuthError> for GmailApiError {
    fn from(e: AuthError) -> Self {
        GmailApiError::AuthError(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rfc2822_simple_message() {
        let msg = build_rfc2822_message(
            "alice@gmail.com",
            "bob@example.com",
            "Hello",
            "Hi Bob!",
            None,
        );

        assert!(msg.contains("From: alice@gmail.com"));
        assert!(msg.contains("To: bob@example.com"));
        assert!(msg.contains("Subject: Hello"));
        assert!(msg.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(msg.contains("Hi Bob!"));
        assert!(!msg.contains("In-Reply-To"));
    }

    #[test]
    fn build_rfc2822_reply_message() {
        let msg = build_rfc2822_message(
            "alice@gmail.com",
            "bob@example.com",
            "Re: Hello",
            "Thanks!",
            Some("<orig@example.com>"),
        );

        assert!(msg.contains("In-Reply-To: <orig@example.com>"));
        assert!(msg.contains("References: <orig@example.com>"));
    }

    #[test]
    fn parse_address_list_single() {
        let addrs = parse_address_list(Some("alice@example.com"));
        assert_eq!(addrs, vec!["alice@example.com"]);
    }

    #[test]
    fn parse_address_list_multiple() {
        let addrs =
            parse_address_list(Some("alice@example.com, bob@example.com, charlie@example.com"));
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[0], "alice@example.com");
        assert_eq!(addrs[1], "bob@example.com");
        assert_eq!(addrs[2], "charlie@example.com");
    }

    #[test]
    fn parse_address_list_empty() {
        let addrs = parse_address_list(None);
        assert!(addrs.is_empty());
    }

    #[test]
    fn urlencoded_spaces() {
        let encoded = urlencoded("from:alice subject:hello world");
        assert!(encoded.contains("from"));
        assert!(encoded.contains("hello"));
        // Should not contain raw spaces
        assert!(!encoded.contains(' '));
    }

    fn make_raw_message_with_date(date_header: Option<&str>, internal_date: Option<&str>) -> RawMessage {
        let headers = date_header.map(|d| {
            vec![Header {
                name: "Date".into(),
                value: d.into(),
            }]
        });
        RawMessage {
            id: "test".into(),
            thread_id: None,
            label_ids: None,
            snippet: None,
            payload: Some(MessagePayload {
                headers,
                mime_type: None,
                body: None,
                parts: None,
            }),
            internal_date: internal_date.map(|s| s.to_string()),
        }
    }

    #[test]
    fn parse_date_valid_rfc2822() {
        let raw = make_raw_message_with_date(
            Some("Tue, 17 Feb 2026 12:00:00 +0000"),
            None,
        );
        let headers = extract_headers(&raw);
        let dt = parse_date(&raw, &headers);
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 2);
    }

    #[test]
    fn parse_date_invalid_header_uses_internal_date() {
        let raw = make_raw_message_with_date(
            Some("not a date"),
            Some("1708185600000"), // 2024-02-17T12:00:00Z in millis
        );
        let headers = extract_headers(&raw);
        let dt = parse_date(&raw, &headers);
        assert_eq!(dt.year(), 2024);
    }

    #[test]
    fn parse_date_no_header_uses_internal_date() {
        let raw = make_raw_message_with_date(
            None,
            Some("1708185600000"),
        );
        let headers = extract_headers(&raw);
        let dt = parse_date(&raw, &headers);
        assert_eq!(dt.year(), 2024);
    }

    #[test]
    fn parse_date_nothing_falls_back_to_now() {
        let raw = make_raw_message_with_date(None, None);
        let headers = extract_headers(&raw);
        let dt = parse_date(&raw, &headers);
        assert!(dt.year() >= 2024);
    }

    #[test]
    fn message_list_response_deserializes() {
        let json = r#"{
            "messages": [
                {"id": "msg1", "threadId": "thread1"},
                {"id": "msg2", "threadId": "thread2"}
            ],
            "nextPageToken": "abc123"
        }"#;

        let resp: MessageListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.messages.unwrap().len(), 2);
        assert_eq!(resp.next_page_token.unwrap(), "abc123");
    }

    #[test]
    fn message_list_response_empty() {
        let json = r#"{}"#;
        let resp: MessageListResponse = serde_json::from_str(json).unwrap();
        assert!(resp.messages.is_none());
    }

    #[test]
    fn raw_message_deserializes() {
        let json = r#"{
            "id": "msg123",
            "threadId": "thread456",
            "labelIds": ["INBOX", "UNREAD"],
            "snippet": "Hello world...",
            "payload": {
                "headers": [
                    {"name": "From", "value": "alice@example.com"},
                    {"name": "Subject", "value": "Test"}
                ],
                "mimeType": "text/plain",
                "body": {
                    "size": 12,
                    "data": "SGVsbG8gV29ybGQh"
                }
            }
        }"#;

        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, "msg123");
        assert_eq!(msg.thread_id.unwrap(), "thread456");
        assert_eq!(msg.label_ids.unwrap().len(), 2);
    }

    #[test]
    fn extract_headers_from_raw_message() {
        let raw = RawMessage {
            id: "msg1".into(),
            thread_id: None,
            label_ids: None,
            snippet: None,
            payload: Some(MessagePayload {
                headers: Some(vec![
                    Header {
                        name: "From".into(),
                        value: "alice@example.com".into(),
                    },
                    Header {
                        name: "Subject".into(),
                        value: "Test".into(),
                    },
                ]),
                mime_type: None,
                body: None,
                parts: None,
            }),
            internal_date: None,
        };

        let headers = extract_headers(&raw);
        assert_eq!(headers["from"], "alice@example.com");
        assert_eq!(headers["subject"], "Test");
    }

    #[test]
    fn parse_full_message_plain_text() {
        let raw = RawMessage {
            id: "msg1".into(),
            thread_id: Some("thread1".into()),
            label_ids: Some(vec!["INBOX".into()]),
            snippet: Some("Hello...".into()),
            payload: Some(MessagePayload {
                headers: Some(vec![
                    Header { name: "From".into(), value: "alice@example.com".into() },
                    Header { name: "To".into(), value: "bob@example.com".into() },
                    Header { name: "Subject".into(), value: "Test".into() },
                    Header { name: "Date".into(), value: "Mon, 15 Jun 2025 12:00:00 +0000".into() },
                ]),
                mime_type: Some("text/plain".into()),
                body: Some(MessageBody {
                    attachment_id: None,
                    size: Some(12),
                    data: Some(URL_SAFE_NO_PAD.encode("Hello World!")),
                }),
                parts: None,
            }),
            internal_date: None,
        };

        let email = parse_full_message(raw).unwrap();
        assert_eq!(email.id, "msg1");
        assert_eq!(email.from, "alice@example.com");
        assert_eq!(email.to, vec!["bob@example.com"]);
        assert_eq!(email.subject, "Test");
        assert_eq!(email.body_text, "Hello World!");
        assert!(email.body_html.is_none());
        assert!(email.attachments.is_empty());
    }

    #[test]
    fn parse_full_message_multipart() {
        let raw = RawMessage {
            id: "msg2".into(),
            thread_id: None,
            label_ids: None,
            snippet: None,
            payload: Some(MessagePayload {
                headers: Some(vec![
                    Header { name: "From".into(), value: "sender@example.com".into() },
                    Header { name: "To".into(), value: "recipient@example.com".into() },
                    Header { name: "Subject".into(), value: "Multipart".into() },
                ]),
                mime_type: Some("multipart/alternative".into()),
                body: None,
                parts: Some(vec![
                    MessagePart {
                        mime_type: Some("text/plain".into()),
                        filename: None,
                        headers: None,
                        body: Some(MessageBody {
                            attachment_id: None,
                            size: Some(5),
                            data: Some(URL_SAFE_NO_PAD.encode("Plain")),
                        }),
                        parts: None,
                    },
                    MessagePart {
                        mime_type: Some("text/html".into()),
                        filename: None,
                        headers: None,
                        body: Some(MessageBody {
                            attachment_id: None,
                            size: Some(12),
                            data: Some(URL_SAFE_NO_PAD.encode("<b>HTML</b>")),
                        }),
                        parts: None,
                    },
                ]),
            }),
            internal_date: None,
        };

        let email = parse_full_message(raw).unwrap();
        assert_eq!(email.body_text, "Plain");
        assert_eq!(email.body_html.unwrap(), "<b>HTML</b>");
    }

    #[test]
    fn parse_full_message_with_attachment() {
        let raw = RawMessage {
            id: "msg3".into(),
            thread_id: None,
            label_ids: None,
            snippet: None,
            payload: Some(MessagePayload {
                headers: Some(vec![
                    Header { name: "From".into(), value: "sender@example.com".into() },
                    Header { name: "Subject".into(), value: "With attachment".into() },
                ]),
                mime_type: Some("multipart/mixed".into()),
                body: None,
                parts: Some(vec![
                    MessagePart {
                        mime_type: Some("text/plain".into()),
                        filename: None,
                        headers: None,
                        body: Some(MessageBody {
                            attachment_id: None,
                            size: Some(4),
                            data: Some(URL_SAFE_NO_PAD.encode("Body")),
                        }),
                        parts: None,
                    },
                    MessagePart {
                        mime_type: Some("application/pdf".into()),
                        filename: Some("report.pdf".into()),
                        headers: None,
                        body: Some(MessageBody {
                            attachment_id: Some("att123".into()),
                            size: Some(1024),
                            data: None,
                        }),
                        parts: None,
                    },
                ]),
            }),
            internal_date: None,
        };

        let email = parse_full_message(raw).unwrap();
        assert_eq!(email.body_text, "Body");
        assert_eq!(email.attachments.len(), 1);
        assert_eq!(email.attachments[0].filename, "report.pdf");
        assert_eq!(email.attachments[0].mime_type, "application/pdf");
        assert_eq!(email.attachments[0].size_bytes, 1024);
    }

    #[test]
    fn gmail_api_error_display() {
        let err = GmailApiError::ApiError {
            status: 404,
            message: "Not Found".into(),
        };
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("Not Found"));
    }

    #[test]
    fn validate_message_id_valid() {
        assert!(validate_message_id("18f3a2b4c5d6e7f8").is_ok());
        assert!(validate_message_id("abc123").is_ok());
    }

    #[test]
    fn validate_message_id_rejects_traversal() {
        assert!(validate_message_id("../../../etc/passwd").is_err());
        assert!(validate_message_id("msg?query=x").is_err());
        assert!(validate_message_id("msg/other").is_err());
        assert!(validate_message_id("").is_err());
    }

    #[test]
    fn sanitize_header_strips_crlf() {
        assert_eq!(sanitize_header_value("normal value"), "normal value");
        assert_eq!(
            sanitize_header_value("injected\r\nBcc: hacker@evil.com"),
            "injectedBcc: hacker@evil.com"
        );
        assert_eq!(sanitize_header_value("line\nbreak"), "linebreak");
    }

    #[test]
    fn decode_base64url_tolerant() {
        // Unpadded
        let data = URL_SAFE_NO_PAD.encode("hello");
        assert_eq!(decode_base64url(&data).unwrap(), b"hello");

        // Padded
        let data_padded = URL_SAFE.encode("hello");
        assert_eq!(decode_base64url(&data_padded).unwrap(), b"hello");
    }

    use chrono::Datelike;
}
