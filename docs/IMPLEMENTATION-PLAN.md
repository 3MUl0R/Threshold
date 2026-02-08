# Threshold — Implementation Plan

## Overview

Threshold is a developer-focused AI agent platform built in Rust. It lives in
Discord, uses the Claude Code CLI as a subprocess for inference (borrowing
existing subscriptions), and can work autonomously via a heartbeat system.

**v1 scope:** Discord portal, Claude CLI integration, first-class conversations,
tool framework, heartbeat system, cron scheduler, browser automation, Gmail
integration, and image generation (NanoBanana).

**Not in v1 (but architecturally supported):** Voice portals, web UI, phone
integration, Codex CLI, multi-user, smart home.

---

## Cargo Workspace Structure

```
threshold/
  Cargo.toml                    workspace root
  crates/
    core/                       Milestone 1  — shared types, config, secrets, audit
    cli-wrapper/                Milestone 2  — Claude CLI subprocess management
    conversation/               Milestone 3  — conversation engine, portals, routing
    discord/                    Milestone 4  — Discord bot (serenity/poise)
    tools/                      Milestone 5  — tool trait, built-in tools, profiles
    heartbeat/                  Milestone 6  — periodic autonomous agent wake-ups
    scheduler/                  Milestone 7  — cron-based scheduled tasks
    browser/                    Milestone 8  — Playwright CLI integration
    gmail/                      Milestone 9  — Gmail API read/send
    imagegen/                   Milestone 10 — NanoBanana image generation
    server/                     Milestone 4+ — the main binary
```

Each crate has a focused responsibility and well-defined dependencies on other
crates. The `server` crate is the only binary — everything else is a library.

---

## Milestone Summary

| # | Name | Crate | Complexity | Depends On | Delivers |
|---|------|-------|-----------|------------|----------|
| 1 | Core Foundation | `core` | Medium | — | Types, config, secrets, audit, logging |
| 2 | Claude CLI Wrapper | `cli-wrapper` | Medium | 1 | Subprocess spawning, session mgmt, response parsing |
| 3 | Conversation Engine | `conversation` | Large | 1, 2 | First-class conversations, portals, mode switching |
| 4 | Discord Portal + Server | `discord`, `server` | Large | 1–3 | **First runnable system** — working Discord bot |
| 5 | Tool Framework | `tools` | Large | 1 | Tool trait, exec/file/web tools, profiles, audit |
| 6 | Heartbeat System | `heartbeat` | Medium | 1, 2, 5 | Autonomous agent wake-ups, task store, handoff notes |
| 7 | Cron Scheduler | `scheduler` | Medium | 1, 2, 4, 5 | Scheduled tasks with Discord delivery |
| 8 | Browser Automation | `browser` | Medium | 5 | Playwright CLI tool, sessions, network filtering |
| 9 | Gmail Integration | `gmail` | Medium | 1, 5 | Read inboxes, permissioned send |
| 10 | Image Generation | `imagegen` | Small | 1, 5 | NanoBanana via Google API, Discord artifact delivery |

---

## Dependency Graph

```
Milestone 1 (Core)
    │
    ├── Milestone 2 (CLI Wrapper)
    │       │
    │       ├── Milestone 3 (Conversation Engine)
    │       │       │
    │       │       └── Milestone 4 (Discord + Server)  ◄── FIRST RUNNABLE SYSTEM
    │       │               │
    │       │               └── Milestone 7 (Cron Scheduler)
    │       │
    │       └── Milestone 6 (Heartbeat)
    │
    └── Milestone 5 (Tool Framework)
            │
            ├── Milestone 6 (Heartbeat)
            ├── Milestone 7 (Cron Scheduler)
            ├── Milestone 8 (Browser Automation)
            ├── Milestone 9 (Gmail)
            └── Milestone 10 (Image Generation)
```

**Milestones 1–4** are sequential and produce the first runnable system.

**Milestone 5** (Tools) can be built in parallel with Milestones 3–4 since it
only depends on Milestone 1.

**Milestones 6–10** are additive capabilities that can be built in any order
once their dependencies are met.

---

## Build Order

**Phase A — Core Platform (Milestones 1–4)**
Get to a working Discord bot that can hold conversations with Claude.

