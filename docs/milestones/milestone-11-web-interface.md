# Milestone 11 — Local Web Interface

**Crate:** `web`
**Complexity:** Large
**Dependencies:** Milestone 1 (core), Milestone 6 (scheduler — daemon API)

## What This Milestone Delivers

A localhost-only web dashboard for managing and monitoring the Threshold system.
Built with **Axum** (server-side rendering) and **htmx** (dynamic updates without
a JS build step). Runs as part of the daemon process — no separate server.

### Design Principles

1. **Localhost only** — binds to `127.0.0.1`, never `0.0.0.0`. Remote access
   requires the user to set up their own tunnel (SSH, Tailscale, etc.)
2. **Server-rendered** — HTML generated in Rust with htmx for interactivity.
   No Node.js, no JS build pipeline, no frontend framework.
3. **Read-first** — dashboard views ship first; management features follow.
4. **Single process** — the web server runs inside `threshold daemon`, sharing
   the same Tokio runtime, config, and cancellation token.
5. **Minimal dependencies** — Axum + Tower + askama (templates) + htmx (vendored).

### Use Cases

- See all active conversations and their portals at a glance
- Browse scheduled tasks, see next run times, toggle enable/disable
- Review audit logs (tool usage, Gmail, image generation)
- View and search application logs
- Manage credentials (add/remove API keys from keychain)
- Edit configuration without hand-editing TOML
- Check system status (daemon uptime, Discord connection, scheduler state)

---

## Architecture

### How It Fits Into the Daemon

The web server is just another concurrent task in `run_daemon()`, alongside
Discord and the scheduler:

```rust
// In crates/server/src/main.rs — run_daemon()
tokio::select! {
    r = discord_handle => { /* ... */ }
    r = scheduler_handle => { /* ... */ }
    r = web_handle => {                          // NEW
        if let Err(e) = r {
            tracing::error!("Web server error: {}", e);
        }
    }
    _ = tokio::signal::ctrl_c() => {
        tracing::info!("Shutdown signal received.");
    }
}
```

### Data Access

The web server reads the same data files that the CLI and daemon use:

| Data | Source | Access Method |
|------|--------|---------------|
| Conversations | `~/.threshold/state/conversations.json` | Read JSON file |
| Schedules | `~/.threshold/state/schedules.json` | Read JSON file + SchedulerHandle |
| Audit logs | `~/.threshold/audit/*.jsonl` | Read + parse JSONL |
| App logs | `~/.threshold/logs/threshold.log.*` | Read log files |
| Config | `~/.threshold/config.toml` | ThresholdConfig (already loaded) |
| Credentials | OS Keychain | SecretStore (check existence, not values) |
| CLI sessions | `~/.threshold/cli-sessions/*.json` | Read JSON files |

For **read-only views**, the web server reads files directly from disk.

For **mutations** (schedule toggle, schedule delete), the web server sends
commands through the existing `SchedulerHandle` channel — same as Discord
commands do. No new IPC mechanism needed.

For **credential management**, the web server uses `SecretStore` directly
(same in-process instance as the daemon).

### No New Portal Type (Yet)

Phase A of this milestone is a **management dashboard**, not a conversation
portal. It does not need `PortalType::Web` or WebSocket streaming. Adding
a web-based chat portal is a future stretch goal that would add:
- `PortalType::Web { session_id: String }` to types.rs
- WebSocket endpoint for streaming Claude responses
- Session token authentication

That's architecturally supported but out of scope for the initial dashboard.

---

## Phase 11.1 — Axum Server Skeleton + Static Assets

### New Crate: `crates/web/`

```
crates/web/
  Cargo.toml
  src/
    lib.rs              — public API: start_web_server()
    routes/
      mod.rs            — router construction
    templates/
      mod.rs            — askama template definitions
    state.rs            — shared application state (AppState)
    error.rs            — web error handling → HTML error pages
  templates/            — askama HTML templates
    base.html           — layout with nav, htmx script tag
    index.html          — dashboard home
  static/
    style.css           — minimal CSS (no framework, or classless like Pico CSS)
```

### AppState

Shared state injected into all Axum handlers:

