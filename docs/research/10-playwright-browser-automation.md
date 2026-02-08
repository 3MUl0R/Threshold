# Playwright CLI & Browser Automation — Integration Plan

## Summary

Microsoft provides **two** Playwright-based tools for AI agents:

1. **Playwright CLI** (`@playwright/cli`) — standalone command-line tool designed
   specifically for coding agents. Token-efficient, session-persistent, simple
   subprocess commands. **This is our recommended integration.**

2. **Playwright MCP** (`@playwright/mcp`) — MCP server with 70+ tools using
   accessibility tree snapshots. Higher context cost, richer introspection.

OpenClaw has its own custom browser tool (Docker-sandboxed, screenshot-based),
but doesn't use either Playwright tool. For our project, Playwright CLI is
the clear winner.

---

## Playwright CLI (Recommended)

**Repo**: https://github.com/microsoft/playwright-cli
**Package**: `@playwright/cli`
**License**: Apache 2.0

### Why CLI Over MCP

| | Playwright MCP | Playwright CLI |
|---|---|---|
| Interface | MCP protocol (JSON-RPC) | Shell commands |
| Context cost | **High** — 70+ tool schemas + accessibility trees | **Low** — concise commands |
| State | Stateless per-call | **Persistent sessions** across commands |
| Best for | Rich introspection, autonomous workflows | Coding agents, high-throughput, limited context |
| Integration | Requires MCP client | Simple subprocess spawn |

The CLI was explicitly designed for the AI coding agent use case — it avoids
forcing page data into model context, keeping token costs low.

### Installation

```bash
npm install -g @playwright/cli@latest
playwright-cli install --skills    # Install browser + skills support
```

### Core Commands

**Navigation & Page Control:**
```bash
playwright-cli open [url]           # Launch browser, optionally navigate
playwright-cli goto <url>           # Navigate to URL
playwright-cli close                # Close page
playwright-cli screenshot [ref]     # Capture page or element
playwright-cli pdf                  # Generate PDF
```

**Interaction:**
```bash
playwright-cli click <ref> [button] # Click elements
playwright-cli type <text>          # Enter text into editable fields
playwright-cli fill <ref> <text>    # Fill form fields
playwright-cli select <ref> <val>   # Choose dropdown options
playwright-cli check / uncheck      # Toggle checkboxes
playwright-cli drag <start> <end>   # Drag and drop
playwright-cli hover <ref>          # Hover over elements
```

**Storage Management:**
```bash
playwright-cli cookie-list / cookie-get / cookie-set / cookie-delete
playwright-cli localstorage-list / localstorage-get / localstorage-set
playwright-cli sessionstorage-list / sessionstorage-get / sessionstorage-set
playwright-cli state-save [filename]    # Persist browser state
playwright-cli state-load <filename>    # Restore browser state
```

**Keyboard & Mouse:**
```bash
playwright-cli press <key>          # Press keyboard key
playwright-cli keydown / keyup      # Key state control
playwright-cli mousemove <x> <y>    # Move cursor
playwright-cli mousedown / mouseup  # Mouse button control
playwright-cli mousewheel <dx> <dy> # Scroll
```

**Developer Tools:**
```bash
playwright-cli console [min-level]  # View console messages
playwright-cli network              # List network requests
playwright-cli tracing-start / tracing-stop   # Record traces
playwright-cli video-start / video-stop       # Capture video
```

**Tab Management:**
```bash
playwright-cli tab-list             # Show all tabs
playwright-cli tab-new [url]        # New tab
playwright-cli tab-close [index]    # Close tab
playwright-cli tab-select <index>   # Switch tab
```

**Network Mocking:**
```bash
playwright-cli route <pattern> [opts]  # Mock network requests
playwright-cli route-list              # View active routes
playwright-cli unroute [pattern]       # Remove routes
```

### Session Persistence

Sessions are named browser profiles that persist across commands:

```bash
playwright-cli open https://example.com              # Default session
playwright-cli -s=shopping open https://amazon.com    # Named session
playwright-cli list                                   # Show all sessions
```

