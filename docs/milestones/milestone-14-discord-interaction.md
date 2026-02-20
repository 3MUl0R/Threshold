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
5. **Multi-conversation parallelism** — Conversations run concurrently instead of being serialized behind a global lock.

### What This Does NOT Do

- **No automatic file posting** — Files are shared only when the agent or user explicitly requests it. The existing artifact/attachment infrastructure handles this.
- **No two-phase execution** — No forced plan-then-execute pattern. The agent works naturally; the ack and status updates are observational, not directive.
- **No burden on the working agent** — The main Claude invocation is untouched. All status intelligence comes from observing its streaming output externally.

---

## Architecture

### Current Flow (and its problems)

```
User message → typing indicator → Claude CLI (blocks 0-300s) → full response → Discord
```

**Current architectural issues this milestone must fix:**

1. **Timeout config is ignored.** `ClaudeCliConfig.timeout_seconds` exists in config (`config.rs:36`) but is never wired through. `ClaudeClient::new()` (`claude.rs:33`) doesn't accept a timeout parameter. `CliProcess::new()` (`process.rs:33`) hardcodes `timeout_secs: 300`. The config field and `config.example.toml` give the impression it works, but it doesn't.

2. **Child process handle is private.** The `Child` is a local variable inside `CliProcess::run()` (`process.rs:91`), never exposed. There is no way for external code to reference or kill a running process. Abort requires fundamental restructuring of how the child is managed.

3. **stdout/stderr draining deadlock.** `CliProcess::run()` waits for process exit *before* reading stdout/stderr (`process.rs:117-124`). If the child produces more than the OS pipe buffer (~64KB), the child blocks on write while we wait for it to exit — classic deadlock. Long Claude responses can trigger this.

4. **Global execution lock starves all conversations.** `ExecutionQueue` is a single `Mutex<()>` (`queue.rs:10`). Every `send_message()` call acquires this lock (`claude.rs:63`). If conversation A runs for 2 hours, conversations B, C, and D all block for 2 hours. Ack and status updates don't solve throughput starvation if the main invocation for other conversations can't even start.

5. **Discord handler blocks synchronously.** `handle_message()` in `handler.rs:72` awaits the full Claude invocation inline. The handler can't send an ack before the main invocation completes because it's blocked waiting for it. The invocation needs to be spawned as a background task.

6. **Conversation deletion doesn't clean up CLI sessions.** The `ConversationDeleted` listener in `main.rs:310` only removes scheduler tasks. `SessionManager::remove()` (`session.rs:108`) is never called, leaving orphaned session mappings.

### New Flow

```
User message
  ├─ [immediate]  Handler spawns task, returns immediately (typing indicator dropped)
  ├─ [immediate]  Haiku ack call (independent, no queue) ──→ ack message to Discord
  ├─ [per-conv lock acquired]  Main Claude CLI (streaming) ──→ stream events
  │                  │
  │                  ├─ [every ~30s] Haiku summarizes recent events ──→ edit status message
  │                  ├─ [every ~30s] ...
  │                  └─ [complete or channel closed]  Final response ──→ response to Discord
  │                                                                     delete status message
  └─ [on /abort]   Kill CLI process via RunId ──→ "Task aborted" to Discord
```

### Key Design Decisions

1. **Haiku via CLI, not API** — Both ack and status calls use the same `CliProcess` mechanism (spawning `claude -p --model haiku`). No new API client needed. These are stateless calls with no session management.

2. **Streaming via `--output-format stream-json`** — The Claude CLI supports JSONL streaming output. Each line is a JSON event (text delta, tool use, result). We read stdout line-by-line instead of buffering everything. This inherently fixes the stdout draining deadlock.

3. **Single editable status message** — One Discord message is created when work begins and edited periodically. This avoids flooding the channel. The message is deleted (or finalized) when the task completes.

4. **Abort via CancellationToken → `child.kill()`** — The `ProcessTracker` stores a `CancellationToken` per run. On `/abort`, the token is cancelled. The streaming read loop (which owns the `Child`) detects cancellation and calls `child.kill()` (SIGKILL on Unix — immediate, no graceful shutdown). We don't attempt SIGTERM-then-SIGKILL because the Claude CLI is a wrapper around an API call with no meaningful cleanup.

5. **Per-conversation execution locks replace the global queue** — `ExecutionQueue` (single `Mutex<()>`) is replaced by `ConversationLockMap`, which holds a per-conversation `Mutex`. Conversations run in parallel; messages within the same conversation are serialized. This fixes the throughput starvation problem.

6. **RunId per user request** — Each invocation of `handle_message` or `send_to_conversation` generates a unique `RunId` (UUID). All events (ack, status update, final response, abort) are tagged with this ID. The portal listener uses the `RunId` to track which status message belongs to which request, and `/abort` can target a specific run.

7. **Handler spawns task, returns immediately** — The Discord message handler (`handler.rs`) spawns the engine call as a `tokio::spawn` background task and returns. This unblocks the handler for subsequent messages and enables the ack to be sent before the main invocation completes.

---

## RunId Model

Each user request gets a unique identifier. All progress events reference it.

```rust
/// Unique identifier for a single user request → agent response cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(pub Uuid);

impl RunId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
```

