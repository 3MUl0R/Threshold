# Milestone 8 — Browser Automation

**Crate:** `browser`
**Complexity:** Medium
**Dependencies:** Milestone 5 (tool framework — CLI binary skeleton)

## Architecture Note: CLI Subcommands

> Per the CLI-based tool architecture (see Milestone 5), browser automation is
> exposed as `threshold browser <action>` CLI subcommands rather than a Tool
> trait implementation. Claude invokes browser commands via its native shell
> execution capability.
>
> ```
> Claude needs to take a screenshot:
>     → exec("threshold browser screenshot --session dev")
>     → stdout returns file path or base64 image data
>     → Claude reads the output naturally
> ```
>
> The internal Playwright integration logic remains the same — only the
> interface changes from Tool trait to clap subcommands with JSON stdout.

## What This Milestone Delivers

Playwright CLI integration as `threshold browser` subcommands. The AI can
browse the web, interact with pages, take screenshots, fill forms, and manage
persistent browser sessions. Disabled by default for security. Network origin
filtering restricts which domains the AI can access.

### Use Cases

- **End-to-end testing** — validate web apps the AI is building
- **Web research** — browse and interact with complex web pages
- **Form filling** — automate repetitive web forms
- **Price monitoring** — check product pages on a schedule (via cron)
- **Screenshot capture** — see the visual result of web development work

---

## Phase 8.1 — Browser CLI Subcommands

### `threshold browser` Command Tree

```
threshold browser
  open [url]          Open a browser session (optionally navigate to URL)
  goto <url>          Navigate to URL in current session
  close               Close the browser session
  click <ref>         Click an element
  fill <ref> <text>   Fill a form field
  type <text>         Type text
  select <ref> <val>  Select dropdown value
  screenshot [ref]    Take screenshot (full page or element)
  pdf                 Export page as PDF
  tab-list            List open tabs
  tab-new [url]       Open new tab
  tab-close [idx]     Close tab
  tab-select <idx>    Switch to tab
  --session <name>    Named browser session (default: "default")
  --format json|text  Output format (default: json)
```

### Implementation (`crates/browser/src/cli.rs`)

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
pub struct BrowserArgs {
    #[command(subcommand)]
    pub command: BrowserCommands,
    #[arg(long, default_value = "default")]
    pub session: String,
    #[arg(long, default_value = "json")]
    pub format: OutputFormat,
}

#[derive(Subcommand)]
pub enum BrowserCommands {
    Open { url: Option<String> },
    Goto { url: String },
    Close,
    Click { selector: String },
    Fill { selector: String, text: String },
    Screenshot { selector: Option<String> },
    // ... etc
}

pub async fn handle_browser_command(args: BrowserArgs) -> Result<()> {
    let config = load_browser_config()?;
    let session_mgr = BrowserSessionManager::new();

    match args.command {
        BrowserCommands::Open { url } => {
            let mut cmd = Command::new("playwright-cli");
            cmd.arg("-s").arg(&args.session).arg("open");
            if let Some(url) = &url {
                cmd.arg(url);
            }
            let output = execute_with_timeout(&mut cmd, 60).await?;
            session_mgr.track_session(&args.session).await;
            print_output(&output, &args.format);
        }
        // ... other commands follow same pattern
    }

    Ok(())
}
```

Each command outputs JSON to stdout for Claude to parse, with optional
`--format text` for human readability.

### Command Reference

| Category | Commands | Description |
|----------|----------|-------------|
| **Navigation** | `open [url]`, `goto <url>`, `close` | Browser and page control |
| **Interaction** | `click <ref>`, `fill <ref> <text>`, `type <text>`, `select <ref> <val>`, `check`, `uncheck`, `hover <ref>`, `drag <from> <to>` | Page interaction |
| **Capture** | `screenshot [ref]`, `pdf` | Visual capture |
| **Storage** | `cookie-*`, `state-save`, `state-load` | Browser state |
| **Tabs** | `tab-list`, `tab-new [url]`, `tab-close [idx]`, `tab-select <idx>` | Tab management |
| **Dev tools** | `console [level]`, `network` | Debugging |

---

## Phase 8.2 — Configuration

### Playwright CLI Config Generation

Generate `playwright-cli.json` from Threshold's config:

```rust
/// Default allowed origins — localhost only (default-deny).
const DEFAULT_ALLOWED_ORIGINS: &[&str] = &["http://localhost:*"];