The `PLAYWRIGHT_CLI_SESSION` env var sets the default session for a process.
This is ideal for our use case — the AI can maintain a browser session across
multiple tool calls without re-navigating.

### Configuration

`playwright-cli.json` in project root:

```json
{
  "browser": "chromium",
  "launchOptions": {
    "headless": true
  },
  "contextOptions": {
    "viewport": { "width": 1280, "height": 720 }
  },
  "actionTimeout": 5000,
  "navigationTimeout": 60000,
  "network": {
    "allowedOrigins": ["https://example.com"],
    "blockedOrigins": ["https://ads.example.com"]
  }
}
```

Key config options:
- **Browser selection**: Chromium, Firefox, WebKit
- **Headless/headed mode**: invisible or visual
- **Network filtering**: allowed/blocked origins (security!)
- **Timeouts**: action (5s default), navigation (60s default)
- **CDP endpoint**: connect to remote/existing browsers
- **Init scripts**: run JS/TS on page load

### Operational Modes

- **Headless** (default) — invisible automation
- **Headed** — visual browser (`--headed` flag)
- **Persistent profiles** — browser state survives between commands
- **Isolated profiles** — in-memory, no disk persistence
- **Remote connections** — connect to CDP endpoints or Playwright servers

---

## Playwright MCP (Alternative)

**Package**: `@playwright/mcp`

Included here for reference. The MCP approach is valid but higher-overhead:

- 70+ tools organized into 7 capability groups
- Uses accessibility tree snapshots (structured DOM data)
- Non-core capabilities (vision, PDF, testing, tracing) require opt-in
- Available as npm, Docker, or browser extension
- Better suited for autonomous workflows where context cost matters less

For our project, the CLI approach is preferred because:
1. Lower token cost per interaction
2. Session persistence without extra infrastructure
3. Simpler Rust integration (subprocess vs MCP client)
4. Network origin filtering built into config

---

## How OpenClaw Does Browser Automation (For Comparison)

OpenClaw's browser tool is custom-built:

- **Docker sandboxed** — browser runs in container
- **Three proxy modes**: sandbox, host, or connected nodes
- **Policy controlled**: `gateway.nodes.browser.mode` = auto/manual/off
- **Screenshot-based** — captures page images for the AI

More complex and less capable than either Playwright tool.

---

## Takeaways for Our Project

### Recommended integration: Playwright CLI as subprocess

```rust
struct PlaywrightBrowserTool {
    session_name: String,
    config_path: PathBuf,
}

impl Tool for PlaywrightBrowserTool {
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        // Parse the AI's requested action
        let action = params["action"].as_str()?;  // "goto", "click", etc.
        let args = params["args"].as_str()?;       // URL, element ref, etc.

        // Spawn playwright-cli subprocess
        let output = Command::new("playwright-cli")
            .args(["-s", &self.session_name, action, args])
            .output()
            .await?;

        Ok(ToolResult {
            content: String::from_utf8(output.stdout)?,
            artifacts: vec![],
        })
    }
}
```

The AI agent calls this as a single tool with action/args parameters.
Playwright CLI handles browser lifecycle, session state, and rendering.

### Security considerations

- **Default: disabled** — user must explicitly enable in config
- **Network origin filtering** — use `playwright-cli.json` `allowedOrigins`
  to restrict which domains the AI can browse
- **No credential auto-fill** — never pass stored passwords to the browser
- **Local only** — browser runs on the home server, not remotely
- **Headless default** — no visible browser window unless explicitly requested
- **Audit log** — every browser action logged with URL and tool params
- **Session isolation** — named sessions prevent cross-task state leakage

### Use cases for our assistant

1. **Web research** — AI browses and summarizes web content
2. **Form filling** — AI fills out forms with user-provided data
3. **Price monitoring** — scheduled checks on product pages (with cron tool)
4. **Appointment booking** — navigate booking systems
5. **Testing** — validate web apps during development
6. **State persistence** — save/load browser state across sessions
7. **Network mocking** — test against mocked APIs
8. **Cookie/storage management** — manage web app sessions programmatically