Benefits:
- **Status message lifecycle**: The portal listener maps `RunId → MessageId` to know which Discord message to edit for status updates and which to delete on completion.
- **Abort targeting**: `/abort` can target the active run for a conversation, or a specific `RunId` if needed.
- **Queue awareness**: Before execution starts, a "queued — waiting for previous message" status can be shown, tagged with the `RunId`.
- **Audit trail**: Each audit event can reference the `RunId` for request-level tracing.

---

## Per-Conversation Execution Locks

### Problem

The current `ExecutionQueue` (`queue.rs`) is a single `Mutex<()>`. Every Claude CLI invocation acquires this lock via `queue.execute()` (`claude.rs:63`). This means:

- Only one conversation can talk to Claude at a time, globally
- A 2-hour task in conversation A blocks conversations B, C, D for 2 hours
- Ack and status messages arrive but the main response for other conversations never starts

### Solution: `ConversationLockMap`

```rust
/// Per-conversation execution locks.
///
/// Allows multiple conversations to invoke Claude concurrently while
/// serializing messages within the same conversation (preventing session
/// race conditions).
pub struct ConversationLockMap {
    locks: RwLock<HashMap<Uuid, Arc<Mutex<()>>>>,
}

impl ConversationLockMap {
    pub fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::new()),
        }
    }

    /// Acquire the lock for a conversation. Creates it if it doesn't exist.
    pub async fn lock(&self, conversation_id: Uuid) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut map = self.locks.write().await;
            map.entry(conversation_id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }

    /// Non-blocking lock attempt. Returns `Some(guard)` if acquired,
    /// `None` if the conversation lock is already held.
    /// Used by the engine to detect "queued" status before blocking.
    pub async fn try_lock(&self, conversation_id: Uuid) -> Option<OwnedMutexGuard<()>> {
        let mutex = {
            let mut map = self.locks.write().await;
            map.entry(conversation_id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        mutex.try_lock_owned().ok()
    }

    /// Remove the lock for a deleted conversation.
    pub async fn remove(&self, conversation_id: Uuid) {
        self.locks.write().await.remove(&conversation_id);
    }
}
```

This replaces `ExecutionQueue` in `ClaudeClient`. The `send_message()` method acquires the per-conversation lock instead of the global lock:

```rust
pub async fn send_message(&self, conversation_id: Uuid, ...) -> Result<ClaudeResponse> {
    let _guard = self.locks.lock(conversation_id).await;
    // ... rest of invocation
}
```

**Session safety**: The Claude CLI uses `--session-id` per conversation. Sessions are conversation-scoped, so per-conversation locking is sufficient to prevent race conditions. Two different conversations use different session IDs and cannot collide.

### Lock Entry Lifecycle

The lock map can leak entries for conversation IDs that are used once and never deleted (e.g., one-shot scheduler tasks or conversations that are abandoned rather than explicitly deleted). Each entry is an `Arc<Mutex<()>>` — small, but unbounded growth over weeks of uptime is a concern.

**Solution:** Auto-cleanup via `Arc::strong_count`. When a lock guard is dropped and the `Arc` strong count is 1 (only the map itself holds a reference), the entry is idle and safe to remove. We implement this as a periodic sweep rather than on every unlock to avoid contention on the map's outer `RwLock`:

```rust
impl ConversationLockMap {
    /// Remove lock entries that are idle (not held by anyone).
    /// Called periodically (e.g., every 10 minutes) or after conversation deletion.
    pub async fn sweep_idle(&self) {
        let mut map = self.locks.write().await;
        map.retain(|_id, mutex| {
            // Keep entries that are actively held (strong_count > 1 means a guard exists)
            Arc::strong_count(mutex) > 1
        });
    }
}
```

The sweep is triggered from the always-on cleanup listener on a timer (every 10 minutes), colocated with the `ConversationDeleted` handler. This bounds memory growth without adding complexity to the hot path.

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

### Stream Termination Safety

The streaming loop must handle all termination scenarios, not just the `Result` event:

```rust
// The stream ends when the mpsc sender is dropped (process exited or pipe closed).
// We MUST NOT depend solely on seeing StreamEvent::Result.
let mut final_text = String::new();
let mut final_session_id = None;
let mut final_usage = None;
let mut saw_result = false;

while let Some(event) = stream_rx.recv().await {
    match &event {
        StreamEvent::Result { text, session_id, usage } => {
            final_text = text.clone();
            final_session_id = session_id.clone();
            final_usage = usage.clone();
            saw_result = true;
            break;
        }
        StreamEvent::Error { message } => {
            return Err(ThresholdError::CliError { ... });
        }
        _ => {
            // Accumulate for status updates
            event_log.push(event);
        }
    }
}

// Channel closed without Result event — process crashed or was killed
if !saw_result {
    // Check if this was an abort
    if abort_token.is_cancelled() {
        return Err(ThresholdError::Aborted);
    }
    // Otherwise, unexpected termination
    return Err(ThresholdError::CliError {
        provider: "claude".into(),
        code: -1,
        stderr: "CLI process exited without producing a result".into(),
    });
}
```

This ensures:
- Normal completion: `Result` event received, response extracted
- Abort: `abort_token` cancellation detected, `Aborted` error returned
- Crash: Channel closed without `Result`, error returned
- Unknown events: Silently ignored, loop continues

---

## Haiku Integration

### Lightweight Call Mechanism

A new `HaikuClient` provides fast, stateless Claude calls. It has its own `CliProcess` and does NOT share the `ConversationLockMap` — ack calls are independent and fire immediately.