```rust
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ThresholdConfig>,
    pub data_dir: PathBuf,
    pub scheduler_handle: Option<SchedulerHandle>,
    pub secret_store: Arc<SecretStore>,
    pub cancel: CancellationToken,
    pub start_time: chrono::DateTime<chrono::Utc>,
}
```

### Server Startup

```rust
/// Start the web server. Returns a future that runs until cancellation.
pub async fn start_web_server(state: AppState) -> anyhow::Result<()> {
    let app = build_router(state.clone());

    let bind = state.config.web.as_ref()
        .and_then(|w| w.bind.as_deref())
        .unwrap_or("127.0.0.1");
    let port = state.config.web.as_ref()
        .and_then(|w| w.port)
        .unwrap_or(3000);

    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse()?;
    tracing::info!("Web interface listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(state.cancel.cancelled_owned())
        .await?;

    Ok(())
}
```

### Configuration

Add to `ThresholdConfig`:

```rust
pub struct WebConfig {
    pub enabled: bool,
    pub bind: Option<String>,    // default: "127.0.0.1"
    pub port: Option<u16>,       // default: 3000
}
```

Config TOML:

```toml
[web]
enabled = true
bind = "127.0.0.1"
port = 3000
```

### Validation

The config validator ensures that if `bind` is set, it must be a loopback
address (`127.0.0.1` or `::1`) unless the user explicitly sets it. We log
a warning if the user binds to `0.0.0.0` since that exposes the interface
to the network.

### Static Assets + htmx

- **htmx** loaded from a vendored copy in `static/htmx.min.js` (no CDN
  dependency — this is a local-only tool)
- **CSS** — a small custom stylesheet, or a classless CSS library like
  Pico CSS vendored into `static/`
- Static files served via `tower_http::services::ServeDir`

### Templates (askama)

Server-rendered HTML using askama (compile-time template checking):

```rust
#[derive(Template)]
#[template(path = "base.html")]
struct BaseTemplate {
    title: String,
    nav_active: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    title: String,
    nav_active: String,
    uptime: String,
    conversation_count: usize,
    schedule_count: usize,
    discord_connected: bool,
}
```

### Deliverables

- [ ] `crates/web/` crate with Cargo.toml
- [ ] `WebConfig` added to `ThresholdConfig` with validation
- [ ] `start_web_server()` function that binds to localhost
- [ ] Base HTML template with navigation
- [ ] Static file serving (CSS, htmx.min.js)
- [ ] Index page with system status summary
- [ ] Web server integrated into `run_daemon()` as concurrent task
- [ ] `config.example.toml` updated with `[web]` section
- [ ] Graceful shutdown via CancellationToken

---

## Phase 11.2 — Conversations Dashboard

### Routes

```
GET /conversations                — list all conversations
GET /conversations/{id}           — conversation detail view
GET /conversations/{id}/audit     — audit trail for a conversation (htmx partial)
```

### Data Source

Read `~/.threshold/state/conversations.json` (the `ConversationStore` format):

```rust
async fn list_conversations(State(state): State<AppState>) -> impl IntoResponse {
    let path = state.data_dir.join("state").join("conversations.json");
    let conversations: HashMap<ConversationId, Conversation> = /* read + parse */;
    // Sort by last_active descending
    // Render template
}
```

### Conversation List View

Table showing:
- Mode (General / Coding / Research) with visual indicator
- Agent ID
- Created date
- Last active (relative time: "2 hours ago")
- Number of portals attached

### Conversation Detail View

- Conversation metadata (mode, agent, CLI provider, model)
- Attached portals list
- CLI session info (session ID, if present)
- Audit trail entries (loaded via htmx from `/conversations/{id}/audit`)

### Audit Trail Partial

htmx-loaded partial that reads the conversation's JSONL audit file and
displays entries in reverse chronological order. Supports:
- Pagination (load more via htmx `hx-get` with offset parameter)
- Filtering by event type

### Deliverables

- [ ] Conversations list page with sorting
- [ ] Conversation detail page
- [ ] Audit trail partial with htmx lazy-loading
- [ ] Relative time formatting ("2 hours ago")
- [ ] Mode-specific styling (color-coded badges)

---

## Phase 11.3 — Schedules Dashboard

### Routes

