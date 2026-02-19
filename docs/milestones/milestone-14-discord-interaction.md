# Milestone 14 — Discord Interaction Improvements

**Crates:** `cli-wrapper`, `conversation`, `discord`, `server`
**Complexity:** Large
**Dependencies:** Milestone 3 (conversation engine), Milestone 4 (Discord bot), Milestone 12 (memory/heartbeat)

## What This Milestone Delivers

A fundamentally better Discord interaction experience. Today, when a user sends a message, the bot shows "typing..." for up to 5 minutes and then dumps the full response. This milestone transforms that into a teammate-like experience:

1. **Immediate acknowledgment** — A fast Haiku-generated response appears within seconds: *"Got it — I'll clone the repo and draft the initial plan. Give me a few minutes."*
2. **Live status updates** — A single Discord message is periodically edited with Haiku-summarized progress: *"Reading repository structure... found 47 files across 8 crates"* → *"Drafting planning document... 3 sections complete"*
3. **No timeout ceiling** — Long-running tasks (hours) are supported with configurable limits.
4. **Abort command** — `/abort` kills the running task for the current conversation.

### What This Does NOT Do

- **No automatic file posting** — Files are shared only when the agent or user explicitly requests it. The existing artifact/attachment infrastructure handles this.
- **No two-phase execution** — No forced plan-then-execute pattern. The agent works naturally; the ack and status updates are observational, not directive.
- **No burden on the working agent** — The main Claude invocation is untouched. All status intelligence comes from observing its streaming output externally.

---

## Architecture

### Current Flow

```
User message → typing indicator → Claude CLI (blocks 0-300s) → full response → Discord
```

### New Flow

```
User message
  ├─ [immediate]  Haiku ack call ──→ ack message to Discord
  ├─ [immediate]  Main Claude CLI (streaming) ──→ stream events
  │                  │
  │                  ├─ [every ~30s] Haiku summarizes recent events ──→ edit status message
  │                  ├─ [every ~30s] ...
  │                  └─ [complete]   Final response ──→ response messages to Discord
  │                                                    delete status message
  └─ [on /abort]   Kill CLI process ──→ "Task aborted" to Discord
```

### Key Design Decisions

1. **Haiku via CLI, not API** — Both ack and status calls use the same `CliProcess` mechanism (spawning `claude -p --model haiku`). No new API client needed. These are stateless calls with no session management.

2. **Streaming via `--output-format stream-json`** — The Claude CLI supports JSONL streaming output. Each line is a JSON event (text delta, tool use, result). We read stdout line-by-line instead of buffering everything.

3. **Single editable status message** — One Discord message is created when work begins and edited periodically. This avoids flooding the channel. The message is deleted (or finalized) when the task completes.

4. **Abort via process kill** — The child process handle is stored per-conversation. `/abort` sends SIGTERM, waits briefly, then SIGKILL if needed.