```rust
pub struct HaikuClient {
    process: CliProcess,
}

impl HaikuClient {
    pub fn new(command: String) -> Self {
        Self {
            process: CliProcess::new(command).with_timeout(30), // 30s hard limit
        }
    }

    /// Generate text with Haiku. No sessions, no locks.
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

## Process Handle Tracking + Abort

### Architecture

The child process handle must be accessible for abort. Currently, `Child` is local to `CliProcess::run()` and never exposed. The streaming refactor solves this: `run_streaming()` registers the child with the `ProcessTracker` *before* starting the read loop, and deregisters on completion.

```rust
/// Tracks running CLI processes for abort support.
pub struct ProcessTracker {
    /// Maps RunId → abort token for the running process.
    runs: RwLock<HashMap<RunId, RunHandle>>,
}

struct RunHandle {
    abort_token: CancellationToken,
    child_pid: u32,
    conversation_id: ConversationId,
    started_at: Instant,
}

impl ProcessTracker {
    pub fn new() -> Self {
        Self { runs: RwLock::new(HashMap::new()) }
    }

    /// Register a running process. Returns the abort token to pass to the streaming loop.
    pub async fn register(
        &self,
        run_id: RunId,
        conversation_id: ConversationId,
        child_pid: u32,
    ) -> CancellationToken {
        let token = CancellationToken::new();
        self.runs.write().await.insert(run_id, RunHandle {
            abort_token: token.clone(),
            child_pid,
            conversation_id,
            started_at: Instant::now(),
        });
        token
    }

    /// Deregister a completed process.
    pub async fn deregister(&self, run_id: &RunId) {
        self.runs.write().await.remove(run_id);
    }

    /// Abort by conversation ID (kills the active run for that conversation).
    pub async fn abort_conversation(&self, conversation_id: &ConversationId) -> Result<RunId> {
        let runs = self.runs.read().await;
        let (run_id, handle) = runs.iter()
            .find(|(_, h)| h.conversation_id == *conversation_id)
            .ok_or(ThresholdError::InvalidInput {
                message: "No running task for this conversation".into(),
            })?;
        let run_id = *run_id;
        handle.abort_token.cancel();
        Ok(run_id)
    }

    /// Get the active run for a conversation, if any.
    pub async fn active_run(&self, conversation_id: &ConversationId) -> Option<RunId> {
        self.runs.read().await.iter()
            .find(|(_, h)| h.conversation_id == *conversation_id)
            .map(|(id, _)| *id)
    }
}
```

### Abort via CancellationToken (not direct Child access)

Rather than passing the `Child` handle around (which creates ownership issues), the `ProcessTracker` stores a `CancellationToken` per run. The streaming read loop monitors this token:

```rust
// Inside run_streaming():
loop {
    tokio::select! {
        line = reader.next_line() => {
            match line {
                Ok(Some(line)) => { /* parse and send event */ }
                Ok(None) => break, // stdout closed — process exited
                Err(e) => { /* send error event, break */ }
            }
        }
        _ = abort_token.cancelled() => {
            // Kill the child process immediately
            let _ = child.kill().await;
            // Send aborted event through channel
            let _ = event_tx.send(StreamEvent::Error {
                message: "Aborted by user".into(),
            });
            break;
        }
    }
}
```

This avoids the ownership problem of sharing `Child` across multiple locations. The abort token is the signal; the streaming loop owns the child and kills it when the token fires.

### `/abort` Slash Command

```rust
/// Abort the running task for this channel's conversation.
#[poise::command(slash_command, prefix_command)]
pub async fn abort(ctx: Context<'_>) -> Result {
    let portal_id = resolve_portal(ctx).await;
    let conversation_id = ctx.data().engine.get_portal_conversation(&portal_id).await?;

    match ctx.data().process_tracker.abort_conversation(&conversation_id).await {
        Ok(run_id) => {
            ctx.say(format!("Aborting task {}...", &run_id.0.to_string()[..8])).await.ok();
        }
        Err(_) => {
            ctx.say("Nothing to abort — no task is running for this conversation.").await.ok();
        }
    }

    Ok(())
}
```

### Abort Flow (Definitive)

1. User sends `/abort`
2. Command resolves conversation from portal
3. `ProcessTracker::abort_conversation()` cancels the `CancellationToken` for the active run
4. The streaming read loop in `run_streaming()` detects cancellation
5. The loop calls `child.kill().await` (sends SIGKILL on Unix — immediate, no graceful shutdown)
6. The loop sends `StreamEvent::Error { message: "Aborted by user" }` and exits
7. The engine detects the aborted error and broadcasts `ConversationEvent::Aborted { run_id }`
8. The portal listener receives the event, sends "Task aborted." to Discord, deletes the status message

---

## Timeout Wiring Fix

### Current Bug

The timeout config field exists but is never connected:

```
config.toml: timeout_seconds = 600
    ↓ parsed into ClaudeCliConfig.timeout_seconds (config.rs:36)
    ✗ IGNORED — ClaudeClient::new() doesn't accept timeout (claude.rs:33)
    ✗ IGNORED — CliProcess::new() hardcodes 300s (process.rs:33)