```
GET  /schedules                   — list all scheduled tasks
POST /schedules/{id}/toggle       — toggle enabled/disabled (htmx)
POST /schedules/{id}/delete       — delete a task (htmx)
```

### Data Source

For **reads**: parse `~/.threshold/state/schedules.json` directly.

For **mutations** (toggle, delete): use the `SchedulerHandle` command channel
that's already shared with Discord commands. This ensures the in-memory
scheduler state stays consistent.

```rust
async fn toggle_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let handle = state.scheduler_handle.as_ref()
        .ok_or(WebError::SchedulerNotRunning)?;
    handle.toggle_task(&id).await?;
    // Return htmx partial with updated row
}
```

### Schedule List View

Table showing:
- Task name
- Cron expression (human-readable description alongside)
- Action type (NewConversation / Script / etc.)
- Enabled/disabled toggle (htmx `hx-post`)
- Next run time
- Last run time + result
- Delete button (with confirmation)

### Deliverables

- [ ] Schedule list page
- [ ] Toggle enabled/disabled via htmx POST
- [ ] Delete task via htmx POST (with confirmation dialog)
- [ ] Cron expression display with human-readable description
- [ ] Next/last run time display

---

## Phase 11.4 — Audit Log Browser

### Routes

```
GET /audit                        — audit log browser
GET /audit/tools                  — tool execution audit (htmx partial)
GET /audit/gmail                  — Gmail audit (htmx partial)
GET /audit/imagegen               — image generation audit (htmx partial)
```

### Data Source

Read JSONL files from `~/.threshold/audit/`:
- `tools.jsonl` — tool execution log
- `gmail.jsonl` — Gmail access log
- Other `*.jsonl` files as they exist

### Audit Browser View

- Tabbed interface (htmx tab switching, no page reload)
- Each tab loads its JSONL file and displays entries in reverse chronological order
- Columns: timestamp, tool/action, parameters (truncated), duration, success/failure
- Expandable rows to see full parameters (htmx `hx-get` for detail)
- Pagination (newest first, load older entries on scroll/button)
- Optional date range filter

### Deliverables

- [ ] JSONL file parser (streaming, handles large files efficiently)
- [ ] Tabbed audit browser with htmx tab switching
- [ ] Paginated reverse-chronological display
- [ ] Expandable detail rows
- [ ] Date range filtering

---

## Phase 11.5 — Log Viewer

### Routes

```
GET /logs                         — log viewer page
GET /logs/entries                 — log entries (htmx partial, paginated)
```

### Data Source

Read log files from `~/.threshold/logs/threshold.log.*`.
Logs are in JSON format (one JSON object per line from tracing-subscriber).

### Log Viewer Features

- File selector (log files listed by date)
- Level filtering (trace / debug / info / warn / error)
- Text search within log entries
- Auto-scroll with htmx polling (`hx-trigger="every 5s"`) for live tail
- Color-coded log levels

### Deliverables

- [ ] Log file listing and selection
- [ ] Level filtering
- [ ] Text search
- [ ] Live tail mode with htmx polling
- [ ] Color-coded severity levels

---

## Phase 11.6 — Configuration Management

### Routes

```
GET  /config                      — config viewer/editor
POST /config                      — save config changes
GET  /config/credentials          — credential status page
POST /config/credentials/{key}    — add/update a credential
POST /config/credentials/{key}/delete — remove a credential
```

### Config Editor

Display the current `config.toml` content in an editable form. Not a raw
text editor — structured form fields grouped by section:

| Section | Fields |
|---------|--------|
| General | `log_level`, `data_dir` |
| CLI | `command`, `model`, `timeout_seconds`, `skip_permissions` |
| Discord | `guild_id`, `allowed_user_ids` |
| Web | `bind`, `port` |
| Tools | `permission_mode`, tool enable/disable toggles |
| Heartbeat | `enabled`, `interval_minutes`, `instruction_file` |
| Scheduler | `enabled`, `store_path` |

**Save flow:**
1. Validate new config (parse as TOML, run ThresholdConfig::validate())
2. Write to `config.toml` (atomic write: write tmp, rename)
3. Display success message
4. Note: changes take effect on next daemon restart (no hot-reload for v1)