**Phase B — Autonomous Capabilities (Milestones 5–7)**
Add the tool framework, heartbeat system, and cron scheduler. This turns the
assistant from "responds to messages" into "works on its own."

**Phase C — Integrations (Milestones 8–10)**
Add browser automation, Gmail, and image generation. These are independent of
each other and can be built in any order.

---

## Key Architectural Decisions

### CLI-First Inference
The assistant does NOT call AI provider APIs directly for conversations. It
spawns `claude` as a subprocess with `-p --output-format json`. The CLI handles
context management, compaction, and caching internally. This gives us token
efficiency and free improvements as the CLI is updated.

### Credential Borrowing
The server reads existing CLI credentials from `~/.claude/.credentials.json`
or OS keychain. It clears `ANTHROPIC_API_KEY` from the child environment to
ensure subscription-rate billing (not API-rate billing).

### Conversations Are First-Class
A conversation is an independent entity with its own CLI session, history, and
context. Portals (Discord channels, future voice devices, etc.) attach and
detach from conversations dynamically.

### No Plugins
Every integration is a native Rust module compiled into the binary. No dynamic
loading, no marketplace, no third-party code execution. Enable/disable via
config.

### Security
- Discord locked to a specific guild + allowlisted user IDs
- Git uses scoped GitHub tokens (not full credentials)
- All API keys in OS keychain, never in config files
- Tool permission modes: FullAuto, ApproveDestructive, ApproveAll
- All tool invocations audit-logged (append-only JSONL)

---

## Milestone Detail Documents

Each milestone has its own detailed implementation document:

- [Milestone 1 — Core Foundation](milestones/milestone-01-core-foundation.md)
- [Milestone 2 — Claude CLI Wrapper](milestones/milestone-02-cli-wrapper.md)
- [Milestone 3 — Conversation Engine](milestones/milestone-03-conversation-engine.md)
- [Milestone 4 — Discord Portal + Server](milestones/milestone-04-discord-server.md)
- [Milestone 5 — Tool Framework](milestones/milestone-05-tool-framework.md)
- [Milestone 6 — Heartbeat System](milestones/milestone-06-heartbeat.md)
- [Milestone 7 — Cron Scheduler](milestones/milestone-07-cron-scheduler.md)
- [Milestone 8 — Browser Automation](milestones/milestone-08-browser-automation.md)
- [Milestone 9 — Gmail Integration](milestones/milestone-09-gmail.md)
- [Milestone 10 — Image Generation](milestones/milestone-10-image-generation.md)

---

## Key Reference Documents

These research docs inform the implementation:

| Doc | Informs |
|-----|---------|
| `docs/PRODUCT-PLAN.md` | Architecture, types, config schema, data flows |
| `docs/03-claude-code-cli-wrapper.md` | CLI flags, session management, response parsing |
| `docs/04-codex-cli-wrapper.md` | Future Codex integration patterns |
| `docs/05-channel-integration.md` | Portal/conversation mapping, message routing |
| `docs/08-api-key-management.md` | Secrets management, keychain patterns |
| `docs/09-tool-execution-model.md` | Tool trait design, profiles, audit patterns |
| `docs/10-playwright-browser-automation.md` | Browser automation CLI commands |

---

## Data Directory Layout

All Threshold data lives under a single root directory across all platforms:

- **Unix/macOS:** `$HOME/.threshold/`
- **Windows:** `%USERPROFILE%\.threshold\`
- **Override:** `THRESHOLD_CONFIG` env var points to config.toml; `data_dir`
  in config overrides the data root

```
~/.threshold/
  config.toml                     Main configuration (no secrets)
  state/
    conversations.json            Conversation metadata
    portals.json                  Active portal registry
    sessions.json                 CLI session ID mapping
    tasks.json                    Heartbeat task store
    schedules.json                Cron scheduled tasks
    heartbeat-notes.md            Heartbeat handoff notes
  audit/
    conversations/
      {conversation-id}.jsonl     Per-conversation audit trail
    tools.jsonl                   Tool invocation audit log
    system.jsonl                  System events
  logs/
    threshold.log                 Application log
  heartbeat.md                    Heartbeat instructions
```