/// Write playwright-cli.json configuration. This is a synchronous function
/// because it runs once at startup, not in a hot path.
pub fn write_playwright_config(config: &BrowserToolConfig, path: &Path) -> Result<()> {
    // Default-deny: if no allowed_origins configured, restrict to localhost only.
    // An explicitly empty list in config means "allow nothing" (strictest).
    // To allow all origins, the user must explicitly set allowed_origins = ["*"].
    let allowed_origins = config.allowed_origins.clone().unwrap_or_else(|| {
        DEFAULT_ALLOWED_ORIGINS.iter().map(|s| s.to_string()).collect()
    });

    let playwright_config = json!({
        "browser": "chromium",
        "launchOptions": {
            "headless": config.headless.unwrap_or(true)
        },
        "contextOptions": {
            "viewport": { "width": 1280, "height": 720 }
        },
        "actionTimeout": 5000,
        "navigationTimeout": 60000,
        "network": {
            "allowedOrigins": allowed_origins,
            "blockedOrigins": config.blocked_origins.clone().unwrap_or_default()
        }
    });

    std::fs::write(path, serde_json::to_string_pretty(&playwright_config)?)?;
    Ok(())
}
```

### Security Configuration

Network origin filtering is the primary security control:

```toml
[tools.browser]
enabled = false                  # Disabled by default — explicit opt-in
headless = true                  # No visible browser window
allowed_origins = [              # Default-deny: only listed origins are accessible
    "http://localhost:*",        # Local dev servers
]
blocked_origins = []             # Additional deny-list (applied after allow check)
```

**Security model: default-deny.**
- **Not set** (field omitted): defaults to `["http://localhost:*"]` — only local dev servers
- **Non-empty list** (e.g., `["https://example.com"]`): only listed origins accessible
- **Empty list** (`allowed_origins = []`): no origins accessible (strictest)
- **Wildcard** (`allowed_origins = ["*"]`): all origins (use with extreme caution)

`blocked_origins` is a second-pass filter applied after the allow check.

---

## Phase 8.3 — Session Management

### `crates/browser/src/sessions.rs`

```rust
pub struct BrowserSessionManager {
    active_sessions: Arc<RwLock<HashSet<String>>>,
}

impl BrowserSessionManager {
    pub fn new() -> Self;

    /// Track that a session has been opened.
    pub async fn track_session(&self, name: &str);

    /// Close a specific session.
    pub async fn close_session(&self, name: &str) -> Result<()>;

    /// Close all active sessions (called on shutdown).
    pub async fn close_all(&self) -> Result<()> {
        let sessions = self.active_sessions.read().await;
        for session in sessions.iter() {
            Command::new("playwright-cli")
                .args(["-s", session, "close"])
                .output()
                .await
                .ok();
        }
        Ok(())
    }
}
```

### Session Lifecycle

1. Claude runs: `threshold browser open https://localhost:3000 --session dev-testing`
2. Handler spawns `playwright-cli -s dev-testing open https://localhost:3000`
3. Session manager tracks "dev-testing" as active
4. Subsequent commands reuse the session: `threshold browser screenshot --session dev-testing`
5. On shutdown, `close_all()` cleans up all active sessions

---

## Crate Module Structure

```
crates/browser/src/
  lib.rs            — re-exports public API, BrowserSessionManager
  cli.rs            — clap subcommand definitions and handler
  config.rs         — playwright-cli.json generation
  sessions.rs       — session tracking and cleanup
```

---

## Verification Checklist

- [ ] Unit test: command construction for each action type
- [ ] Unit test: args parsing (with quotes, spaces)
- [ ] Unit test: playwright config JSON generation
- [ ] Unit test: session tracking (add, remove, close_all)
- [ ] Integration test (requires playwright-cli): open page, take screenshot
- [ ] Integration test: screenshot returns JSON with file path
- [ ] Integration test: session persistence across multiple commands
- [ ] Integration test: tool is blocked when config has `enabled = false`
- [ ] Integration test: network filtering blocks disallowed origins
- [ ] Integration test: sessions are cleaned up on shutdown
