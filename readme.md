# Threshold - Multi-Agent Conversation System

A Rust-based conversation orchestration system that enables Claude to persist across multiple interfaces (Discord, CLI, future: web, API) with intelligent routing and multi-agent support.

## Current Status: Milestone 4 Complete ✅

### What's Working

#### ✅ Core Infrastructure
- **Configuration**: TOML-based config with validation
- **Logging**: Dual output (colored console + JSON file), daily rotation, 30-day retention
- **Secret Management**: macOS Keychain integration for secure token storage
- **State Persistence**: Conversations and portals saved to `~/.threshold/`

#### ✅ Conversation Engine
- Multi-agent support (default, coder, researcher)
- Per-agent Claude CLI configuration (model, prompts, tools)
- Portal-based interface abstraction
- Conversation lifecycle management
- Per-conversation audit trails (JSONL format)

#### ✅ Discord Integration
- Bot framework with poise/serenity
- Security middleware (guild + user allowlist)
- Smart message chunking (2000 char limit, code block aware)
- 5 slash commands: `/general`, `/coding`, `/research`, `/conversations`, `/join`
- Dynamic portal listeners with conversation tracking
- Graceful shutdown with state persistence

#### ✅ CLI Integration
- Claude CLI wrapper with model aliases (sonnet/opus/haiku)
- Session state management
- Configurable timeouts and permissions
- Health check mechanism

### Test Results

**138 passing tests** across all crates:
- `threshold-core`: 58 passed, 15 ignored
- `threshold-cli-wrapper`: 33 passed, 2 ignored
- `threshold-conversation`: 27 passed
- `threshold-discord`: 20 passed
- All doctests: passing

### Verified Functionality

✅ **Startup Sequence**
```bash
$ cargo run --bin threshold
# or
$ ./target/release/threshold

INFO threshold: Threshold starting...
INFO threshold: Claude CLI client configured.
INFO threshold: Conversation engine initialized.
```

✅ **Logging System**
- Console: Pretty-formatted, colored output
- File: JSON format at `~/.threshold/logs/threshold.log.YYYY-MM-DD`
- Rotation: Automatic daily rotation at midnight
- Cleanup: Old logs removed after 30 days

✅ **Graceful Shutdown**
```
INFO threshold: Shutdown signal received.
INFO threshold: Threshold shut down cleanly.
```

✅ **Configuration Validation**
- Catches invalid `permission_mode` values
- Validates agent configurations
- Optional Discord config (can run without Discord for testing)

### Directory Structure

```
~/.threshold/
├── config.toml              # Main configuration
├── logs/
│   └── threshold.log.*      # Daily JSON logs (30-day retention)
├── cli-sessions/            # Claude CLI session state
├── audit/                   # Per-conversation audit trails
│   └── {conversation-id}.jsonl
├── conversations.json       # Conversation metadata
└── portals.json            # Portal registry
```

## Quick Start

See [SETUP.md](SETUP.md) for detailed setup instructions.

### Minimal Setup

```bash
# 1. Install Rust and Claude CLI
# 2. Create config
mkdir -p ~/.threshold
cp config.example.toml ~/.threshold/config.toml

# 3. Build and run (Discord disabled for testing)
cargo run --bin threshold

# Or release build
cargo build --release --bin threshold
./target/release/threshold
```

### With Discord

```bash
# 1. Store Discord bot token
./scripts/setup-discord-token.sh

# 2. Edit config: set guild_id and allowed_user_ids
vim ~/.threshold/config.toml

# 3. Run
cargo run --bin threshold
```

## Architecture

### Layered Design

```
┌─────────────────────────────────────────┐
│  Interfaces (Discord, CLI, Web, API)    │
├─────────────────────────────────────────┤
│  Portal Layer (Channel Abstraction)     │
├─────────────────────────────────────────┤
│  Conversation Engine (State Machine)    │
├─────────────────────────────────────────┤
│  Claude CLI Wrapper                      │
├─────────────────────────────────────────┤
│  Core (Config, Logging, Secrets, Types) │
└─────────────────────────────────────────┘
```

### Key Concepts

- **Portal**: Abstraction for any communication channel (Discord channel, CLI session, etc.)
- **Conversation**: Stateful dialogue with specific mode (General/Coding/Research) and agent
- **Agent**: Configuration profile for Claude (model, prompts, tools)
- **Event Bus**: `broadcast` channel for engine → interface communication

### Concurrency Model

- Tokio async runtime with structured concurrency
- `Arc<RwLock<>>` for shared state with careful lock ordering
- `CancellationToken` for graceful shutdown coordination
- Background portal listeners for async message handling

## Testing

```bash
# All tests
cargo test --workspace

# Specific crate
cargo test -p threshold-discord

# With output
cargo test -- --nocapture

# Release mode
cargo test --release
```

## Development

### Code Structure

```
threshold/
├── crates/
│   ├── core/              # Config, logging, secrets, types
│   ├── cli-wrapper/       # Claude CLI integration
│   ├── conversation/      # Engine, portals, state
│   ├── discord/           # Discord bot implementation
│   └── server/            # Main binary
├── scripts/               # Setup and utility scripts
├── config.example.toml    # Configuration template
├── SETUP.md              # Setup guide
└── MILESTONE-*.md        # Implementation plans
```

### Adding New Interfaces

1. Implement interface handler (like `discord/`)
2. Emit `PortalEvent` on user messages
3. Subscribe to `ConversationEvent` for responses
4. Handle `PortalAttached` events to track conversation changes

### Lock Ordering

**Always** acquire locks in this order to prevent deadlocks:
1. `ConversationEngine.conversations`
2. `ConversationEngine.portals`
3. Individual conversation/portal locks

**Pattern**: Store `Arc` before calling `.read()`/`.write()`:
```rust
let portals_arc = engine.portals();  // Store Arc first
let portals = portals_arc.read().await;  // Then lock
```

## What's Next

See [ENGINEERING-PLAN.md](ENGINEERING-PLAN.md) for roadmap:
- **Milestone 5**: Web Dashboard (Next.js + tRPC)
- **Milestone 6**: Heartbeat & Proactive Notifications
- **Milestone 7**: Task Scheduler with Natural Language
- **Milestone 8**: Voice Integration
- **Milestone 9**: Memory Systems

---

## Development Notes

### Use OpenAI Codex for reviews on planning and staged code
Loop staged code reviews until all are resolved.

```bash
# Start a new review session
codex exec --full-auto "your prompt"

# Continue/resume an existing session
codex exec resume <session-id> --full-auto "follow-up prompt here"
```

### Use playwright-cli for end-to-end testing
```bash
playwright-cli --help
```
