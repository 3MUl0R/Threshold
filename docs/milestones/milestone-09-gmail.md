# Milestone 9 — Gmail Integration

**Crate:** `gmail`
**Complexity:** Medium
**Dependencies:** Milestone 1 (core, secrets), Milestone 5 (tool framework — CLI binary skeleton)

## Architecture Note: CLI Subcommands

> Per the CLI-based tool architecture (see Milestone 5), Gmail is exposed as
> `threshold gmail <action>` CLI subcommands rather than a Tool trait
> implementation. Claude invokes Gmail commands via its native shell execution.
>
> ```
> Claude needs to check email:
>     → exec("threshold gmail list --inbox user@gmail.com --max 10")
>     → stdout returns JSON array of message summaries
>     → Claude reads and summarizes naturally
>
> Claude needs to send a reply:
>     → exec("threshold gmail reply MSG_ID --body '...'")
>     → stdout confirms send (or error)
> ```
>
> The Gmail API client, OAuth flow, and permission gating remain unchanged
> from the designs below. Only the interface changes from Tool trait to clap
> subcommands. The `threshold gmail auth` command handles the interactive
> OAuth setup flow.
>
> **Audit logging:** Each CLI subcommand writes its own audit entry via
> `AuditTrail` before returning output. Send/reply actions are logged at
> elevated prominence.

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
2. User runs `threshold gmail auth --inbox user@gmail.com` for each inbox, which:
   a. Opens a browser to Google's consent screen
   b. User grants read/send permissions for that account
   c. Callback captures the authorization code
   d. Exchanges code for access + refresh tokens
   e. Stores tokens in OS keychain, namespaced by inbox
      (`gmail-oauth-access-token-user@gmail.com`,
       `gmail-oauth-refresh-token-user@gmail.com`)
3. On subsequent API calls, the client resolves the inbox-specific access
   token. If expired, it uses the inbox-specific refresh token automatically.

```rust
pub struct GmailAuth {
    secret_store: Arc<SecretStore>,
    inbox: String,
    client_id: String,
    client_secret: String,
}

impl GmailAuth {
    pub fn new(secret_store: Arc<SecretStore>, inbox: &str) -> Self;

    /// Get a valid access token for this inbox, refreshing if expired.
    pub async fn get_access_token(&self) -> Result<String>;

    /// Run the initial OAuth consent flow (interactive, opens browser).
    pub async fn authorize(&self) -> Result<()>;
}
```

**Keychain keys (namespaced per inbox):**
- `gmail-oauth-client-id` — Google OAuth client ID (shared across inboxes)
- `gmail-oauth-client-secret` — Google OAuth client secret (shared across inboxes)
- `gmail-oauth-access-token-{inbox}` — current access token for a specific inbox
- `gmail-oauth-refresh-token-{inbox}` — refresh token for a specific inbox (long-lived)

Where `{inbox}` is the email address (e.g., `gmail-oauth-access-token-user@gmail.com`).
Each inbox requires its own OAuth consent flow via `threshold gmail auth --inbox <email>`.

**Required scopes:**
- `https://www.googleapis.com/auth/gmail.readonly` — read access
- `https://www.googleapis.com/auth/gmail.send` — send access (only if
  `allow_send = true` in config)

---

## Phase 9.2 — Gmail CLI Subcommands

### `crates/gmail/src/cli.rs`

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
pub struct GmailArgs {
    #[command(subcommand)]
    pub command: GmailCommands,
}

#[derive(Subcommand)]
pub enum GmailCommands {
    /// Run OAuth setup flow (interactive, opens browser)
    Auth {
        #[arg(long)]
        inbox: String,
    },
    /// List recent messages from an inbox
    List {
        #[arg(long)]
        inbox: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long, default_value = "10")]
        max: u32,
    },
    /// Read a specific message by ID
    Read {
        #[arg(long)]
        inbox: String,
        /// Message ID
        id: String,
    },
    /// Search messages with Gmail search syntax
    Search {
        #[arg(long)]
        inbox: String,
        /// Gmail search query
        query: String,
        #[arg(long, default_value = "10")]
        max: u32,
    },
    /// Send a new email (requires allow_send = true)
    Send {
        #[arg(long)]
        inbox: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
    },
    /// Reply to an existing email (requires allow_send = true)
    Reply {
        #[arg(long)]
        inbox: String,
        /// Message ID to reply to
        id: String,
        #[arg(long)]
        body: String,
    },
}

pub async fn handle_gmail_command(args: GmailArgs) -> Result<()> {
    let config = load_gmail_config()?;
    let secret_store = Arc::new(SecretStore::new());
    let client = GmailClient::new(secret_store.clone());
    let audit = AuditTrail::new(/* ... */);

    match args.command {
        GmailCommands::Auth { inbox } => {
            let auth = GmailAuth::new(secret_store, &inbox);
            auth.authorize().await?;
            println!(r#"{{"status": "ok", "inbox": "{}", "message": "Gmail OAuth setup complete"}}"#, inbox);
        }
        GmailCommands::List { inbox, query, max } => {
            let messages = client.list_messages(&inbox, query.as_deref(), max).await?;
            audit.append_event("gmail", &json!({"action": "list", "inbox": inbox})).ok();
            println!("{}", serde_json::to_string_pretty(&messages)?);
        }
        GmailCommands::Send { inbox, to, subject, body } => {
            check_send_permission(&config)?;
            client.send(&inbox, &to, &subject, &body).await?;
            audit.append_event("gmail", &json!({
                "action": "send", "to": to, "subject": subject
            })).ok();
            println!(r#"{{"status": "ok", "message": "Email sent"}}"#);
        }
        // ... other commands follow same pattern
    }

    Ok(())
}
```

All subcommands output **JSON to stdout** by default.

---

## Phase 9.3 — Send Permission Gate

Sending email is a potentially destructive action. The permission system
controls whether the AI can send without confirmation.

```rust
/// Check send permission — called by send/reply subcommands before execution.
fn check_send_permission(config: &GmailToolConfig) -> Result<()> {
    if !config.allow_send.unwrap_or(false) {
        return Err(ThresholdError::ToolError {
            tool: "gmail".into(),
            message: "Email sending is disabled. Set tools.gmail.allow_send = true \
                      in config to enable it.".into(),
        });
    }
    Ok(())
}
```

### Design Note: No Silent Bypass

Send/reply subcommands check `allow_send` in the config before executing.
If disabled, the command exits with a JSON error and non-zero status code.

Future enhancement: a confirmation flow where the CLI prompts the daemon
to ask the user via Discord before proceeding with the send. But until
that mechanism exists, we gate at the config level.

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
  lib.rs            — re-exports public API
  cli.rs            — clap subcommand definitions and handler
  client.rs         — GmailClient (Google API wrapper)
  auth.rs           — GmailAuth (OAuth 2.0 flow, token refresh)
  types.rs          — MessageSummary, EmailMessage, etc.
```

---

## Verification Checklist

- [ ] Unit test: message summary formatting
- [ ] Unit test: email message parsing
- [ ] Unit test: send permission gate — blocks when allow_send = false
- [ ] Unit test: send permission gate — allows when allow_send = true
- [ ] Integration test (with OAuth credentials): list messages from inbox
- [ ] Integration test: search messages with query
- [ ] Integration test: read a specific message by ID
- [ ] Integration test: tool is blocked when config has `enabled = false`
- [ ] Integration test: audit trail records all Gmail actions
- [ ] Integration test: send actions have elevated audit logging