```

### Fix

1. **`ClaudeClient::new()` accepts timeout** — Add `timeout_secs: u64` parameter
2. **`main.rs` wires config value** — `config.cli.claude.timeout_seconds.unwrap_or(21600)`
3. **`CliProcess` receives configured timeout** — `CliProcess::new(command).with_timeout(timeout_secs)`
4. **Zero means no timeout** — When `timeout_secs == 0`, the streaming loop has no timeout select branch. For the legacy `run()` method (kept for Haiku and health checks), `timeout_secs == 0` disables the `tokio::time::sleep` branch in the `select!`.
5. **Default changes from 300s to 21600s (6 hours)** — In `config.example.toml` and code

---

## stdout/stderr Draining Fix

### Current Bug

`CliProcess::run()` has a deadlock:

```rust
// process.rs:117-124 (current)
result = child.wait() => {
    // Process exited — NOW read output
    let _ = BufReader::new(stdout).read_to_end(&mut stdout_buf).await;
    let _ = BufReader::new(stderr).read_to_end(&mut stderr_buf).await;
}
```

If the child produces more output than the OS pipe buffer (~64KB on most systems), the child blocks trying to write to stdout. But we're waiting for the child to exit before reading. Deadlock.

### Fix

For the non-streaming `run()` method (used by `HaikuClient` and `health_check()`), drain stdout/stderr concurrently with the wait:

```rust
// Fixed: drain stdout/stderr concurrently with wait
let stdout_task = tokio::spawn(async move {
    let mut buf = Vec::new();
    let _ = BufReader::new(stdout).read_to_end(&mut buf).await;
    buf
});
let stderr_task = tokio::spawn(async move {
    let mut buf = Vec::new();
    let _ = BufReader::new(stderr).read_to_end(&mut buf).await;
    buf
});

tokio::select! {
    result = child.wait() => {
        let status = result?;
        let stdout_buf = stdout_task.await.unwrap_or_default();
        let stderr_buf = stderr_task.await.unwrap_or_default();
        // ... build CliOutput
    }
    _ = tokio::time::sleep(timeout) => {
        let _ = child.kill().await;
        stdout_task.abort();
        stderr_task.abort();
        return Err(ThresholdError::CliTimeout { .. });
    }
}
```

For the streaming `run_streaming()` method (main path), this is inherently not an issue — stdout is read line-by-line as data arrives, so the pipe never fills.

---

## Discord Handler: Spawn-and-Return

### Current Bug

```rust
// handler.rs:72 (current)
data.engine.handle_message(&portal_id, &msg.content).await?;
```

The handler blocks until the full Claude invocation completes. This means:
- The handler can't send an ack before the invocation finishes
- The `_typing` indicator is held for the entire duration (actually fine, but misleading now)
- For multi-hour tasks, the handler coroutine is blocked for hours

### Fix

The handler spawns the engine call as a background task and returns immediately:

```rust
// handler.rs (new)
async fn handle_message(ctx, msg, data) -> Result<()> {
    // ... authorization, portal resolution, listener setup (same as before)

    // Spawn the engine call as a background task
    let engine = data.engine.clone();
    let portal_id = portal_id;
    let content = msg.content.clone();
    tokio::spawn(async move {
        if let Err(e) = engine.handle_message(&portal_id, &content).await {
            tracing::error!(
                error = %e,
                portal_id = ?portal_id,
                "Background message handling failed"
            );
            // Error is broadcast via ConversationEvent::Error by the engine
        }
    });

    Ok(())
}
```

The response still reaches Discord through the existing portal listener (which subscribes to broadcast events). The handler no longer needs to wait for the response.

---

## Conversation Deletion: Always-On Cleanup Listener

### Current Bug

The `ConversationDeleted` listener in `main.rs:310` lives inside the `if let Some(sched_handle) = scheduler_cmd_handle` block. If the scheduler is disabled, the listener is never spawned, and no cleanup happens at all — not for scheduler tasks, not for session mappings, not for conversation locks. This is not merely an "also clean up sessions" fix; the listener must be restructured to run unconditionally.

### Fix

Create an **always-on** `ConversationDeleted` listener in `main.rs` that is spawned unconditionally (outside any scheduler conditional). Each cleanup action is individually guarded:

```rust
// In main.rs — spawned UNCONDITIONALLY after engine creation:
{
    let mut event_rx = engine.subscribe();
    let cancel_clone = cancel.clone();
    let session_manager = session_manager.clone();
    let conversation_locks = conversation_locks.clone();
    let sched_handle = scheduler_cmd_handle.clone(); // Option<SchedulerHandle>
    let sweep_interval = tokio::time::interval(std::time::Duration::from_secs(600)); // 10 min
    tokio::spawn(async move {
        tokio::pin!(sweep_interval);
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    match event {
                        Ok(ConversationEvent::ConversationDeleted { conversation_id }) => {
                            // 1. Scheduler cleanup (only if scheduler is enabled)
                            if let Some(handle) = &sched_handle {
                                if let Err(e) = handle.remove_tasks_for_conversation(conversation_id) {
                                    tracing::warn!("Failed to remove scheduler tasks: {}", e);
                                }
                            }
                            // 2. CLI session mapping cleanup (always)
                            if let Err(e) = session_manager.remove(conversation_id.0).await {
                                tracing::warn!("Failed to remove CLI session: {}", e);
                            }
                            // 3. Conversation lock cleanup (always)
                            conversation_locks.remove(conversation_id.0).await;
                        }
                        Ok(_) => {} // ignore other events
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Deletion listener lagged by {} events", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = sweep_interval.tick() => {
                    // Periodic sweep of idle lock entries (see Lock Entry Lifecycle)
                    conversation_locks.sweep_idle().await;
                }
                _ = cancel_clone.cancelled() => break,
            }
        }
    });
}
```

The existing scheduler-only listener (section 9a in `main.rs:310-338`) is **deleted entirely** — this new always-on listener replaces it.

**Design:** `SessionManager` is created in `main.rs` and shared with both `ClaudeClient::new()` and the listener via `Arc`. This avoids adding a public accessor to `ClaudeClient`.

---

## Discord Message Lifecycle

### Status Message Flow (with RunId)

```
1. User sends message → handler spawns background task with RunId = abc123
2. Engine broadcasts Acknowledgment { run_id: abc123, content: "Got it, ..." }
3. Portal listener sends ack to Discord
4. Engine acquires per-conversation lock (may queue)
   - If queued: broadcasts StatusUpdate { run_id: abc123, summary: "Queued — waiting..." }
