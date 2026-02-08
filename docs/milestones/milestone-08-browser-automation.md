# Milestone 8 — Browser Automation

**Crate:** `browser`
**Complexity:** Medium
**Dependencies:** Milestone 5 (tool framework)

## What This Milestone Delivers

Playwright CLI integration as a native tool. The AI can browse the web,
interact with pages, take screenshots, fill forms, and manage persistent
browser sessions. Disabled by default for security. Network origin filtering
restricts which domains the AI can access.

### Use Cases

- **End-to-end testing** — validate web apps the AI is building
- **Web research** — browse and interact with complex web pages
- **Form filling** — automate repetitive web forms
- **Price monitoring** — check product pages on a schedule (via cron)
- **Screenshot capture** — see the visual result of web development work

---

## Phase 8.1 — Playwright CLI Tool

### `crates/browser/src/tool.rs`

```rust
pub struct BrowserTool {
    config: BrowserToolConfig,
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str { "browser" }

    fn description(&self) -> &str {
        "Control a web browser via Playwright CLI. Navigate pages, click elements, \
         fill forms, take screenshots, and manage browser sessions."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "open", "goto", "close",
                        "click", "fill", "type", "select", "check", "uncheck",
                        "hover", "press", "drag",
                        "screenshot", "pdf",
                        "cookie-list", "cookie-get", "cookie-set", "cookie-delete",
                        "state-save", "state-load",
                        "tab-list", "tab-new", "tab-close", "tab-select",
                        "console", "network"
                    ],
                    "description": "Browser action to perform"
                },
                "args": {
                    "type": "string",
                    "description": "Arguments for the action (URL, element ref, text, etc.)"
                },
                "session": {
                    "type": "string",
                    "description": "Named browser session (default: 'default')"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let action = params["action"].as_str()
            .ok_or_else(|| tool_error("Missing 'action' parameter"))?;
        let args = params["args"].as_str().unwrap_or("");
        let session = params["session"].as_str().unwrap_or("default");

        // Build command
        let mut cmd = Command::new("playwright-cli");
        cmd.arg("-s").arg(session);
        cmd.arg(action);
        if !args.is_empty() {
            // Split args respecting quotes
            cmd.args(shell_words::split(args)?);
        }

        // Execute with timeout
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            cmd.output(),
        ).await
        .map_err(|_| tool_error("Browser action timed out after 60s"))??;

        // Handle artifacts (screenshots)
        let mut artifacts = vec![];
        if action == "screenshot" || action == "pdf" {
            if let Some(path) = extract_file_path(&output.stdout) {
                let data = tokio::fs::read(&path).await?;
                let mime = if action == "pdf" { "application/pdf" } else { "image/png" };
                artifacts.push(Artifact {
                    name: path.file_name().unwrap().to_string_lossy().to_string(),
                    data,
                    mime_type: mime.to_string(),
                });
            }
        }

        Ok(ToolResult {
            content: String::from_utf8_lossy(&output.stdout).to_string(),
            artifacts,
            success: output.status.success(),
        })
    }
}
```

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
/// Write playwright-cli.json configuration. This is a synchronous function
/// because it runs once at startup, not in a hot path.
pub fn write_playwright_config(config: &BrowserToolConfig, path: &Path) -> Result<()> {
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
            "allowedOrigins": config.allowed_origins.clone().unwrap_or_default(),
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
allowed_origins = []             # Empty = allow all (use with caution)
blocked_origins = [              # Block known tracking/ad domains
    "https://ads.example.com",
    "https://tracking.example.com",
]
```

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

1. AI requests `browser` tool with `action: "open"` and `session: "dev-testing"`
2. BrowserTool spawns `playwright-cli -s dev-testing open https://localhost:3000`
3. Session manager tracks "dev-testing" as active
4. Subsequent tool calls can reuse the session: `screenshot`, `click`, etc.
5. On shutdown, `close_all()` cleans up all active sessions

---

## Crate Module Structure

```
crates/browser/src/
  lib.rs            — re-exports BrowserTool, BrowserSessionManager
  tool.rs           — BrowserTool implementing the Tool trait
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
- [ ] Integration test: screenshot returns image as artifact
- [ ] Integration test: session persistence across multiple commands
- [ ] Integration test: tool is blocked when config has `enabled = false`
- [ ] Integration test: network filtering blocks disallowed origins
- [ ] Integration test: sessions are cleaned up on shutdown
