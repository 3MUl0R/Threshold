<div align="center">

# Threshold

**One conversation. Every interface. Persistent memory.**

[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024_edition-orange?style=flat-square)](https://doc.rust-lang.org/edition-guide/)
[![Built by AI](https://img.shields.io/badge/built_by-AI_agents-blueviolet?style=flat-square)](https://claude.ai/claude-code)

A Rust daemon that gives your AI agent a persistent home —
with memory, tools, scheduling, and multi-interface access that survives restarts.

</div>

---

## The Problem

AI assistants today are trapped in single-channel silos. Your Discord bot doesn't know what your web chat discussed. Scheduled tasks run in a vacuum. Context dies between sessions.

**Threshold fixes that.**

Discord channels map as portals into shared conversations — history, memory, agent context, and audit trails carry across. The web dashboard gives you a management view of everything. The portal framework is designed so additional interfaces (Teams, Slack) can plug in without reworking the engine.

## Key Ideas

- **Conversation-centric, not bot-centric.** Interfaces are windows into shared state, not isolated endpoints.
- **Persistent by default.** Conversations, scheduled work, and audit trails survive restarts. The daemon manages its own lifecycle.
- **Self-improving.** Threshold runs from source so AI agents can modify code, recompile, restart, and continue — autonomously improving the system they run on.
- **Auditable.** Every conversation action is logged to per-conversation JSONL audit trails. Scheduled work, tool usage, and agent decisions are all traceable.

## Features

**Conversation Engine** — Multi-agent profiles, portal-based routing, persistent state, per-conversation JSONL audit trails.

**Discord** — Slash commands, smart message chunking, typing indicators, live status updates, guild/user allowlists.

**Web Dashboard** — Conversation browser, log viewer with search and live tail, config editor, credential manager. Built with htmx and Pico CSS.

**Scheduler** — Cron with timezone support, per-conversation heartbeats, skip-if-running concurrency.

**Tools** — Gmail (OAuth read/search/send) and image generation have dedicated CLI subcommands. Browser automation (Playwright) is available to the AI agent via tool prompts injected into its system context.

**Daemon Management** — PID tracking, health checks, graceful drain-then-restart, follow-on hooks for agent continuity, launchd auto-start.

## Quick Start

**Prerequisites:** [Rust](https://rustup.rs) (stable) and [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli) (`npm install -g @anthropic-ai/claude-code`).

```bash
git clone https://github.com/3MUl0R/threshold.git && cd threshold
cargo build --release
mkdir -p ~/.threshold && cp config.example.toml ~/.threshold/config.toml
# Edit config.toml with your settings (Discord is optional)
./target/release/threshold daemon start
```

The web dashboard is live at http://127.0.0.1:3000 when `[web] enabled = true` in your config. Discord requires additional setup — see [SETUP.md](SETUP.md). For all config options, see [config.example.toml](config.example.toml).

## Give This to Your AI

Copy the block below and paste it into Claude Code, Cursor, or your preferred AI coding assistant:

> **Threshold** is a Rust workspace with 10 crates under `crates/` (auto-discovered). It's a daemon that orchestrates persistent AI conversations across interfaces (Discord + web today; portal framework supports more).
>
> **Build**: `cargo build --release`
> **Test**: `cargo test --workspace`
> **Run**: `./target/release/threshold daemon start`
> **Config**: `~/.threshold/config.toml` (example at `config.example.toml`)
>
> **Read `AGENTS.md` at the repo root** for full onboarding — crate map, code conventions, architecture, and testing details. Use `ENGINEERING.md` for project-wide standards. Detailed specs live in `docs/milestones/`.

Your AI now has enough context to explore, build, test, and contribute.

## Architecture

```
┌──────────────────────────────────────────┐
│  Interfaces  (Discord, Web, future)      │
├──────────────────────────────────────────┤
│  Portal Layer  (shared conversation map) │
├──────────────────────────────────────────┤
│  Conversation Engine  (state + routing)  │
├──────────────────────────────────────────┤
│  AI Provider Layer  +  Tool Integrations │
├──────────────────────────────────────────┤
│  Core  (config, secrets, audit, logging) │
└──────────────────────────────────────────┘
```

<details>
<summary><strong>Crate structure</strong></summary>

```
crates/
├── core/           Config, secrets, audit, logging, shared types
├── cli-wrapper/    Claude CLI subprocess & session management
├── conversation/   Conversation engine, portals, event bus
├── discord/        Discord bot (poise/serenity)
├── gmail/          Gmail OAuth + API client
├── imagegen/       Image generation API client
├── scheduler/      Cron scheduler + daemon API (Unix socket)
├── tools/          Tool prompt builder + registry
├── web/            Web dashboard (axum + htmx + Pico CSS)
└── server/         Main daemon binary — composition layer
```

</details>

<details>
<summary><strong>Data directory</strong></summary>

```
~/.threshold/
├── config.toml           Main configuration
├── threshold.pid          Daemon PID file
├── threshold.sock         Daemon API socket
├── logs/                 Daily-rotated log files
├── cli-sessions/         AI CLI session state
├── audit/                Per-conversation JSONL audit trails
├── state/                Scheduler, restart hooks, sentinels
├── conversations.json    Conversation metadata
└── portals.json          Portal registry
```

</details>

## Daemon Commands

```bash
threshold daemon start                    # Start the daemon
threshold daemon status                   # PID, uptime, active work, scheduler info
threshold daemon stop                     # Graceful drain + shutdown
threshold daemon restart                  # Build, drain, restart (with follow-on hooks)
threshold daemon install                  # Create launchd service (macOS auto-start)
threshold daemon uninstall                # Remove launchd service
threshold portal list                     # List active portals
threshold schedule --help                 # Scheduler commands
threshold gmail --help                    # Gmail integration
threshold imagegen --help                 # Image generation
```

## Development

The recommended way to work on Threshold is to let your AI handle it.

1. Open this repo in Claude Code, Cursor, or your preferred AI coding assistant
2. Point it at [`AGENTS.md`](AGENTS.md) for onboarding and [`docs/milestones/`](docs/milestones/) for feature specs
3. Use Codex CLI for code reviews: `codex exec --full-auto "review prompt"`

<details>
<summary><strong>Manual workflow</strong></summary>

```bash
cargo test --workspace                    # All tests
cargo test -p threshold-<crate> --lib     # Single crate
cargo fmt --all && cargo clippy --workspace
RUST_LOG=debug cargo run -p threshold -- daemon start
```

</details>

<details>
<summary><strong>How this project is built</strong></summary>

Threshold is developed using AI coding assistants as the primary developers. A human provides direction, reviews architecture, and makes product decisions. The AI writes the code, runs the tests, debugs failures, and iterates until everything passes.

Every feature starts as a detailed milestone spec in [`docs/milestones/`](docs/milestones/) — 17 milestones and counting. These specs serve as both documentation and executable instructions for AI agents. The system is designed to be improved by the agents that run on it.

Built with [Claude Code](https://claude.ai/claude-code). Reviewed with [Codex CLI](https://github.com/openai/codex).

</details>

## Docs

| Document | Purpose |
|----------|---------|
| [`AGENTS.md`](AGENTS.md) | AI-agent onboarding — crate map, conventions, architecture |
| [`ENGINEERING.md`](ENGINEERING.md) | Engineering principles and quality bar |
| [`docs/milestones/`](docs/milestones/) | Detailed implementation specs (17 milestones) |
| [`SETUP.md`](SETUP.md) | Discord bot creation and setup |
| [`config.example.toml`](config.example.toml) | All config options with inline comments |

## License

MIT — see [LICENSE](LICENSE).