5. **Concurrent ack + main invocation** — The Haiku ack and main Claude call launch simultaneously. The ack arrives in 1-2 seconds; the main call may take minutes. The `ExecutionQueue` serializes main invocations but ack calls bypass it (they're independent, session-less).

---

## Streaming Event Model

The Claude CLI with `--output-format stream-json` emits JSONL events. We parse these into a simplified internal model:

```rust
/// Events parsed from Claude CLI streaming output.
pub enum StreamEvent {
    /// Partial text content from the assistant.
    TextDelta { text: String },

    /// The assistant is using a tool.
    ToolUse {
        tool_name: String,
        /// Human-readable summary (e.g., "Reading src/main.rs")
        summary: Option<String>,
    },

    /// Tool execution result.
    ToolResult {
        tool_name: String,
        success: bool,
    },

    /// Thinking/reasoning content (if extended thinking is enabled).
    Thinking { text: String },

    /// Final result with complete response.
    Result {
        text: String,
        session_id: Option<String>,
        usage: Option<Usage>,
    },

    /// Error from the CLI.
    Error { message: String },
}
```

The streaming parser reads lines from stdout, parses each as JSON, and maps to `StreamEvent`. Unknown event types are silently ignored for forward compatibility.

---

## Haiku Integration

### Lightweight Call Mechanism

A new `HaikuClient` provides fast, stateless Claude calls:

```rust
pub struct HaikuClient {
    process: CliProcess,
}

impl HaikuClient {
    pub fn new(command: String) -> Self {
        Self {
            process: CliProcess::new(command).with_timeout(30), // 30s max
        }
    }

    /// Generate text with Haiku. No sessions, no queue.
    pub async fn generate(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let mut args = vec![
            "-p".into(),
            "--output-format".into(), "json".into(),
            "--model".into(), "haiku".into(),
        ];
        if let Some(sys) = system {
            args.push("--append-system-prompt".into());
            args.push(sys.into());
        }
        args.push(prompt.into());

        let output = self.process.run(&args, None, None).await?;
        let response = ClaudeResponse::parse(&output.stdout)?;
        Ok(response.text)
    }
}
```

### Acknowledgment Prompt

```
You are generating a brief acknowledgment for a Discord chat message.
The user just sent a message to an AI assistant. Generate a short (1-2 sentence)
acknowledgment that shows you understand what they're asking and sets expectations.
Be conversational and natural — like a teammate saying "on it."
Do NOT actually do the work. Just acknowledge.

User message: {message}
```

### Status Summary Prompt

```
You are summarizing the live activity of an AI coding assistant for a Discord status update.
Below is a log of recent events from the assistant's work. Generate a single concise line
(under 100 chars) describing what's happening right now. Use present tense.
Focus on the most recent activity. Examples:
- "Reading project structure... 12 files examined"
- "Writing implementation for user auth module"
- "Running test suite — 3 of 8 passing so far"
- "Thinking through the database schema design"

Recent events:
{events_summary}
```

---

## Process Handle Tracking

To support abort, we need to track child process handles per conversation:

```rust
/// Tracks running CLI processes by conversation ID.
/// Used for abort and status reporting.
pub struct ProcessTracker {
    processes: RwLock<HashMap<ConversationId, ProcessHandle>>,
}

pub struct ProcessHandle {
    child: tokio::process::Child,
    started_at: Instant,
    conversation_id: ConversationId,
}

impl ProcessTracker {
    pub async fn abort(&self, conversation_id: &ConversationId) -> Result<()> {
        if let Some(mut handle) = self.processes.write().await.remove(conversation_id) {
            // Try graceful shutdown first
            handle.child.kill().await?;
            Ok(())
        } else {
            Err(ThresholdError::InvalidInput {
                message: "No running task for this conversation".into(),
            })
        }
    }
}
```

The `ProcessTracker` is created at startup and shared (via `Arc`) with:
- `ClaudeClient` — registers handles when spawning, removes on completion
- Discord `/abort` command — calls `abort()` by conversation ID
- `ConversationEngine` — checks if a conversation has an active process

---

## Discord Message Lifecycle

### Status Message Flow

```
1. User sends message
2. Bot creates ack message (via Haiku): "Got it, working on this..."
3. Bot creates status message: "⏳ Starting..."
4. [30s later] Bot edits status message: "📖 Reading src/lib.rs..."
5. [30s later] Bot edits status message: "✏️ Writing new module..."
6. [complete]  Bot deletes status message
7. Bot sends final response (chunked as usual)
```

The portal listener needs to track the status message ID so it can edit it:

```rust
// In portal_listener, track the editable status message
let mut status_message_id: Option<MessageId> = None;

match event {
    ConversationEvent::StatusUpdate { conversation_id: cid, summary } if cid == conversation_id => {
        if let Some(msg_id) = status_message_id {
            // Edit existing message
            channel_id.edit_message(&http, msg_id, EditMessage::new().content(&summary)).await.ok();
        } else {
            // Create new status message
            if let Ok(msg) = channel_id.say(&http, &summary).await {
                status_message_id = Some(msg.id);
            }
        }
    }
    ConversationEvent::AssistantMessage { .. } => {
        // Delete status message before sending response
        if let Some(msg_id) = status_message_id.take() {
            channel_id.delete_message(&http, msg_id).await.ok();
        }
        // ... send response as usual
    }
}
```

### New ConversationEvent Variants

```rust
pub enum ConversationEvent {
    // ... existing variants ...

    /// Quick acknowledgment before main processing starts.
    Acknowledgment {
        conversation_id: ConversationId,
        content: String,
    },

    /// Live status update during processing (edit, don't post new).
    StatusUpdate {
        conversation_id: ConversationId,
        summary: String,
        elapsed_secs: u64,
    },

    /// Task was aborted by user.
    Aborted {
        conversation_id: ConversationId,
    },
}
```

---

## Abort Command

### `/abort` Slash Command

```rust
/// Abort the running task for this channel's conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn abort(ctx: Context<'_>) -> Result {
    let portal_id = resolve_portal(ctx).await;
    let conversation_id = ctx.data().engine.get_portal_conversation(&portal_id).await?;

    match ctx.data().process_tracker.abort(&conversation_id).await {
        Ok(()) => {
            ctx.say("Task aborted.").await.ok();
        }
        Err(e) => {
            ctx.say(format!("Nothing to abort: {}", e)).await.ok();
        }
    }

    Ok(())
}
```

### Abort Flow

1. User sends `/abort`
2. Command resolves conversation from portal
3. `ProcessTracker::abort()` sends SIGKILL to the child process
4. The streaming reader in `ClaudeClient` sees the process exit
5. `ClaudeClient` returns an `Err(ThresholdError::Aborted)` (new variant)
6. `ConversationEngine` broadcasts `ConversationEvent::Aborted`
7. Portal listener sends "Task aborted." to Discord
8. Portal listener deletes the status message

---

## Timeout Configuration

### Config Changes

```toml
[cli.claude]
# Maximum time for a single Claude invocation (seconds).
# Set to 0 for no timeout. Default: 21600 (6 hours).
timeout_seconds = 21600
```

### Implementation

- `CliProcess::with_timeout()` already exists — just change the default
- When `timeout_seconds = 0`, skip the `tokio::select!` timeout branch entirely
- The streaming reader naturally handles long-running processes (it reads lines as they arrive)
- Config field `timeout_seconds` already exists in `ClaudeCliConfig` but defaults to 300s in `CliProcess::new()`

---

## Implementation Phases

### Phase 14A — Timeout Removal + Abort Infrastructure

**Goal:** Unblock long-running tasks immediately. This is the quickest win.

**Files:**
| File | Change |
|------|--------|
| `crates/core/src/config.rs` | Default `timeout_seconds` to 21600 (6h) |
| `crates/cli-wrapper/src/process.rs` | Support `timeout_secs = 0` (no timeout), pass config timeout |
| `crates/cli-wrapper/src/claude.rs` | Accept timeout from config, create `ProcessTracker`, store child handles |
| `crates/cli-wrapper/src/tracker.rs` | **New**: `ProcessTracker` struct with abort support |
| `crates/core/src/types.rs` | Add `ThresholdError::Aborted` variant |
| `crates/discord/src/commands.rs` | Add `/abort` command |
| `crates/discord/src/bot.rs` | Register `/abort` command, share `ProcessTracker` via `BotData` |
| `crates/server/src/main.rs` | Create `ProcessTracker` at startup, inject into client + Discord |
| `config.example.toml` | Document `timeout_seconds` default change |

**Tests:**
- `tracker::abort_kills_process` — spawn a sleep process, abort it, verify killed
- `tracker::abort_nonexistent_returns_error`
- `process::no_timeout_when_zero` — verify process runs beyond old 300s limit (use short test)
- `/abort` command resolves conversation and calls tracker

### Phase 14B — Streaming CLI Process

**Goal:** Read Claude CLI output incrementally instead of all-at-once.

**Files:**
| File | Change |
|------|--------|
| `crates/cli-wrapper/src/stream.rs` | **New**: `StreamEvent` enum, JSONL parser, `StreamingProcess` |
| `crates/cli-wrapper/src/process.rs` | Add `run_streaming()` → returns `mpsc::Receiver<StreamEvent>` |
| `crates/cli-wrapper/src/claude.rs` | Add `send_message_streaming()` that returns stream receiver |
| `crates/cli-wrapper/src/response.rs` | Factor out JSON field extraction for reuse in streaming parser |
| `crates/conversation/src/engine.rs` | Add `handle_message_streaming()` that broadcasts stream events |

**Key implementation detail:** `run_streaming()` spawns the CLI with `--output-format stream-json`, then spawns a tokio task that reads stdout line-by-line via `BufReader::lines()` and sends parsed `StreamEvent`s through an `mpsc` channel. The caller receives events as they arrive.

The `ExecutionQueue` still serializes invocations — the queue lock is held for the duration of the streaming process, same as today.

**Tests:**
- `stream::parse_text_delta` — parse a text delta JSONL line
- `stream::parse_tool_use` — parse a tool use event
- `stream::parse_result` — parse a final result event
- `stream::parse_unknown_type_is_ignored`
- `stream::run_streaming_echo` — spawn `echo` with JSONL, verify events received
- Integration: streaming process completes and produces a `Result` event

### Phase 14C — Immediate Acknowledgment

**Goal:** Send a fast Haiku-generated ack within 1-2 seconds of receiving a message.

**Files:**
| File | Change |
|------|--------|
| `crates/cli-wrapper/src/haiku.rs` | **New**: `HaikuClient` with `generate()` method |
| `crates/conversation/src/engine.rs` | In `handle_message()`: spawn Haiku ack concurrently with main call, broadcast `Acknowledgment` event |
| `crates/conversation/src/engine.rs` | Add `ConversationEvent::Acknowledgment` variant |
| `crates/discord/src/handler.rs` | Portal listener handles `Acknowledgment` → sends message |
| `crates/server/src/main.rs` | Create `HaikuClient`, inject into engine |
| `crates/core/src/config.rs` | Add `ack_enabled: Option<bool>` to config (default: true) |

**Flow:**
1. `handle_message()` receives user message
2. Spawns `tokio::spawn(haiku.generate(ack_prompt))` — fire and forget
3. Simultaneously starts main `send_message_streaming()` call
4. When Haiku returns (1-2s), broadcasts `Acknowledgment` event
5. Portal listener sends ack to Discord
6. Main invocation continues independently

**Tests:**
- `haiku::generate_returns_text` (requires CLI, `#[ignore]`)
- `engine::ack_event_broadcast` — mock test verifying `Acknowledgment` event is emitted
- Config: `ack_enabled = false` suppresses ack generation

### Phase 14D — Live Status Updates

**Goal:** Periodically summarize streaming activity and edit a Discord status message.

**Files:**
| File | Change |
|------|--------|
| `crates/conversation/src/engine.rs` | Add status update loop in streaming path: accumulate events, call Haiku every ~30s, broadcast `StatusUpdate` |
| `crates/conversation/src/engine.rs` | Add `ConversationEvent::StatusUpdate` and `ConversationEvent::Aborted` variants |
| `crates/discord/src/handler.rs` | Portal listener handles `StatusUpdate` → create or edit Discord message; handles `Aborted` → delete status message |
| `crates/core/src/config.rs` | Add `status_interval_seconds: Option<u64>` (default: 30) |

**Status update loop (inside streaming `handle_message`):**
```
let mut event_log = Vec::new();
let mut last_status = Instant::now();

while let Some(event) = stream_rx.recv().await {
    event_log.push(event_summary(&event));

    if last_status.elapsed() > Duration::from_secs(30) && !event_log.is_empty() {
        // Summarize recent events via Haiku
        let summary = haiku.generate(&format_status_prompt(&event_log)).await;
        broadcast(StatusUpdate { summary, elapsed_secs });
        event_log.clear();
        last_status = Instant::now();
    }

    if let StreamEvent::Result { .. } = event {
        break; // Done — send final response
    }
}
```

**Discord message editing:**
- `serenity::model::channel::Message::edit()` for updating the status message
- `ChannelId::delete_message()` to clean up the status message when done
- If editing fails (message deleted by user), create a new one

**Tests:**
- `engine::status_updates_emitted_periodically` — mock streaming with timed events
- Portal listener: verify message editing behavior
- Config: `status_interval_seconds = 0` disables status updates

---

## New Error Variant

```rust
// In crates/core/src/error.rs
pub enum ThresholdError {
    // ... existing variants ...

    /// Task was aborted by user request.
    #[error("Task aborted")]
    Aborted,
}
```

---

## Config Summary

New/changed fields:

```toml
[cli.claude]
# Maximum time for a single Claude invocation (seconds).
# 0 = no timeout. Default: 21600 (6 hours).
timeout_seconds = 21600

# Enable immediate acknowledgment via Haiku when a message is received.
# Default: true.
# ack_enabled = true

# Interval for live status updates during processing (seconds).
# 0 = disabled. Default: 30.
# status_interval_seconds = 30
```

---

## Verification

After all phases:
```bash
cargo test -p threshold-cli-wrapper --lib    # streaming, tracker, haiku tests
cargo test -p threshold-conversation --lib   # engine event tests
cargo build --workspace                      # full compilation
```

Manual testing with running daemon:
1. Send a message in Discord → verify ack appears within 2s
2. Send a complex task → verify status message updates every ~30s
3. Send `/abort` during a long task → verify process killed, "Task aborted" message
4. Verify long tasks (>5min) no longer timeout
5. Verify final response appears correctly after status messages

---

## Files Affected (Summary)

| File | Action | Phase |
|------|--------|-------|
| `crates/core/src/config.rs` | Timeout default, ack_enabled, status_interval_seconds | 14A, 14C, 14D |
| `crates/core/src/error.rs` | Add `Aborted` variant | 14A |
| `crates/cli-wrapper/src/process.rs` | Support zero timeout, `run_streaming()` | 14A, 14B |
| `crates/cli-wrapper/src/tracker.rs` | **New**: ProcessTracker | 14A |
| `crates/cli-wrapper/src/stream.rs` | **New**: StreamEvent, JSONL parser | 14B |
| `crates/cli-wrapper/src/claude.rs` | Configurable timeout, process tracking, `send_message_streaming()` | 14A, 14B |
| `crates/cli-wrapper/src/haiku.rs` | **New**: HaikuClient | 14C |
| `crates/cli-wrapper/src/lib.rs` | Re-export new modules | 14A-14C |
| `crates/conversation/src/engine.rs` | Streaming handle_message, ack, status loop, new event variants | 14B-14D |
| `crates/discord/src/handler.rs` | Portal listener: ack messages, status editing, abort cleanup | 14C, 14D |
| `crates/discord/src/commands.rs` | `/abort` command | 14A |
| `crates/discord/src/bot.rs` | Register `/abort`, share ProcessTracker | 14A |
| `crates/server/src/main.rs` | Create ProcessTracker + HaikuClient, inject, wire timeout config | 14A, 14C |
| `config.example.toml` | Document new fields | 14A |

---

## Resolved Design Questions

1. **Why Haiku via CLI, not the Anthropic API directly?** — Consistency. The entire system uses the Claude CLI. Adding a direct API client introduces a new auth path (API keys vs CLI auth), a new dependency (reqwest + API client), and a new failure mode. CLI is already proven and handles auth transparently.

2. **Why not stream raw output to Discord?** — Claude's raw stream includes file contents, tool calls, thinking blocks, and other noisy content. A user watching Discord would see hundreds of rapid edits with incomprehensible fragments. Haiku summarization distills this into human-friendly status.

3. **Why edit one message instead of posting multiple?** — Long tasks (hours) would fill the channel with hundreds of status messages. Editing one message keeps the channel clean, like a teammate's status indicator.

4. **Why not burden the main agent with status reporting?** — The working agent should focus on its task. Adding "send status updates" to its system prompt would consume tokens, slow it down, and create fragile behavior. External observation via streaming is zero-cost to the agent.

5. **Why 6-hour default timeout instead of unlimited?** — Safety net. A stuck process consuming API credits for days would be costly. 6 hours covers any reasonable task while preventing runaway billing. Users can set 0 for truly unlimited.

6. **Why not use the Agent SDK instead of CLI?** — The Agent SDK would be a much larger migration affecting the entire cli-wrapper architecture. The CLI streaming approach gets 80% of the benefit with 20% of the effort. Agent SDK migration can be a future milestone if needed.

7. **Should ack bypass ExecutionQueue?** — Yes. Haiku ack calls are stateless (no sessions) and independent of the main invocation. They should fire immediately without waiting for the queue. The `HaikuClient` has its own `CliProcess` instance that doesn't share the queue.

8. **What if Haiku ack fails?** — Silent failure. The main invocation continues regardless. An ack timeout or error is logged but doesn't affect the user's task. The old behavior (typing indicator → response) is the graceful degradation.
