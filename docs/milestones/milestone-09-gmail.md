# Milestone 9 — Gmail Integration

**Crate:** `gmail`
**Complexity:** Medium
**Dependencies:** Milestone 1 (core, secrets), Milestone 5 (tool framework)

## What This Milestone Delivers

Read access across multiple Gmail inboxes and permissioned send capability.
The AI can check your email, search for specific messages, summarize what's
important, and optionally send replies or new emails.

### Use Cases

- "Check my email and tell me if anything is urgent"
- "Search my inbox for emails from the Johnson project"
- "Draft a reply to Bob's last email about the deadline"
- "Every morning, summarize my important unread emails" (via cron)

---

## Phase 9.1 — Google API Client

### `crates/gmail/src/client.rs`

```rust
pub struct GmailClient {
    http: reqwest::Client,
    // Credentials are resolved per-request from the secret store
    secret_store: Arc<SecretStore>,
}

impl GmailClient {
    pub fn new(secret_store: Arc<SecretStore>) -> Self;

    /// List recent messages from an inbox.
    pub async fn list_messages(
        &self,
        inbox: &str,
        query: Option<&str>,
        max_results: u32,
    ) -> Result<Vec<MessageSummary>>;

    /// Get full message content.
    pub async fn get_message(
        &self,
        inbox: &str,
        message_id: &str,
    ) -> Result<EmailMessage>;

    /// Search messages.
    pub async fn search(
        &self,
        inbox: &str,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<MessageSummary>>;

    /// Send a new email.
    pub async fn send(
        &self,
        inbox: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<()>;

    /// Reply to an existing email.
    pub async fn reply(
        &self,
        inbox: &str,
        message_id: &str,
        body: &str,
    ) -> Result<()>;
}
```

### Data Types

```rust
#[derive(Debug, Clone, Serialize)]
pub struct MessageSummary {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub date: DateTime<Utc>,
    pub labels: Vec<String>,
    pub is_unread: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailMessage {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: String,       // Plain text body
    pub body_html: Option<String>, // HTML body (if available)
    pub date: DateTime<Utc>,
    pub labels: Vec<String>,
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachmentInfo {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
}
```

### Authentication

Gmail API requires **OAuth 2.0 user tokens** for accessing personal Gmail
accounts. A simple API key is NOT sufficient for reading or sending mail —
Google requires user consent via the OAuth flow.

**OAuth Flow for v1:**

1. User registers a Google Cloud project and creates OAuth 2.0 credentials
   (client ID + client secret) with Gmail API scopes
2. User runs `threshold gmail auth` CLI command, which:
   a. Opens a browser to Google's consent screen
   b. User grants read/send permissions
   c. Callback captures the authorization code
   d. Exchanges code for access + refresh tokens
   e. Stores both tokens in OS keychain (`gmail-oauth-access-token`,
      `gmail-oauth-refresh-token`)
3. On subsequent API calls, the client uses the access token. If expired,
   it uses the refresh token to obtain a new access token automatically.

```rust
pub struct GmailAuth {
    secret_store: Arc<SecretStore>,
    client_id: String,
    client_secret: String,
}

impl GmailAuth {
    /// Get a valid access token, refreshing if expired.
    pub async fn get_access_token(&self) -> Result<String>;

    /// Run the initial OAuth consent flow (interactive, opens browser).
    pub async fn authorize(&self) -> Result<()>;
}
```

**Keychain keys:**
- `gmail-oauth-client-id` — Google OAuth client ID
- `gmail-oauth-client-secret` — Google OAuth client secret
- `gmail-oauth-access-token` — current access token
- `gmail-oauth-refresh-token` — refresh token (long-lived)

**Required scopes:**
- `https://www.googleapis.com/auth/gmail.readonly` — read access
- `https://www.googleapis.com/auth/gmail.send` — send access (only if
  `allow_send = true` in config)

---

## Phase 9.2 — Gmail Tool

### `crates/gmail/src/tool.rs`

