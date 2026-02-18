# Threshold

A Rust-based AI conversation orchestration system. Threshold gives Claude persistent, multi-interface presence — Discord, a local web dashboard, scheduled tasks, and tool integrations — all managed through a single daemon.

## Features

### Conversation Engine
- **Multi-agent support** with configurable profiles (model, system prompt, tools)
- **Portal-based routing** — any interface channel maps to a conversation
- **Persistent state** — conversations, portals, and audit trails survive restarts
- **Per-conversation audit trails** in JSONL format

### Discord Bot
- Slash commands: `/general`, `/coding`, `/research`, `/conversations`, `/join`
- Smart message chunking (2000-char limit, code-block aware)
- Guild + user allowlist security
- Typing indicators and graceful shutdown

### Web Management Interface
- Local dashboard at `http://127.0.0.1:3000` (loopback-only, no auth needed)
- Real-time status: uptime, conversations, scheduler, Discord connection
- Conversation browser with full audit trail viewer
- Log viewer with level filtering, search, and live tail
- Config editor with TOML validation
- Credential manager (keychain-backed)
- Built with axum, minijinja, htmx, and Pico CSS

### Task Scheduler
- Cron-based scheduling with natural language task definitions
- Per-conversation heartbeats (periodic check-ins)
- Skip-if-running concurrency control
- Persistent task store with daemon API (Unix socket)

### Tool Integrations
- **Browser**: Headless/headed browsing via Playwright
- **Gmail**: OAuth-based email access (read, search, send)
- **Image Generation**: Google API image generation
- Tools are injected into Claude's system prompt automatically

### Infrastructure
- **Secrets**: macOS Keychain integration (env var fallback)
- **Logging**: Colored console + daily-rotated file logs, 30-day retention
- **Config**: TOML-based with validation
- **Graceful shutdown** with `CancellationToken` coordination

## Quick Start

### Prerequisites

- [Rust](https://rustup.rs) (stable, 2024 edition)
- [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli) (`npm install -g @anthropic-ai/claude-code`)

### Install and Run

```bash
# Clone and build
git clone <repo-url> threshold
cd threshold
cargo build --release

# Create config
mkdir -p ~/.threshold
cp config.example.toml ~/.threshold/config.toml
# Edit config.toml with your settings

# Run the daemon
./target/release/threshold daemon
```

The web interface starts automatically at http://127.0.0.1:3000 if `[web] enabled = true` is in your config.

### Discord Setup

```bash
# Store bot token in keychain
./scripts/setup-discord-token.sh

# Or use environment variable
export DISCORD_BOT_TOKEN='your-token'

# Set guild_id and allowed_user_ids in config.toml, then run
./target/release/threshold daemon
```

See [SETUP.md](SETUP.md) for detailed Discord bot creation and invitation steps.

## Configuration

Config lives at `~/.threshold/config.toml`. See [config.example.toml](config.example.toml) for all options.

Key sections:
- `[cli.claude]` — Claude CLI model, timeout, permissions
- `[discord]` — Guild ID, allowed users
- `[[agents]]` — Named agent profiles (model, system prompt, tools)
- `[tools]` — Browser, Gmail, image gen settings
- `[scheduler]` — Task scheduling
- `[web]` — Web interface (bind address, port)

Credentials (bot tokens, API keys, OAuth secrets) are stored in the macOS Keychain and managed via the web interface or CLI scripts.

## Architecture

```
┌─────────────────────────────────────────┐
│  Interfaces (Discord, Web, CLI)         │
├─────────────────────────────────────────┤
│  Portal Layer (Channel Abstraction)     │
├─────────────────────────────────────────┤
│  Conversation Engine (State Machine)    │
├─────────────────────────────────────────┤
│  Claude CLI Wrapper + Tool Integrations │
├─────────────────────────────────────────┤
│  Core (Config, Logging, Secrets, Types) │
└─────────────────────────────────────────┘
```

### Crate Structure

```
crates/
├── core/           # Config, secrets, audit, logging, shared types
├── cli-wrapper/    # Claude CLI client and session management
├── conversation/   # Conversation engine, portal routing, event bus
├── discord/        # Discord bot (poise/serenity)
├── gmail/          # Gmail OAuth + API client
├── imagegen/       # Image generation API client
├── scheduler/      # Cron scheduler + daemon API
├── tools/          # Tool prompt builder
├── web/            # Web management interface (axum + htmx)
└── server/         # Main daemon binary
```

### Data Directory

```
~/.threshold/
├── config.toml           # Main configuration
├── logs/                 # Daily rotated log files
├── cli-sessions/         # Claude CLI session state
├── audit/                # Per-conversation JSONL audit trails
├── state/                # Scheduler task persistence
├── conversations.json    # Conversation metadata
├── portals.json          # Portal registry
└── threshold.sock        # Daemon API socket
```

## Development

```bash
# Run all tests
cargo test --workspace

# Run specific crate tests
cargo test -p threshold-web --lib
cargo test -p threshold-conversation

# Run with debug logging
RUST_LOG=debug cargo run -p threshold -- daemon

# Format and lint
cargo fmt --all
cargo clippy --workspace
```

### E2E Testing

The web interface has a Playwright E2E test suite:

```bash
cd crates/web
bash tests/e2e_playwright.sh
```

### Code Reviews

```bash
# Start a new review session
codex exec --full-auto "your prompt"

# Resume an existing session
codex exec resume <session-id> --full-auto "follow-up prompt"
```

## License

See [LICENSE](LICENSE) for details.
