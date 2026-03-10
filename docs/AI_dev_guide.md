# Threshold — AI Agent Onboarding

Canonical onboarding for AI coding agents working in this repository.

Threshold is a conversation-centric Rust daemon. Interfaces (Discord, web today; more via portals) map into shared conversations with persistent memory, audit trail, and scheduler integration. Runtime is currently Claude CLI, with additional providers planned.

## Quick Reference

```bash
cargo build --release                          # Build release binary
cargo test --workspace                         # Run workspace tests
cargo test -p threshold-<crate> --lib          # Run a single crate's library tests
./target/release/threshold daemon start         # Start the daemon
./target/release/threshold daemon --help       # Check daemon command surface on this branch
cargo fmt --all && cargo clippy --workspace    # Format and lint
```

## Crate Map

```
crates/
├── core/           Config, secrets, audit, logging, shared types (no internal deps)
├── cli-wrapper/    AI CLI subprocess/session layer (depends: core)
├── conversation/   Conversation engine, portals, event bus (depends: core, cli-wrapper)
├── discord/        Discord bot via poise/serenity (depends: core, conversation, scheduler)
├── gmail/          Gmail OAuth + API client (depends: core)
├── imagegen/       Image generation API client (depends: core)
├── scheduler/      Cron scheduler + daemon API over Unix socket (depends: core, cli-wrapper, conversation)
├── tools/          Tool prompt builder (depends: core)
├── web/            Web dashboard: axum + minijinja + htmx + Pico CSS
└── server/         Main daemon binary — composition/wiring layer
```

Workspace crates are auto-discovered under `crates/`.

## Working Rules

- Rust 2024 edition conventions.
- Thin server wrappers in `crates/server/src/<tool>.rs`, with logic in library crates.
- Preserve conversation-centric semantics: portals map interfaces into shared conversation state.
- Keep provider logic abstract where possible; avoid hard-coding interface/provider assumptions in core flow.
- Keep shutdown/restart behavior safe and explicit (see Milestone 16 docs).
- Maintain lock discipline in conversation engine: `portals` before `conversations` when both are needed.

## Data Directory

```
~/.threshold/
├── config.toml
├── threshold.sock
├── threshold.pid
├── logs/
├── cli-sessions/
├── audit/
├── state/
├── conversations.json
└── portals.json
```

## Testing

- Workspace tests: `cargo test --workspace`
- Targeted crate tests: `cargo test -p threshold-<crate> --lib`
- Web E2E: `cd crates/web && bash tests/e2e_playwright.sh`

## Review Flow

```bash
codex exec --full-auto "your review prompt"
codex exec resume <session-id> --full-auto "follow-up prompt"
```

Resolve findings before merge.

## Key Docs

- `ENGINEERING.md` — enduring engineering principles and quality bar
- `docs/PRODUCT-PLAN.md` — product vision and data flows
- `docs/IMPLEMENTATION-PLAN.md` — milestone sequencing/dependencies
- `docs/milestones/` — detailed implementation specs
- `SETUP.md` — environment setup and Discord onboarding
- `config.example.toml` — configuration reference