**Redaction:** API keys and tokens are NEVER displayed in the config editor.
Fields like `discord.bot_token` don't exist in the config file (they're in
keychain), so this happens naturally. But we should ensure no accidental
secret display.

### Credential Manager

Shows which credentials are configured (exist in keychain) without revealing
their values:

| Credential | Status | Actions |
|------------|--------|---------|
| discord-bot-token | Configured | [Update] [Remove] |
| google-api-key | Not configured | [Add] |
| gmail-oauth-client-id | Configured | [Update] [Remove] |
| gmail-oauth-refresh-token-user@gmail.com | Configured | [Update] [Remove] |

**Add/Update flow:**
1. Form with credential name (pre-filled) and value (password input)
2. POST stores value via `SecretStore::store()`
3. Value is NEVER echoed back — only status (configured / not configured)

**Remove flow:**
1. Confirm dialog
2. POST removes via `SecretStore::delete()`

### Deliverables

- [ ] Structured config form grouped by section
- [ ] Config validation before save
- [ ] Atomic config file write
- [ ] Credential status page (configured / not configured)
- [ ] Add/update credential form
- [ ] Remove credential with confirmation
- [ ] No secret values ever in HTML responses

---

## Phase 11.7 — System Status & Polish

### Routes

```
GET /                             — dashboard home (enhanced from 11.1)
GET /status                       — system status API (JSON, for htmx polling)
```

### Enhanced Dashboard

The index page becomes a proper status dashboard with htmx polling:

- **Uptime** — daemon start time and duration
- **Discord** — connected / disconnected, guild name
- **Scheduler** — running / stopped, number of active tasks, next task due
- **Conversations** — total count, active count (last activity < 1hr)
- **Web server** — bound address, request count
- **Quick links** — to conversations, schedules, audit, config

### Status Polling

htmx polls `/status` every 10 seconds to keep the dashboard current:

```html
<div hx-get="/status" hx-trigger="every 10s" hx-swap="innerHTML">
    <!-- status content refreshed automatically -->
</div>
```

### UI Polish

- Responsive layout (works on mobile browsers too)
- Consistent navigation across all pages
- Flash messages for success/error feedback on mutations
- Loading indicators for htmx requests
- Empty states ("No conversations yet", "No scheduled tasks")

### Deliverables

- [ ] Enhanced dashboard with live status polling
- [ ] Responsive CSS layout
- [ ] Flash message system for form feedback
- [ ] Loading indicators
- [ ] Empty state handling
- [ ] Navigation breadcrumbs

---

## Crate Module Structure

```
crates/web/
  Cargo.toml
  src/
    lib.rs              — start_web_server(), public API
    state.rs            — AppState definition
    error.rs            — WebError → HTML error responses
    routes/
      mod.rs            — build_router(), mount all route groups
      index.rs          — GET / (dashboard)
      conversations.rs  — conversation routes
      schedules.rs      — schedule routes
      audit.rs          — audit log routes
      logs.rs           — log viewer routes
      config.rs         — config editor routes
      credentials.rs    — credential management routes
      status.rs         — GET /status (JSON for htmx polling)
    templates/
      mod.rs            — askama template structs
    helpers/
      mod.rs            — shared utilities
      time.rs           — relative time formatting
      jsonl.rs          — JSONL file parser with pagination
  templates/            — askama HTML templates (*.html)
    base.html
    index.html
    conversations/
      list.html
      detail.html
      audit_partial.html
    schedules/
      list.html
      row_partial.html
    audit/
      browser.html
      tab_partial.html
      detail_partial.html
    logs/
      viewer.html
      entries_partial.html
    config/
      editor.html
      credentials.html
    error.html
    partials/
      nav.html
      flash.html
  static/
    htmx.min.js         — vendored htmx (no CDN)
    style.css            — custom styles
```

---

## Dependencies

```toml
[dependencies]
axum = "0.8"
tower = "0.5"
tower-http = { version = "0.6", features = ["fs", "trace"] }
askama = "0.12"
askama_axum = "0.4"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"
uuid = { version = "1", features = ["serde"] }

# Internal
threshold-core = { path = "../core" }
threshold-scheduler = { path = "../scheduler" }
```

