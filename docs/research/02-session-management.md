# Session Management — Patterns & Architecture

## Summary

OpenClaw implements its own session management layer on top of the AI providers.
This is the area most responsible for token-hungry behavior — full conversation
history is persisted and replayed on each API call. The system is sophisticated
(tree-structured transcripts, compaction, multi-channel isolation) but competing
with the optimizations built into Claude Code and Codex CLIs is a losing battle.

**Key lesson**: For our project, delegate long-running conversations to the CLIs.
Only use direct API calls for stateless/short inference tasks.

---

## Storage Architecture

### Two-Layer Persistence

**Layer 1: Session Store** (`~/.openclaw/agents/<agentId>/sessions/sessions.json`)
- Lightweight JSON key/value map: `sessionKey → SessionEntry`
- Tracks metadata: timestamps, token counters, per-session toggles
- Safe to edit/delete entries

**Layer 2: Transcripts** (`~/.openclaw/agents/<agentId>/sessions/<sessionId>.jsonl`)
- Append-only JSONL files (one JSON object per line)
- First line: session header with metadata
- Tree-structured: each entry has `id` + `parentId` (supports branching)
- Entry types: `message`, `custom_message`, `custom`, `compaction`, `branch_summary`

### Session Entry Metadata

Each session tracks extensive per-session state:
- Token counters: `inputTokens`, `outputTokens`, `totalTokens`, `contextTokens`
- Compaction count
- Per-session overrides: model, provider, auth profile, thinking level
- Channel context: chat type, delivery context, origin
- Queue mode, send policy, TTS settings

---

## Session Lifecycle

### Creation
1. Resolve `sessionKey` from channel + sender + dmScope config
2. Check session store for existing entry
3. Apply freshness policy (daily reset at 4 AM, or idle timeout)
4. If fresh → reuse existing session ID
5. If expired or `/new` command → mint new UUID
6. Persist entry to `sessions.json`

### Expiry Conditions
- **Daily reset**: 4:00 AM local time (default)
- **Idle timeout**: configurable `session.idleMinutes` (default: 60 min)
- **Manual**: `/new` or `/reset` commands
- When both configured: whichever expires first wins

### Replay
On each new message:
1. `SessionManager.open(sessionFile)` loads the JSONL transcript
2. `buildSessionContext()` reconstructs messages from the tree
3. `sanitizeSessionHistory()` applies provider-specific fixes
4. Full history sent to model API

This is where token waste happens — every turn replays the entire conversation.

---

## Context Window Management

### Three-Level Token Management

1. **Model context window** (hard cap) — e.g., 200K tokens for Claude
2. **Session token counters** (reporting) — best-effort estimates for dashboards
3. **Reserved tokens** (headroom) — default 20K, ensures room for compaction

### Auto-Compaction

```
IF contextTokens > contextWindow - reserveTokens
THEN trigger compaction
```

Before compaction:
- A "memory flush" turn runs silently (NO_REPLY prefix)
- Writes durable state to workspace memory files
- Triggers ~4000 tokens before the hard compaction threshold

### Context Pruning (separate from compaction)
- Trims old tool results from in-memory context
- Does NOT rewrite on-disk history
- Only runs when cache TTL expires

---

## Multi-Channel Session Isolation

### DM Scope Modes

| Mode | Session Key Pattern | Use Case |
|------|-------------------|----------|
| `main` (default) | `agent:<id>:main` | All DMs share one session |
| `per-peer` | `agent:<id>:dm:<peerId>` | One session per person |
| `per-channel-peer` | `agent:<id>:<channel>:dm:<peerId>` | Per person per channel |
| `per-account-channel-peer` | `agent:<id>:<channel>:<account>:dm:<peerId>` | Multi-account inboxes |

### Identity Linking
Users can be unified across channels:
```json
{
  "session": {
    "identityLinks": {
      "alice": ["telegram:123456789", "discord:987654321012345678"]
    }
  }
}
```

### Group/Channel Sessions
- Groups: `agent:<id>:<channel>:group:<groupId>`
- Channels/rooms: `agent:<id>:<channel>:channel:<channelId>`
- Telegram topics: append `:topic:<threadId>`

---

## Concurrency & Reliability

### Write Locking
- File-based locks on session files (`proper-lockfile`)
- Process ID tracking in `.lock` files
- Dead holder detection (PID no longer alive)
- Stale timeout: 30 minutes
- Cleanup on SIGINT, SIGTERM, SIGQUIT, SIGABRT

### Session File Repair
- Drops malformed JSONL lines automatically
- Creates backup: `.bak-<pid>-<timestamp>`
- Validates session header is present
- Reconstructs cleaned file atomically

### Caching
- Session manager cache TTL: 45 seconds (tunable via env var)
- Session store cache: 45 seconds with mtime-based invalidation
- Pre-warms OS page cache by reading 4KB chunk

---

## Tool Call Storage

Tool calls follow a guard pattern:

1. Assistant message with tool calls → appended to session
2. Tool execution happens outside session manager
3. Tool result message created → guard tracks pending results
4. Hard size cap applied (~100-200MB based on context)
5. Synthetic results auto-generated if provider requires paired tool results

The guard prevents "unexpected toolUseId" API errors by ensuring every tool call
has a corresponding result, even if execution was interrupted.

---

## Takeaways for Our Project

### What to adopt
- **Session isolation by channel/peer** — essential for multi-channel assistant.
  The `per-channel-peer` scope is the right default.
- **Identity linking** — users should have continuity across channels
- **Append-only JSONL transcripts** — good audit trail, crash-safe
- **File locking for concurrent access** — multiple channels may write simultaneously
- **Session file repair** — JSONL is resilient to partial writes

### What to do differently
- **Delegate context management to CLIs** — this is the biggest lesson. OpenClaw's
  custom compaction/pruning is fighting a battle the CLIs have already won. Use
  `claude --session-id` and `codex exec resume` for long conversations.
- **Direct API only for short/stateless tasks** — small inference (classification,
  extraction, summarization) where we control the full context explicitly.
- **Simpler session metadata** — we don't need 20+ per-session toggle fields.
  Session key + creation time + last activity + CLI session ID is sufficient.
- **Rust-native JSONL handling** — `serde_json` line-by-line is trivial and
  much more efficient than the Node.js approach.

### What to be careful about
- **Token counting is always an estimate** — OpenClaw uses a 1.2x safety margin.
  When using CLIs, let them handle this entirely.
- **Session expiry policy matters** — 4 AM daily reset is clever (fresh start each day)
  but idle timeout is more important for a home assistant.
- **Tree-structured transcripts add complexity** — unless we need branching
  (unlikely for a home assistant), linear JSONL is simpler and sufficient.