```rust
pub struct GmailTool {
    client: GmailClient,
    config: GmailToolConfig,
}

#[async_trait]
impl Tool for GmailTool {
    fn name(&self) -> &str { "gmail" }

    fn description(&self) -> &str {
        "Read and send email via Gmail. Can list inbox, search messages, \
         read full emails, send new emails, and reply to existing ones."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "search", "send", "reply"],
                    "description": "Gmail action to perform"
                },
                "inbox": {
                    "type": "string",
                    "description": "Email address / inbox to use"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (Gmail search syntax)"
                },
                "message_id": {
                    "type": "string",
                    "description": "Message ID (for read/reply)"
                },
                "to": {
                    "type": "string",
                    "description": "Recipient email (for send)"
                },
                "subject": {
                    "type": "string",
                    "description": "Email subject (for send)"
                },
                "body": {
                    "type": "string",
                    "description": "Email body text (for send/reply)"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max messages to return (default: 10)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let action = params["action"].as_str()
            .ok_or_else(|| tool_error("Missing 'action'"))?;

        match action {
            "list" => self.handle_list(&params).await,
            "read" => self.handle_read(&params).await,
            "search" => self.handle_search(&params).await,
            "send" => {
                self.check_send_permission(ctx)?;
                self.handle_send(&params).await
            }
            "reply" => {
                self.check_send_permission(ctx)?;
                self.handle_reply(&params).await
            }
            _ => Err(tool_error(&format!("Unknown action: {}", action))),
        }
    }
}
```

---

## Phase 9.3 — Send Permission Gate

Sending email is a potentially destructive action. The permission system
controls whether the AI can send without confirmation.

```rust
impl GmailTool {
    fn check_send_permission(&self, ctx: &ToolContext) -> Result<()> {
        // Hard gate: config must explicitly enable sending
        if !self.config.allow_send.unwrap_or(false) {
            return Err(ThresholdError::ToolError {
                tool: "gmail".into(),
                message: "Email sending is disabled. Set tools.gmail.allow_send = true \
                          in config to enable it.".into(),
            });
        }

        // Permission mode gate: in non-FullAuto modes, sending is blocked.
        // Runtime approval via Discord confirmation is a future enhancement.
        match ctx.permission_mode {
            ToolPermissionMode::FullAuto => {
                tracing::info!("Gmail send: auto-approved (full-auto mode)");
                Ok(())
            }
            ToolPermissionMode::ApproveDestructive | ToolPermissionMode::ApproveAll => {
                Err(ThresholdError::ToolError {
                    tool: "gmail".into(),
                    message: format!(
                        "Email sending requires full-auto permission mode. \
                         Current mode: {:?}. Either change tools.permission_mode \
                         to 'full-auto' or send the email manually.",
                        ctx.permission_mode
                    ),
                })
            }
        }
    }
}
```

### Design Note: No Silent Bypass

In non-FullAuto permission modes, Gmail send/reply is **blocked, not
silently allowed.** This is a deliberate choice — the permission model
must mean what it says.

Future enhancement: when the conversation engine supports confirmation
requests (emit a "please confirm" event to the portal, wait for user
response), we can allow send/reply with explicit Discord approval in
ApproveDestructive mode. But until that mechanism exists, we block rather
than pretend to gate.
```

### Audit Trail

All Gmail actions are logged, with send/reply actions logged at a **higher
prominence level**:

```json
{"ts":"...","tool":"gmail","action":"send","to":"bob@example.com","subject":"Re: Project deadline","agent":"default","conversation":"abc","portal":"discord-123","duration_ms":1200,"success":true}
```

---

## Configuration

```toml
[tools.gmail]
enabled = true
inboxes = ["personal@gmail.com", "work@company.com"]
allow_send = false    # Must be explicitly enabled
```

- `inboxes` — which email accounts the AI can access
- `allow_send` — separate toggle for sending (read access doesn't require this)

---

## Crate Module Structure

```
crates/gmail/src/
  lib.rs            — re-exports GmailTool
  client.rs         — GmailClient (Google API wrapper)
  tool.rs           — GmailTool implementing the Tool trait
  types.rs          — MessageSummary, EmailMessage, etc.
```

---

## Verification Checklist

- [ ] Unit test: message summary formatting
- [ ] Unit test: email message parsing
- [ ] Unit test: send permission gate — blocks when allow_send = false
- [ ] Unit test: send permission gate — allows in FullAuto mode
- [ ] Unit test: send permission gate — warns in ApproveDestructive mode
- [ ] Integration test (with Google API key): list messages from inbox
- [ ] Integration test: search messages with query
- [ ] Integration test: read a specific message by ID
- [ ] Integration test: tool is blocked when config has `enabled = false`
- [ ] Integration test: audit trail records all Gmail actions
- [ ] Integration test: send actions have elevated audit logging