5. Lock acquired, streaming starts
6. [30s later] Engine broadcasts StatusUpdate { run_id: abc123, summary: "Reading src/lib.rs..." }
7. Portal listener creates status message, stores mapping: abc123 → MessageId(456)
8. [30s later] Engine broadcasts StatusUpdate { run_id: abc123, summary: "Writing module..." }
9. Portal listener edits MessageId(456) with new summary
10. [complete]  Engine broadcasts AssistantMessage { run_id: abc123, content: "..." }
11. Portal listener deletes MessageId(456), sends final response
```

### Portal Listener Changes

```rust
// In portal_listener, track status messages by RunId
let mut status_messages: HashMap<RunId, MessageId> = HashMap::new();

match event {
    ConversationEvent::Acknowledgment { run_id, content, .. } if cid == conversation_id => {
        // Send ack as a new message
        channel_id.say(&http, &content).await.ok();
    }

    ConversationEvent::StatusUpdate { run_id, summary, .. } if cid == conversation_id => {
        if let Some(msg_id) = status_messages.get(&run_id) {
            // Edit existing status message
            let edit = serenity::builder::EditMessage::new().content(&summary);
            channel_id.edit_message(&http, *msg_id, edit).await.ok();
        } else {
            // Create new status message
            if let Ok(msg) = channel_id.say(&http, &summary).await {
                status_messages.insert(run_id, msg.id);
            }
        }
    }

    ConversationEvent::AssistantMessage { run_id, .. } if cid == conversation_id => {
        // Delete status message for this run
        if let Some(msg_id) = status_messages.remove(&run_id) {
            channel_id.delete_message(&http, msg_id).await.ok();
        }
        // Send response (chunked, same as before)
        // ...
    }

    ConversationEvent::Aborted { run_id, .. } if cid == conversation_id => {
        // Delete status message and notify
        if let Some(msg_id) = status_messages.remove(&run_id) {
            channel_id.delete_message(&http, msg_id).await.ok();
        }
        channel_id.say(&http, "Task aborted.").await.ok();
    }
}
```

### Updated ConversationEvent Variants

All events now carry `RunId` for request-level tracking:

```rust
pub enum ConversationEvent {
    // Existing (add run_id to AssistantMessage):
    AssistantMessage {
        conversation_id: ConversationId,
        run_id: RunId,
        content: String,
        artifacts: Vec<Artifact>,
        usage: Option<Usage>,
        timestamp: DateTime<Utc>,
    },
    Error {
        conversation_id: ConversationId,
        run_id: Option<RunId>,  // None for errors outside a run
        error: String,
    },

    // Existing (unchanged):
    ConversationCreated { conversation: Conversation },
    PortalAttached { portal_id: PortalId, conversation_id: ConversationId },
    PortalDetached { portal_id: PortalId, conversation_id: ConversationId },
    ConversationDeleted { conversation_id: ConversationId },

    // New:
    Acknowledgment {
        conversation_id: ConversationId,
        run_id: RunId,
        content: String,
    },
    StatusUpdate {
        conversation_id: ConversationId,
        run_id: RunId,
        summary: String,
        elapsed_secs: u64,
    },
    Aborted {
        conversation_id: ConversationId,
        run_id: RunId,
    },
}
```

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

- Default changes from 300s → 21600s (6 hours)
- `timeout_seconds = 0` disables the timeout entirely
- For streaming: the streaming loop has no timeout — it runs until the process exits, the channel closes, or the abort token fires
- For non-streaming `run()` (used by Haiku/health check): `timeout_secs == 0` skips the `tokio::time::sleep` branch
- The 6-hour safety net prevents runaway billing from stuck processes; `/abort` provides manual override

---

## Implementation Phases

### Phase 14A — Fix Foundations (stdout, timeout wiring, per-conversation locks, handler spawn, session cleanup)

**Goal:** Fix the architectural issues that block everything else. No new user-facing features yet (except timeout lift), but the foundation is solid.

**Changes:**

| File | Change |
|------|--------|
| `crates/cli-wrapper/src/process.rs` | Fix stdout/stderr deadlock: drain concurrently via spawned tasks. Support `timeout_secs = 0`. |
| `crates/cli-wrapper/src/claude.rs` | Accept `timeout_secs` and externally-created `Arc<SessionManager>` in `new()`. Replace `ExecutionQueue` with `ConversationLockMap`. |
| `crates/cli-wrapper/src/locks.rs` | **New**: `ConversationLockMap` with per-conversation `Mutex`, `try_lock()`, `sweep_idle()`. |
| `crates/cli-wrapper/src/queue.rs` | **Delete** (replaced by `locks.rs`). |
| `crates/core/src/types.rs` | Add `RunId` struct. |
| `crates/core/src/error.rs` | Add `#[error("Task aborted")] Aborted` variant to `ThresholdError`. |
| `crates/discord/src/handler.rs` | Spawn `engine.handle_message()` as background task instead of awaiting inline. |
| `crates/server/src/main.rs` | Wire `timeout_seconds` from config into `ClaudeClient::new()`. Replace scheduler-only `ConversationDeleted` listener (section 9a) with always-on listener that handles session cleanup, lock cleanup, and optional scheduler cleanup. Share `SessionManager` and `ConversationLockMap` with listener. |
| `config.example.toml` | Update `timeout_seconds` default to 21600, add comment about 0 = unlimited. |

