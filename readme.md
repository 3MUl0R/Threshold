# Threshold

**AI conversation orchestration — built by AI, for AI, with AI.**

[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![Workspace Tests](https://img.shields.io/badge/tests-workspace-brightgreen.svg)]()

Threshold is a Rust daemon for persistent, interface-agnostic AI conversations. Discord and the local web dashboard are integrated today; the portal framework is designed for additional interfaces like Teams and Slack. AI runtime is currently via the Claude CLI, with additional providers (including Codex) planned.

## What Makes Threshold Different

Threshold is conversation-centric.

- Interfaces are portals into the same conversation, not separate bot silos.
- You can switch interface and continue the same conversation history, memory, and agent context.
- Scheduled and autonomous work flows through the same conversation state and audit trail.

## Project Status

Threshold is actively developed through milestone specs in [`docs/milestones/`](docs/milestones/).

- Stable foundation: conversation engine, Discord + web interfaces, scheduler, tools, persistence, and audit trails.
- Milestone 15 (in progress): multi-platform portal semantics (primary portal + origin-targeted delivery).
- Milestone 16 (in progress): daemon lifecycle management (`status`, `stop`, `restart`, supervised mode).

If command behavior differs from examples below, treat `threshold --help` as source of truth for your branch.

## How This Project Is Built

Threshold is developed using AI coding assistants as the primary developer. The human provides direction, reviews architecture, and makes product decisions. The AI writes the code, runs the tests, debugs failures, and iterates until everything passes.

The project was built with [Claude Code](https://claude.ai/claude-code) and reviewed with [Codex CLI](https://github.com/openai/codex). Every feature has a detailed specification in [`docs/milestones/`](docs/milestones/) — 16 milestones of implementation specs that serve as both documentation and instructions for AI agents.

The recommended way to work on Threshold is to let your AI handle it.

## Give This to Your AI

Copy the block below and paste it into Claude Code, Cursor, or your preferred AI coding assistant:

> **Threshold** is a Rust workspace with 10 crates under `crates/` (auto-discovered). It's a daemon that orchestrates persistent AI conversations across interfaces (Discord + web today; portal framework supports more).
>
> **Key conventions**: Rust 2024 edition (no `ref` in patterns). Thin server wrappers in `crates/server/src/<tool>.rs` delegate to library crates. Milestone specs in `docs/milestones/` are implementation contracts.
>
> **Build**: `cargo build --release`
> **Test**: `cargo test --workspace`
> **Run**: `./target/release/threshold daemon`
> **Config**: `~/.threshold/config.toml` (example at `config.example.toml`)
>
> **Read `AGENTS.md` at the repo root** for full onboarding — crate map, code conventions, architecture, and testing details. Use `ENGINEERING.md` for project-wide standards. Detailed specs live in `docs/milestones/`.

Your AI now has enough context to explore, build, test, and contribute.

## What Threshold Does

**Conversation Engine** — Multi-agent support with configurable profiles, portal-based routing (any channel maps to a conversation), persistent state, and per-conversation JSONL audit trails.

**Discord Bot** — Slash commands (`/general`, `/coding`, `/research`, `/conversations`, `/join`), smart message chunking, guild/user allowlist, typing indicators, live status updates.

**Web Dashboard** — Local interface at `http://127.0.0.1:3000` with real-time status, conversation browser, log viewer with search and live tail, config editor, and credential manager. Built with axum, minijinja, htmx, and Pico CSS.

**Provider Layer** — CLI-provider architecture with Claude integrated today and Codex support planned.

**Task Scheduler** — Cron-based with timezone-aware scheduling, per-conversation heartbeats, skip-if-running concurrency, one-shot tasks, and persistent state via daemon API (Unix socket).

**Tool Integrations** — Browser automation (Playwright), Gmail (OAuth read/search/send), and image generation (Google API). Tools are injected into Claude's system prompt automatically.

**Infrastructure** — File-based or keychain-backed secrets with env var fallback. Daily-rotated logs with 30-day retention. TOML configuration. Graceful shutdown via cancellation tokens. Daemon-management enhancements are rolling in via Milestone 16.

## Portal Routing Model (Milestone 15)

Threshold uses a portal abstraction so multiple interfaces can map into shared conversation state.

- One conversation can have multiple portals.
- User-initiated responses should route to the origin portal.
- Scheduled/heartbeat output should route to the conversation's primary portal (or an explicit portal override).
- This enables multi-platform support without noisy cross-posting.

Discord is the first integrated portal. The abstraction is designed so Teams/Slack-style portals can plug in without reworking the conversation engine.

## Daemon Management (Milestone 16)

Current entrypoint:

```bash
threshold daemon [--config <path>]
```

Milestone 16 target surface (rolling out):

```bash
threshold daemon start [--config <path>]
threshold daemon status
threshold daemon stop [--drain-timeout <secs>]
threshold daemon restart [--skip-build] [--drain-timeout <secs>]
threshold daemon install
threshold daemon uninstall
```

Design goals for this flow are build-first safety, drain-before-stop behavior, and supervised restart support.

## Architecture

```
┌─────────────────────────────────────────┐
│  Interfaces (Discord/Web today;          │
│              Teams/Slack-ready)          │
├─────────────────────────────────────────┤
│  Portal Layer (Channel Abstraction)     │
├─────────────────────────────────────────┤
│  Conversation Engine (State Machine)    │
├─────────────────────────────────────────┤
│  AI CLI Provider Layer + Integrations    │
├─────────────────────────────────────────┤
│  Core (Config, Logging, Secrets, Types) │
└─────────────────────────────────────────┘
```

### Crate Structure

```
crates/
├── core/           Config, secrets, audit, logging, shared types (no internal deps)
├── cli-wrapper/    Claude CLI subprocess & session management
├── conversation/   Conversation engine, portals, event bus
├── discord/        Discord bot (poise/serenity)
├── gmail/          Gmail OAuth + API client
├── imagegen/       Image generation API client
├── scheduler/      Cron scheduler + daemon API
├── tools/          Tool prompt builder
├── web/            Web dashboard (axum + htmx)
└── server/         Main daemon binary
```

### Data Directory

```
~/.threshold/
├── config.toml           # Main configuration
├── threshold.pid          # Daemon PID file (M16)
├── threshold.sock         # Daemon API socket
├── logs/                 # Daily rotated log files
├── cli-sessions/         # Claude CLI session state
├── audit/                # Per-conversation JSONL audit trails
├── state/                # Scheduler tasks, restart hooks, service sentinels
├── conversations.json    # Conversation metadata
└── portals.json          # Portal registry
```

## Setup

### Prerequisites (you do these)

1. Install [Rust](https://rustup.rs) (stable toolchain)
2. Install [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli): `npm install -g @anthropic-ai/claude-code`
3. (Optional) Create a Discord bot at the [Discord Developer Portal](https://discord.com/developers/applications)

### Then tell your AI

"Clone and build Threshold, set up the config, and start the daemon."

Or if you prefer explicit steps:

```bash
# Clone and build
git clone <repo-url> threshold
cd threshold
cargo build --release

# Create config
mkdir -p ~/.threshold
cp config.example.toml ~/.threshold/config.toml
# Edit config.toml with your settings

# Store Discord token (if using Discord)
./scripts/setup-discord-token.sh
# Or: export DISCORD_BOT_TOKEN='your-token'

# Start the daemon
./target/release/threshold daemon
```

The web dashboard starts automatically at http://127.0.0.1:3000 if `[web] enabled = true` is in your config.

See [SETUP.md](SETUP.md) for detailed Discord bot creation and invitation steps. See [config.example.toml](config.example.toml) for all configuration options.

### Daemon Command Notes

```bash
threshold daemon --help                         # Always check available actions
threshold schedule --help                       # Scheduler command surface
threshold gmail --help                          # Gmail integration commands
threshold imagegen --help                       # Image generation commands
```

## Development

### AI-as-developer (recommended)

1. Open this repo in Claude Code, Cursor, or your preferred AI coding assistant
2. Start with [`AGENTS.md`](AGENTS.md) for onboarding and [`ENGINEERING.md`](ENGINEERING.md) for project standards
3. Point your AI at the relevant spec in [`docs/milestones/`](docs/milestones/) for context on any feature area
4. Your AI runs `cargo test --workspace` to verify changes
5. Use Codex CLI for code reviews before committing:
   ```bash
   codex exec --full-auto "review prompt here"
   ```

### Manual workflow (also works)

```bash
cargo test --workspace                         # Run all tests
cargo test -p threshold-<crate> --lib          # Test a specific crate
cargo fmt --all && cargo clippy --workspace    # Format and lint
RUST_LOG=debug cargo run -p threshold -- daemon         # Debug logging
```

### E2E Testing

```bash
cd crates/web && bash tests/e2e_playwright.sh
```

### Key Conventions

- Rust 2024 edition — no `ref` in patterns
- Thin server wrappers in `crates/server/src/<tool>.rs` delegate to library crates
- CLI subcommands output JSON to stdout
- `SecretStore` for credentials, `AuditTrail` for JSONL logging
- `CancellationToken` for graceful shutdown coordination

## Documentation

| Document | Purpose |
|----------|---------|
| [`AGENTS.md`](AGENTS.md) | Canonical AI-agent onboarding — crate map, conventions, architecture |
| [`ENGINEERING.md`](ENGINEERING.md) | Project engineering principles and quality bar |
| [`CLAUDE.md`](CLAUDE.md) | Compatibility shim that points to `AGENTS.md` |
| [`docs/PRODUCT-PLAN.md`](docs/PRODUCT-PLAN.md) | Full product vision and data flow design |
| [`docs/IMPLEMENTATION-PLAN.md`](docs/IMPLEMENTATION-PLAN.md) | Milestone summary and dependency graph |
| [`docs/milestones/`](docs/milestones/) | 16 detailed implementation specifications |
| [`SETUP.md`](SETUP.md) | Discord bot creation and troubleshooting |
| [`config.example.toml`](config.example.toml) | All config options with inline comments |

## License

MIT License. See [LICENSE](LICENSE) for details.