Note: exact versions of `axum`, `askama`, and `askama_axum` should be
verified against crates.io at implementation time — the Axum ecosystem
moves quickly and we want compatible versions.

---

## Configuration Section (config.example.toml)

```toml
# ── Web Interface ──
[web]
enabled = true
bind = "127.0.0.1"       # NEVER set to 0.0.0.0 unless you understand the risk
port = 3000
```

---

## Security Considerations

1. **Localhost binding** — default `127.0.0.1`. The config validator warns
   (but does not block) if the user sets `0.0.0.0`.
2. **No authentication for v1** — localhost-only means the user's OS login
   is the auth boundary. Future: add session tokens if remote access is
   supported.
3. **No secret display** — credentials are never included in HTML responses.
   The credential manager shows "Configured" / "Not configured" status only.
4. **CSRF protection** — POST routes use a simple token-based CSRF guard
   (hidden form field checked on submission).
5. **Config write safety** — atomic writes (write to tmp file, then rename)
   prevent corruption on crash.
6. **Input validation** — all form inputs validated server-side before use.
   TOML config is parsed and validated before writing.

---

## Testing Strategy

### Unit Tests

- Template rendering (askama compiles templates at build time — compilation
  is the test)
- JSONL parser: handles empty files, malformed lines, large files
- Relative time formatting: "just now", "5 minutes ago", "2 hours ago", etc.
- Config form → TOML serialization round-trip

### Integration Tests

- Axum test client (`axum::test::TestClient`) for each route group:
  - GET routes return 200 with expected content
  - POST routes perform mutations and return appropriate responses
  - Error cases return proper error pages
- Schedule toggle/delete via web matches SchedulerHandle behavior
- Config save + reload produces valid config

### E2E Tests (Playwright CLI)

- Navigate dashboard, verify status elements present
- Click through conversations, schedules, audit tabs
- Toggle a schedule, verify state change
- Submit config changes, verify persistence
- Add/remove credentials, verify status updates
- Test responsive layout at different viewport sizes

```bash
# Example Playwright CLI test flow
playwright-cli open http://127.0.0.1:3000
playwright-cli screenshot --full-page
playwright-cli click "text=Conversations"
playwright-cli screenshot --name conversations
playwright-cli click "text=Schedules"
playwright-cli click "[data-action=toggle]"  # Toggle a schedule
playwright-cli screenshot --name schedule-toggled
```

---

## Verification Checklist

### Phase 11.1 — Server Skeleton
- [ ] `cargo build -p threshold-web` succeeds
- [ ] Web server starts on `127.0.0.1:3000` as part of daemon
- [ ] Index page renders with basic status info
- [ ] Static files (CSS, htmx.js) served correctly
- [ ] Graceful shutdown works (CancellationToken)
- [ ] Config validation rejects non-loopback bind addresses with warning

### Phase 11.2 — Conversations
- [ ] Conversations list page loads with real data
- [ ] Conversation detail page shows metadata
- [ ] Audit trail loads via htmx
- [ ] Empty state handled ("No conversations yet")

### Phase 11.3 — Schedules
- [ ] Schedule list page loads with real data
- [ ] Toggle enable/disable via htmx works
- [ ] Delete via htmx works (with confirmation)
- [ ] Mutations go through SchedulerHandle (not direct file writes)

### Phase 11.4 — Audit Logs
- [ ] Audit browser loads JSONL files
- [ ] Tab switching works via htmx
- [ ] Pagination handles large files
- [ ] Expandable detail rows

### Phase 11.5 — Log Viewer
- [ ] Log files listed by date
- [ ] Level filtering works
- [ ] Live tail mode with htmx polling
- [ ] Text search works

### Phase 11.6 — Configuration
- [ ] Config form renders current values
- [ ] Config save validates and writes atomically
- [ ] Credential status page shows correct states
- [ ] Add/update credentials works
- [ ] Remove credentials works
- [ ] No secrets ever in HTML

### Phase 11.7 — Status & Polish
- [ ] Dashboard polls for live status updates
- [ ] Responsive layout works on mobile viewports
- [ ] Flash messages appear for mutations
- [ ] All pages have consistent navigation
- [ ] E2E tests pass via Playwright CLI