**Tests:**
- `process::concurrent_drain_large_output` — child produces >64KB, verify no deadlock
- `process::no_timeout_when_zero` — verify no timeout fires
- `locks::multiple_conversations_run_concurrently` — two conversations don't block each other
- `locks::same_conversation_serialized` — two messages to same conversation are sequential
- `locks::try_lock_returns_none_when_held` — verify `try_lock()` returns `None` while lock is held
- `locks::try_lock_returns_guard_when_free` — verify `try_lock()` acquires successfully
- `locks::remove_cleans_up` — verify lock removal
- `locks::sweep_idle_removes_unused_entries` — verify `sweep_idle()` removes entries with `strong_count == 1`
- `locks::sweep_idle_keeps_held_entries` — verify `sweep_idle()` retains entries where a guard is active

### Phase 14B — Streaming CLI Process + Process Tracking + Abort

**Goal:** Stream Claude output line-by-line. Track processes for abort. Add `/abort` command.

**Changes:**

| File | Change |
|------|--------|
| `crates/cli-wrapper/src/stream.rs` | **New**: `StreamEvent` enum, JSONL line parser. |
| `crates/cli-wrapper/src/process.rs` | Add `run_streaming()` → spawns reader task, returns `mpsc::Receiver<StreamEvent>`. Registers child with `ProcessTracker`. Monitors abort token. |
| `crates/cli-wrapper/src/tracker.rs` | **New**: `ProcessTracker` with `register()`, `deregister()`, `abort_conversation()`, `active_run()`. Uses `CancellationToken` per run. |
| `crates/cli-wrapper/src/claude.rs` | Add `send_message_streaming()` → uses `run_streaming()`, returns event receiver. |
| `crates/cli-wrapper/src/response.rs` | Factor out JSON field extraction helpers for reuse in streaming parser. |
| `crates/conversation/src/engine.rs` | Switch `handle_message()` to use streaming. Add `RunId` generation. Consume stream events, build final response. Broadcast `AssistantMessage` with `run_id`. |
| `crates/discord/src/commands.rs` | Add `/abort` command. |
| `crates/discord/src/bot.rs` | Register `/abort`, add `ProcessTracker` to `BotData`. |
| `crates/server/src/main.rs` | Create `ProcessTracker` at startup, inject into `ClaudeClient` and `BotData`. |

**Stream termination safety:** The streaming loop exits on:
1. `StreamEvent::Result` received → normal completion
2. `recv()` returns `None` (channel closed) without `Result` → check abort token, return appropriate error
3. `StreamEvent::Error` received → return error
4. Abort token cancelled → `child.kill()`, return `Aborted`

**Tests:**
- `stream::parse_text_delta` — parse a text delta JSONL line
- `stream::parse_tool_use` — parse a tool use event
- `stream::parse_result` — parse a final result event
- `stream::parse_unknown_type_ignored` — unknown types don't break parser
- `stream::channel_close_without_result` — verify error, not hang
- `tracker::register_and_abort` — register a run, abort it, verify token cancelled
- `tracker::abort_nonexistent_returns_error`
- `tracker::deregister_cleans_up`
- Integration: `/abort` kills a running `sleep` process

### Phase 14C — Immediate Acknowledgment

**Goal:** Send a fast Haiku-generated ack within 1-2 seconds of receiving a message.

**Changes:**

| File | Change |
|------|--------|
| `crates/cli-wrapper/src/haiku.rs` | **New**: `HaikuClient` with `generate()` method. Own `CliProcess` with 30s timeout. |
| `crates/conversation/src/engine.rs` | In `handle_message()`: spawn Haiku ack concurrently with main invocation. Broadcast `Acknowledgment` event. Accept `HaikuClient` in constructor. |
| `crates/conversation/src/engine.rs` | Add `ConversationEvent::Acknowledgment` variant. |
| `crates/discord/src/handler.rs` | Portal listener handles `Acknowledgment` → sends message to Discord. |
| `crates/server/src/main.rs` | Create `HaikuClient`, inject into `ConversationEngine`. |
| `crates/core/src/config.rs` | Add `ack_enabled: Option<bool>` to `ClaudeCliConfig` (default: true). |
| 7 test sites across 6 files (see Config Field Rollout) | Add `ack_enabled: None` to all `ClaudeCliConfig` struct literals. |

**Flow:**
1. `handle_message()` generates `RunId`
2. Spawns `tokio::spawn(haiku.generate(ack_prompt))` — independent, no lock needed
3. Simultaneously acquires per-conversation lock and starts main streaming invocation
4. When Haiku returns (1-2s), broadcasts `Acknowledgment { run_id, content }`
5. Portal listener sends ack to Discord
6. If Haiku fails (timeout, error), silently logged — no impact on main invocation

