# Threshold Setup Guide

This guide will help you get Threshold up and running.

## Prerequisites

- **Rust** (latest stable) - Install from [rustup.rs](https://rustup.rs)
- **Claude CLI** - Install with: `npm install -g @anthropic-ai/claude-sdk`
- **Discord Bot** - Create one at [Discord Developer Portal](https://discord.com/developers/applications)

## Quick Start

### 1. Store Discord Bot Token

Run the setup script to store your Discord bot token securely:

```bash
./scripts/setup-discord-token.sh
```

This will read the token from `.env` and store it in the secret store.

**Alternative**: Set environment variable:
```bash
export DISCORD_BOT_TOKEN='your-token-here'
```

### 2. Create Configuration File

Copy the example config and customize it:

```bash
mkdir -p ~/.threshold
cp config.example.toml ~/.threshold/config.toml
```

Edit `~/.threshold/config.toml`:

```toml
[discord]
guild_id = YOUR_SERVER_ID          # Right-click server → Copy Server ID
allowed_user_ids = [
    YOUR_USER_ID                   # Right-click your name → Copy User ID
]
```

**To enable Discord Developer Mode:**
1. Discord Settings → Advanced
2. Enable "Developer Mode"

### 3. Invite Bot to Your Server

Use this URL (replace `YOUR_APP_ID` with your bot's Application ID):

```
https://discord.com/api/oauth2/authorize?client_id=YOUR_APP_ID&permissions=274878024768&scope=bot%20applications.commands
```

### 4. Build and Run

```bash
# Build the server
cargo build --release --bin threshold

# Run it
./target/release/threshold
```

Or for development:

```bash
cargo run --bin threshold
```

## Logging

Logs are stored in `~/.threshold/logs/`:

- **Current logs**: `threshold.log` (JSON format)
- **Console output**: Pretty-formatted, colored
- **Rotation**: Daily at midnight
- **Retention**: Last 30 days automatically kept
- **Permissions**: Owner-only (0600 on Unix)

### Viewing Logs

Pretty-print JSON logs:
```bash
tail -f ~/.threshold/logs/threshold.log | jq
```

Filter by level:
```bash
jq 'select(.level == "ERROR")' ~/.threshold/logs/threshold.log
```

Search for errors:
```bash
jq 'select(.fields.error)' ~/.threshold/logs/threshold.log
```

### Log Levels

Set via environment variable or config:

```bash
# Environment variable (takes precedence)
export RUST_LOG=debug

# Or in config.toml
log_level = "debug"
```

Levels: `trace` | `debug` | `info` | `warn` | `error`

## Directory Structure

```
~/.threshold/
├── config.toml              # Main configuration
├── logs/
│   ├── threshold.log        # Current log file
│   └── threshold.log.*      # Archived logs (rotated daily)
├── cli-sessions/            # Claude CLI session state
│   └── cli-sessions.json
├── audit/                   # Per-conversation audit trails
│   └── {conversation-id}.jsonl
├── conversations.json       # Conversation metadata
└── portals.json            # Portal registry
```

## Usage

Once running, interact via Discord:

### Slash Commands

- `/general` - Switch to General conversation
- `/coding [project]` - Start/resume coding conversation
- `/research [topic]` - Start/resume research conversation
- `/conversations` - List all active conversations
- `/join [id]` - Join specific conversation by ID

### Regular Messages

Just type in any channel! The bot will:
- Auto-create a portal for that channel
- Respond with typing indicator
- Chunk long responses (2000 char Discord limit)
- Preserve code blocks across chunks

## Troubleshooting

### Bot doesn't respond

Check:
1. Bot is online (green status in Discord)
2. Your user ID is in `allowed_user_ids`
3. Message is in the correct guild (server)
4. Check logs: `tail -f ~/.threshold/logs/threshold.log | jq`

### "Secret not found" error

Discord token not set. Run:
```bash
./scripts/setup-discord-token.sh
```

Or set environment variable:
```bash
export DISCORD_BOT_TOKEN='your-token'
```

### "Configuration file not found"

Create config file:
```bash
mkdir -p ~/.threshold
cp config.example.toml ~/.threshold/config.toml
```

### Logs filling up disk

Logs automatically rotate daily and keep only 30 days. To change retention:

Edit `crates/core/src/logging.rs` and change:
```rust
cleanup_old_logs(log_dir, 30)?;  // Change 30 to desired days
```

Then rebuild:
```bash
cargo build --release --bin threshold
```

## Development

### Run tests

```bash
# All tests
cargo test

# Specific crate
cargo test -p threshold-conversation

# With output
cargo test -- --nocapture
```

### Enable debug logging

```bash
RUST_LOG=debug cargo run --bin threshold
```

### Code formatting

```bash
cargo fmt --all
cargo clippy --all
```

## Next Steps

- Configure agent system prompts in `config.toml`
- Set up tool permissions (browser, gmail, etc.)
- Explore conversation audit trails in `~/.threshold/audit/`
- Try different Claude models (sonnet, opus, haiku)

For more information, see the full documentation in `docs/`.