**Tests:**
- `haiku::generate_returns_text` (requires CLI, `#[ignore]`)
- `engine::ack_event_broadcast` — verify `Acknowledgment` event emitted
- Config: `ack_enabled = false` suppresses ack generation

### Phase 14D — Live Status Updates

**Goal:** Periodically summarize streaming activity and edit a Discord status message.

**Changes:**

| File | Change |
|------|--------|
| `crates/conversation/src/engine.rs` | Add status update loop: accumulate stream events, call Haiku every ~30s, broadcast `StatusUpdate`. Show "Queued — waiting for previous message" while waiting for lock. |
| `crates/conversation/src/engine.rs` | Add `ConversationEvent::StatusUpdate` and `ConversationEvent::Aborted` variants. |
| `crates/discord/src/handler.rs` | Portal listener handles `StatusUpdate` → create or edit Discord message (tracked by `RunId → MessageId`). Handles `Aborted` → delete status message. |
| `crates/core/src/config.rs` | Add `status_interval_seconds: Option<u64>` to `ClaudeCliConfig` (default: 30, 0 = disabled). |
| 7 test sites across 6 files (see Config Field Rollout) | Add `status_interval_seconds: None` to all `ClaudeCliConfig` struct literals. |

**Status update loop (inside streaming `handle_message`):**
```rust
let mut event_log: Vec<String> = Vec::new();
let mut last_status = Instant::now();

while let Some(event) = stream_rx.recv().await {
    event_log.push(event_summary(&event));

    if last_status.elapsed() >= Duration::from_secs(status_interval) && !event_log.is_empty() {
        // Summarize recent events via Haiku (fire and forget)
        let summary_result = haiku.generate(&format_status_prompt(&event_log)).await;
        if let Ok(summary) = summary_result {
            broadcast(StatusUpdate { run_id, summary, elapsed_secs });
        }
        event_log.clear();
        last_status = Instant::now();
    }

    match event {
        StreamEvent::Result { .. } => break,
        StreamEvent::Error { .. } => { /* handle error */ break; }
        _ => {} // continue accumulating
    }
}
// On exit: broadcast AssistantMessage or Aborted (handled by caller)
```

**Queue awareness:** The engine uses `try_lock()` before blocking on `lock()`. If `try_lock()` returns `None`, the lock is already held, so the engine broadcasts a `StatusUpdate` with summary "Queued — waiting for previous message to complete", then falls through to the blocking `lock()` call:

```rust
let guard = match conversation_locks.try_lock(conversation_id).await {
    Some(guard) => guard, // lock acquired immediately, no queue status needed
    None => {
        // Another message is processing — show queued status
        broadcast(StatusUpdate { run_id, summary: "Queued — waiting for previous message...".into(), .. });
        conversation_locks.lock(conversation_id).await // block until acquired
    }
};
```

**Tests:**
- `engine::status_updates_emitted_periodically` — mock streaming with timed events
- `engine::queue_status_shown_when_waiting` — verify "queued" status when lock is held
- Portal listener: verify message create → edit → delete lifecycle
- Config: `status_interval_seconds = 0` disables status updates

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

### Config Field Rollout: Struct Literal Updates

Adding `ack_enabled` and `status_interval_seconds` to `ClaudeCliConfig` will break every test file that constructs a `ClaudeCliConfig` struct literal (because Rust struct literals require all fields). All new fields use `Option<T>` with `#[serde(default)]` so deserialization is unaffected, but test code that constructs struct literals directly must be updated.

**All `ClaudeCliConfig` struct literal locations (7 sites across 6 files, all test code):**

| File | Lines | Context |
|------|-------|---------|
| `crates/web/src/lib.rs` | 90-96 | `test_app_state()` helper |
| `crates/web/tests/e2e_server.rs` | 24-30 | E2E test config setup |
| `crates/tools/src/prompt.rs` | 81-87 | Tool prompt builder tests |
| `crates/discord/src/portals.rs` | 48-54 | Portal resolution tests |
| `crates/conversation/src/engine.rs` | 890-896 | `test_config()` helper |
| `crates/conversation/src/engine.rs` | 1155-1161 | `make_engine_with_dir()` helper |
| `crates/scheduler/src/engine.rs` | 519-525 | Scheduler engine tests |

**Strategy:** Add both fields as `Option<T>` (matching the existing pattern for `command`, `model`, `timeout_seconds`, `skip_permissions`). Test literals set them to `None`. This is the established pattern — see how `command: None`, `model: None`, etc. are already used in these literals. Each phase that adds a config field must update all 7 sites — the compiler will enforce this via exhaustive struct literal checking.

**Phase rollout:**
- Phase 14C adds `ack_enabled: Option<bool>` → update all 7 sites to include `ack_enabled: None`
- Phase 14D adds `status_interval_seconds: Option<u64>` → update all 7 sites to include `status_interval_seconds: None`

---

## Verification

After all phases:
```bash
cargo test -p threshold-cli-wrapper --lib    # streaming, tracker, haiku, locks tests
cargo test -p threshold-conversation --lib   # engine event tests
cargo build --workspace                      # full compilation
```

Manual testing with running daemon:
1. Send a message in Discord → verify ack appears within 2s
2. Send a complex task → verify status message updates every ~30s
3. Send `/abort` during a long task → verify process killed, "Task aborted" message
4. Verify long tasks (>5min) no longer timeout
5. Verify final response appears correctly after status messages
6. Send messages to two different conversations simultaneously → verify both proceed in parallel
7. Send two messages to the same conversation → verify second shows "queued" then processes after first completes
8. Delete a conversation → verify session mapping cleaned up (check cli-sessions.json)

---

## Files Affected (Summary)

| File | Action | Phase |
|------|--------|-------|
| `crates/core/src/types.rs` | Add `RunId` | 14A |
| `crates/core/src/error.rs` | Add `Aborted` variant | 14A |
| `crates/core/src/config.rs` | `ack_enabled`, `status_interval_seconds` fields | 14C, 14D |
| `crates/cli-wrapper/src/process.rs` | Fix stdout drain, support zero timeout, add `run_streaming()` | 14A, 14B |
| `crates/cli-wrapper/src/locks.rs` | **New**: `ConversationLockMap` | 14A |
| `crates/cli-wrapper/src/queue.rs` | **Delete** (replaced by locks.rs) | 14A |
| `crates/cli-wrapper/src/stream.rs` | **New**: `StreamEvent`, JSONL parser | 14B |
| `crates/cli-wrapper/src/tracker.rs` | **New**: `ProcessTracker` with CancellationToken-based abort | 14B |
| `crates/cli-wrapper/src/claude.rs` | Accept timeout + shared SessionManager, per-conversation locks, streaming send | 14A, 14B |
| `crates/cli-wrapper/src/response.rs` | Factor out JSON helpers | 14B |
| `crates/cli-wrapper/src/haiku.rs` | **New**: `HaikuClient` | 14C |
| `crates/cli-wrapper/src/lib.rs` | Re-export new modules | 14A-14C |
| `crates/conversation/src/engine.rs` | RunId generation, streaming message handling, ack, status loop, new events | 14A-14D |
| `crates/discord/src/handler.rs` | Spawn-and-return handler, portal listener: ack, status editing, abort | 14A, 14C, 14D |
| `crates/discord/src/commands.rs` | `/abort` command | 14B |
| `crates/discord/src/bot.rs` | Register `/abort`, share ProcessTracker | 14B |
| `crates/server/src/main.rs` | Wire timeout, always-on deletion listener, create ProcessTracker + HaikuClient | 14A, 14B, 14C |
| `config.example.toml` | Document new/changed fields | 14A |

---

## Resolved Design Questions

1. **Why Haiku via CLI, not the Anthropic API directly?** — Consistency. The entire system uses the Claude CLI. Adding a direct API client introduces a new auth path (API keys vs CLI auth), a new dependency (reqwest + API client), and a new failure mode. CLI is already proven and handles auth transparently.

2. **Why not stream raw output to Discord?** — Claude's raw stream includes file contents, tool calls, thinking blocks, and other noisy content. A user watching Discord would see hundreds of rapid edits with incomprehensible fragments. Haiku summarization distills this into human-friendly status.

3. **Why edit one message instead of posting multiple?** — Long tasks (hours) would fill the channel with hundreds of status messages. Editing one message keeps the channel clean, like a teammate's status indicator.

4. **Why not burden the main agent with status reporting?** — The working agent should focus on its task. Adding "send status updates" to its system prompt would consume tokens, slow it down, and create fragile behavior. External observation via streaming is zero-cost to the agent.

5. **Why 6-hour default timeout instead of unlimited?** — Safety net. A stuck process consuming API credits for days would be costly. 6 hours covers any reasonable task while preventing runaway billing. Users can set 0 for truly unlimited.

6. **Why not use the Agent SDK instead of CLI?** — The Agent SDK would be a much larger migration affecting the entire cli-wrapper architecture. The CLI streaming approach gets 80% of the benefit with 20% of the effort. Agent SDK migration can be a future milestone if needed.

7. **Should ack bypass per-conversation locks?** — Yes. Haiku ack calls are stateless (no sessions) and independent of the main invocation. They use their own `CliProcess` instance with no lock. They fire immediately even if the conversation lock is held.

8. **What if Haiku ack fails?** — Silent failure. The main invocation continues regardless. An ack timeout or error is logged but doesn't affect the user's task. The old behavior (typing indicator → response) is the graceful degradation.

9. **Why CancellationToken for abort instead of passing Child handle?** — Ownership. The `Child` must be owned by the task that reads its stdout. Passing it to both the reader and the tracker creates shared mutable state issues. The CancellationToken is a clean signal: the tracker cancels it, the reader (which owns the Child) performs the actual kill.

10. **Why per-conversation locks instead of keeping the global queue?** — Product requirement. Multi-channel responsiveness means conversation B shouldn't wait hours for conversation A. Per-conversation locks allow parallelism across conversations while still serializing messages within a conversation (needed for session file safety).

11. **Why RunId instead of using conversation_id for status tracking?** — A conversation can have multiple sequential requests. If the user sends message 1, then `/abort`, then message 2, the portal listener needs to know which status message belongs to which request. RunId provides this disambiguation. It also enables future "show all active runs" diagnostics.

12. **What about the typo milestone files?** — Only `milestone-13-file-secret-store.md` exists on disk. The review mentioned `milestone-13-fiel-secret-store.md` as a potential duplicate, but it doesn't exist in the repository. This appears to be an IDE tabs issue, not a code issue.
