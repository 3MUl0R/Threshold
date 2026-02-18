# Milestone 12: Per-Conversation Memory & Heartbeat

## Problem Statement

Two critical gaps exposed during runtime testing:

1. **No persistent context across session resets.** When a Claude CLI session dies or is reset (BUG-001, daemon restart, timeout), all conversation context is lost. The agent starts fresh with no memory of what was being worked on, what decisions were made, or what tools/patterns the user prefers.

2. **Heartbeat is global, not per-conversation.** The current heartbeat system has a single global heartbeat task. Each conversation needs its own independent heartbeat — a recurring wake-up cycle where the agent checks in, reviews its instructions, does work if needed, and goes back to sleep.

### Terminology

- **Conversation** — The persistent logical unit. A conversation has one mode (General, Coding, Research), one memory file, and one optional heartbeat. Multiple portals can be attached to the same conversation simultaneously.
- **Portal** — A communication endpoint (Discord channel, future: Telegram channel, hardware device, etc.). A portal is attached to exactly one conversation at a time but can be moved between conversations via `/coding`, `/research`, `/general`, or `/join`. Multiple portals can share the same conversation — messages from any portal are routed to the same agent context.
- **CLI session** — An ephemeral Claude CLI subprocess. Sessions are created and destroyed as needed; session IDs are decoupled from conversation IDs (BUG-001 fix). A conversation has at most one active CLI session.
- **Heartbeat** — A per-conversation recurring wake-up. Optional, opt-in via `/heartbeat enable` from any portal attached to that conversation.

### Portal-Conversation Relationship (Current Codebase)

The existing architecture intentionally supports many-to-one portal-to-conversation mapping:

- **`register_portal()`** (`engine.rs:307`) — New portals auto-attach to the singleton General conversation via `get_or_create_general()` (`store.rs:142`).
- **`switch_mode()`** (`engine.rs:404`) — Reuses existing conversations by mode key via `find_by_mode()` (`store.rs:128`). If a Coding("threshold") conversation already exists, switching any portal to `/coding threshold` attaches it to that same conversation.
- **`join_conversation()`** (`engine.rs:490`) — Explicitly moves a portal to a specific conversation by ID.
- **`get_portals_for_conversation()`** (`portals.rs:124`) — Returns all portals sharing a conversation, used for broadcasting responses to all attached endpoints.

**Implication for memory/heartbeat**: Memory and heartbeat are per-conversation, not per-portal. If two Discord channels (or a Discord channel and a future Telegram bot) share the same conversation, they share the same memory file and heartbeat. This is intentional — it provides continuity across communication endpoints.

## Design

### Phase 12A: Memory Substrate & Dynamic Prompt

The foundation: per-conversation directories, memory files, and dynamic prompt injection at session creation time.

#### Storage

```
~/.threshold/conversations/{conversation_id}/
  memory.md       # persistent agent memory for this conversation
  heartbeat.md    # heartbeat instructions (created when heartbeat is enabled)
```

The directory is created automatically when a conversation is first established (in `ConversationStore::create()` or `get_or_create_general()`).

#### Memory Lifecycle

1. **Initialization** — When a new conversation is created (via `/coding`, `/research`, `/general`, or auto-created General on first portal), `memory.md` is seeded with default starter content based on the conversation mode.

2. **Injection** — The contents of `memory.md` are injected into the system prompt when a **new CLI session** is created for that conversation (via `--append-system-prompt`). This happens only at session creation time, not on every message.

3. **Agent access** — The agent can read and update `memory.md` at any time via filesystem access. The system prompt tells the agent where its memory file lives.

4. **Persistence** — The file persists across CLI session resets, daemon restarts, and conversation resumptions. When a session is destroyed, the next session for that conversation re-injects the current memory contents.

5. **Growth** — The default content is a seed. Over time the agent appends decisions, progress, blockers, and context. The file grows organically.

#### Dynamic Prompt Injection

`build_tool_prompt()` (`tools/src/prompt.rs:13`) is static/global — built once at startup (`server/src/main.rs:106`) and shared across all conversations. Rather than refactoring it to be dynamic, memory injection uses a separate mechanism.

The conversation engine (`engine.rs`) already composes the system prompt before passing it to `ClaudeClient::send_message()`. This is the correct layer for per-conversation prompt assembly:

- `ConversationEngine::handle_message()` (`engine.rs:172`) builds the system prompt from the static tool prompt
- When creating a new CLI session, it appends a **conversation-specific memory supplement** containing:
  - The memory file path
  - The current contents of `memory.md` (so the agent has it immediately without needing to read the file)
  - Instructions for maintaining the memory file
- `send_message()` in `claude.rs` passes this combined prompt via `--append-system-prompt` only for new sessions (`claude.rs:210`)

```rust
// In conversation engine (engine.rs), when building system prompt for new session:
fn build_memory_prompt(conversation_id: &ConversationId, data_dir: &Path) -> String {
    let memory_path = data_dir
        .join("conversations")
        .join(conversation_id.0.to_string())
        .join("memory.md");
    let memory_contents = std::fs::read_to_string(&memory_path).unwrap_or_default();

    // Truncate if over 4KB (UTF-8 safe — find nearest char boundary)
    let (contents, truncated) = if memory_contents.len() > 4096 {
        let boundary = memory_contents.floor_char_boundary(4096);
        (&memory_contents[..boundary], true)
    } else {
        (memory_contents.as_str(), false)
    };

    let mut prompt = format!(
        "### Conversation Memory\n\
         Your persistent memory file is at: {path}\n\
         This file persists across session resets. Update it when you make important \
         decisions, complete milestones, or need to preserve information across sessions.\n\n\
         Current contents:\n{contents}",
        path = memory_path.display(),
        contents = contents,
    );

    if truncated {
        prompt.push_str(&format!(
            "\n\n[Memory truncated — full file at {}. Read it directly for complete context.]",
            memory_path.display()
        ));
    }

    prompt
}
```

#### Default Starter Content by Conversation Type

**Coding** (`/coding <project>`):
```markdown
# Conversation Memory

## Project
{project_name}

## Tools & Workflows
- **Codex CLI** — Use for all review cycles (planning, code, architecture). Run until all findings resolved.
  ```bash
  codex exec --full-auto "your prompt"
  codex exec resume <session-id> --full-auto "follow-up prompt"
  ```
- **Playwright CLI** — Use for browser access, end-to-end testing, and tasks beyond normal web tools.
  ```bash
  playwright-cli --help
  ```

## Notes
(Agent: update this section as you work. Record decisions, progress, blockers, and anything important to remember across sessions.)
```

**Research** (`/research <topic>`):
```markdown
# Conversation Memory

## Topic
{topic_name}

## Notes
(Agent: update this section as you work. Record findings, sources, key conclusions, and anything important to remember across sessions.)
```

**General** (default):
```markdown
# Conversation Memory

## Notes
(Agent: update this section as you work. Record anything important to remember across sessions.)
```

#### Memory Size Management

Memory files grow over time. To prevent unbounded system prompt growth:

- **Soft limit**: When memory exceeds 4KB, the injection includes only the first 4KB with a truncation notice pointing to the full file path.
- **No rotation**: The agent manages its own memory. If it grows too large, the agent can reorganize or prune it.
- The agent always has filesystem access to read the full file regardless of truncation.

### Phase 12B: Per-Conversation Heartbeat & Scheduler

Refactor the heartbeat system from global to per-conversation, with scheduler and concurrency fixes.

#### Key Changes from Global Heartbeat

| Aspect | Current (Global) | New (Per-Conversation) |
|--------|------------------|------------------------|
| Scope | One heartbeat for entire daemon (`main.rs:225-251`) | One heartbeat per conversation (optional) |
| Identity | Random `conversation_id` generated if missing (`heartbeat.rs:106`) | Uses the existing conversation's real ID |
| Prompt | Empty placeholder string (`heartbeat.rs:112`), never dynamically built at runtime | Dynamically built at fire time from `heartbeat.md` + `memory.md` paths |
| Config | `[heartbeat]` in config.toml creates global task at startup | `/heartbeat enable` per conversation via Discord |
| Instructions | Global `~/.threshold/heartbeat.md` | Per-conversation `~/.threshold/conversations/{id}/heartbeat.md` |
| Handoff notes | Separate `handoff.md` + `extract_handoff_notes()` + `save_handoff_notes()` | Eliminated — agent uses `heartbeat.md` directly (self-modifying) |
| Default state | Enabled globally | Disabled per conversation (opt-in) |
| Control | Config file + global `/heartbeat status\|pause\|resume` | `/heartbeat enable\|disable\|status\|pause\|resume` per conversation |
| Skip check | `running_tasks: HashSet<Uuid>` checks task ID only (`engine.rs:279`) | Extended to also track `conversation_id` for per-conversation skip |

#### Heartbeat File

Each conversation's `heartbeat.md` is self-modifying — the agent reads it on wake-up and updates it with status. This eliminates the separate handoff notes mechanism (`extract_handoff_notes()`, `save_handoff_notes()`, `load_handoff_notes()` in `heartbeat.rs`).

Default `heartbeat.md` (created when heartbeat is enabled):
```markdown
# Heartbeat Instructions

This is a heartbeat wake-up. Review your memory file and this heartbeat file for context.

If you find specific instructions or pending tasks below, follow them.
Do not infer or repeat tasks that are already complete.

If nothing needs attention, reply: "Heartbeat OK — nothing requires attention."

## Pending Tasks
(none)

## Status Log
(Agent: update this section with timestamps when you complete work during heartbeats)
```

#### Heartbeat Firing

When a heartbeat fires for a conversation:

1. The scheduler's `execute_task()` detects `TaskKind::Heartbeat` and dynamically builds the prompt (replacing the current empty-prompt-passthrough in `execution.rs:48`)
2. The prompt reads the conversation's `heartbeat.md` and includes paths: `"[Heartbeat] Check your heartbeat file at {heartbeat_path} and your memory file at {memory_path}."`
3. The action is `ResumeConversation { conversation_id, prompt }` targeting the real conversation
4. The agent reads both files, decides what to do, and acts
5. If the agent does work, it updates the heartbeat file with status

#### Concurrency: skip_if_running

The current `skip_if_running` check (`engine.rs:277-290`) uses `running_tasks: Arc<RwLock<HashSet<Uuid>>>` to track which **task UUIDs** are executing. This works for preventing the same task from running concurrently, but doesn't prevent a heartbeat from firing while a user message is being processed for the same conversation.

**Fix — shared `active_conversations` tracker injected into both scheduler and conversation engine:**

The scheduler state is internal (`engine.rs:92`) and `ConversationEngine` has no scheduler dependency (`engine.rs:64`). To bridge this, introduce a shared tracker that both systems can update:

```rust
// New: crates/core/src/active_tracker.rs (or inline in types.rs)
// A shared set of conversation IDs with active CLI invocations.
// Injected via Arc into both ConversationEngine and Scheduler at startup.
pub struct ActiveConversations {
    inner: RwLock<HashSet<ConversationId>>,
}

impl ActiveConversations {
    pub fn new() -> Self { ... }
    pub async fn insert(&self, id: ConversationId) { ... }
    pub async fn remove(&self, id: &ConversationId) { ... }
    pub async fn contains(&self, id: &ConversationId) -> bool { ... }
}
```

**Wiring** (in `server/src/main.rs` during startup):
```rust
let active_conversations = Arc::new(ActiveConversations::new());
// Pass to conversation engine (marks active during handle_message)
let engine = ConversationEngine::new(..., active_conversations.clone());
// Pass to scheduler (checks before firing heartbeat)
let scheduler = Scheduler::new(..., active_conversations.clone());
```

- `ConversationEngine::handle_message()` inserts `conversation_id` before calling `send_message()`, removes it on completion
- Scheduler's heartbeat skip check (`engine.rs:279`) checks **both** `running_tasks` (same task ID already running) **and** `active_conversations.contains(conversation_id)` (any activity on this conversation)
- When a scheduled `ResumeConversation` task starts, it also inserts into `active_conversations` to prevent user messages from colliding

This replaces the previous plan to use `ExecutionQueue`, which is just a global `Mutex<()>` in `cli-wrapper` (`queue.rs:10`) with no conversation awareness and no public API for checking state.

#### Phase Shifting

When multiple conversations have heartbeats enabled, they could fire simultaneously and overload the system (since the global `ExecutionQueue` serializes all CLI invocations). Simple mitigation:

- When creating a new heartbeat task, offset the cron schedule by a random jitter (0 to `interval_minutes - 1` minutes) to spread heartbeats across time
- The existing semaphore in scheduler (`engine.rs:309`) already limits concurrent task spawns

#### Discord Slash Commands

The current `/heartbeat` command (`scheduler_commands.rs:124-178`) finds the first heartbeat task globally via `tasks.iter().find(|t| t.kind == TaskKind::Heartbeat)` and only supports `status|pause|resume`. This needs a complete rewrite:

**New `/heartbeat` command** — resolves the current channel's conversation via portal, then operates on that conversation's heartbeat:

- `/heartbeat enable [interval_minutes]` — Enable heartbeat for this channel's conversation (default from config, fallback 30 min). Creates `heartbeat.md` with defaults if it doesn't exist. Creates a `ScheduledTask` with `kind: Heartbeat` and the conversation's real `conversation_id` in its `ResumeConversation` action.
- `/heartbeat disable` — Disable and remove heartbeat for this channel's conversation. Removes the scheduled task (not just toggles it).
- `/heartbeat status` — Show heartbeat status for this channel's conversation (enabled/disabled, interval, last run, next run).
- `/heartbeat pause` / `/heartbeat resume` — Temporarily pause/resume via `scheduler.toggle_task()` without deleting.

**Key implementation detail**: The command must resolve the current channel's conversation, then find the heartbeat task by matching `conversation_id` — not by finding the first `TaskKind::Heartbeat`.

**New helpers needed** (currently missing from the codebase):
- `resolve_or_create_portal()` is already public in `portals.rs:8` and usable from `scheduler_commands.rs`
- Add `ConversationEngine::get_portal_conversation(portal_id) -> Result<ConversationId>` — a thin wrapper over the existing `portals.read().get_conversation()` pattern used in `handle_message()` (`engine.rs:174-179`)

```rust
// Resolve conversation for the channel where /heartbeat was invoked
let portal_id = resolve_or_create_portal(&engine, guild_id, channel_id).await;
let conversation_id = engine.get_portal_conversation(&portal_id).await?;

// Find heartbeat for THIS conversation (not the first global one)
let heartbeat_task = tasks.iter().find(|t| {
    t.kind == TaskKind::Heartbeat
        && matches!(&t.action, ScheduledAction::ResumeConversation { conversation_id: cid, .. }
            if *cid == conversation_id)
});
```

#### Scheduler Changes

- **Remove single-heartbeat assumption**: The scheduler can have multiple `TaskKind::Heartbeat` tasks, each tied to a different `conversation_id`. Remove the `any(kind == Heartbeat)` guard in `main.rs:232`.
- **Remove global heartbeat startup**: Remove the `heartbeat_task_from_config()` call in `main.rs:225-251`. Heartbeats are created per-conversation via Discord commands only.
- **Add `active_conversations` tracking**: New `HashSet<ConversationId>` alongside existing `running_tasks` for conversation-level skip checks.
- **Dynamic prompt building at fire time**: When executing a heartbeat task, read the conversation's `heartbeat.md` and build the prompt dynamically instead of using the stored empty string.
- **Remove dead handoff code**: Remove `extract_handoff_notes()`, `save_handoff_notes()`, `load_handoff_notes()`, and `build_heartbeat_prompt()` from `heartbeat.rs`. Remove `handoff_notes_path` from `ScheduledTask`. The self-modifying `heartbeat.md` replaces all of this.

#### Config Changes

```toml
[heartbeat]
# Default interval for new per-conversation heartbeats (minutes)
default_interval_minutes = 30
# Template file for new heartbeat.md files (optional)
# default_template = "~/.threshold/templates/heartbeat.md"
```

The `[heartbeat]` section no longer creates a global heartbeat. Fields like `enabled`, `instruction_file`, `conversation_id`, `handoff_notes_path`, and `notification_channel_id` are removed. Only `default_interval_minutes` and `default_template` remain as defaults for new per-conversation heartbeats.

### Phase 12C: Migration, Cleanup & Conversation Deletion

Handle the transition from global heartbeat to per-conversation, add conversation deletion, and clean up dead code.

#### Migration Strategy

1. **Global heartbeat.md** — If `~/.threshold/heartbeat.md` exists at startup, log a warning: `"Global heartbeat.md found. Per-conversation heartbeats are now configured via /heartbeat enable. See docs for migration."` Do not auto-migrate.
2. **[heartbeat] config section** — If old-style fields (`enabled`, `instruction_file`, `conversation_id`) are present, log a warning at startup. The config section now only provides defaults.
3. **Persisted heartbeat tasks** — Detect legacy global heartbeat tasks by validating the `conversation_id` in their `ResumeConversation` action against the conversation store. If the conversation_id doesn't exist in the store (because it was a randomly generated ID from `heartbeat_task_from_config()`), log a warning and remove the task from the schedule.
4. **No auto-conversion** — Users opt into per-conversation heartbeats explicitly via `/heartbeat enable`.

#### Conversation Deletion

Currently the conversation engine has no delete API — only `register_portal` (`engine.rs:307`), `switch_mode` (`engine.rs:404`), `join_conversation` (`engine.rs:490`), and `list_conversations` (`engine.rs:376`). Add `delete_conversation()`:

**Constraints**:
- **General conversation cannot be deleted** — it's the singleton fallback. `delete_conversation()` must reject attempts to delete General.
- **Portal re-attachment must use direct `portals.attach()`** — not `switch_mode()`, because `switch_mode` no-ops when the target mode matches the current conversation's mode (`engine.rs:451`). Directly attaching portals to the General conversation avoids this.

**New engine APIs needed** (not in current surface):
- `ConversationEngine` needs access to `data_dir: PathBuf` (currently passed at construction but not stored as a field — add it)
- `ConversationEngine` needs access to `ClaudeClient` or `SessionManager` for session cleanup — currently `ClaudeClient` is a sibling, not owned by engine. **Option**: engine emits `ConversationDeleted` event and the server wiring layer handles session cleanup in response, keeping the crate boundary clean.

```rust
// In ConversationEngine (pseudocode — field names will be finalized at implementation):
pub async fn delete_conversation(&self, conversation_id: ConversationId) -> Result<()> {
    // 0. Reject deletion of General conversation
    {
        let conversations = self.conversations.read().await;
        let conv = conversations.get(&conversation_id)
            .ok_or(ThresholdError::ConversationNotFound { id: conversation_id.0 })?;
        if conv.mode == ConversationMode::General {
            return Err(ThresholdError::InvalidInput {
                message: "Cannot delete the General conversation".into(),
            });
        }
    }

    // 1. Re-attach all portals to General (using direct attach, not switch_mode)
    let general_id = {
        let conversations = self.conversations.read().await;
        conversations.find_by_mode(&ConversationMode::General)
            .map(|c| c.id)
            .expect("General conversation must exist")
    };
    {
        let portals = self.portals.read().await
            .get_portals_for_conversation(&conversation_id);
        let mut portal_registry = self.portals.write().await;
        for portal in portals {
            portal_registry.attach(&portal.id, general_id)?;
        }
    }

    // 2. Remove conversation from store (new method — ConversationStore needs a `remove()` API)
    self.conversations.write().await.remove(conversation_id)?;

    // 3. Remove conversation directory (memory.md, heartbeat.md)
    let conv_dir = self.data_dir.join("conversations").join(conversation_id.0.to_string());
    if conv_dir.exists() {
        tokio::fs::remove_dir_all(&conv_dir).await?;
    }

    // 4. Broadcast deletion event — listeners handle:
    //    - Scheduler: remove heartbeat task for this conversation_id
    //    - Server layer: remove CLI session mapping via ClaudeClient
    let _ = self.event_tx.send(ConversationEvent::ConversationDeleted { conversation_id });

    Ok(())
}
```

**Session cleanup**: `SessionManager::remove` is already public (`session.rs:108`) and takes `Uuid` by value. The server wiring layer listens for `ConversationDeleted` events and calls `session_manager.remove(conversation_id.0)` directly (the `SessionManager` is `Arc`-shared). This keeps `cli-wrapper` out of the conversation engine's dependency tree. Note: `ClaudeClient` currently has no `session_manager()` accessor — either add one or share the `Arc<SessionManager>` independently at startup.

#### Dead Code Removal

Remove from `crates/scheduler/src/heartbeat.rs`:
- `build_heartbeat_prompt()` (line 19) — replaced by dynamic prompt at fire time
- `extract_handoff_notes()` (line 46) — handoff eliminated
- `load_handoff_notes()` (line 61) — handoff eliminated
- `save_handoff_notes()` (line 66) — handoff eliminated
- `heartbeat_task_from_config()` (line 87) — global heartbeat creation eliminated
- Associated tests (line 143+) for the above functions

Remove from `crates/scheduler/src/task.rs`:
- `handoff_notes_path: Option<PathBuf>` field (line 44)
- Associated test assertions for `handoff_notes_path` (lines 259, 263-266, 290)

Remove from `crates/core/src/config.rs`:
- Old `HeartbeatConfig` fields: `enabled`, `instruction_file`, `conversation_id`, `skip_if_running`, `handoff_notes_path`, `notification_channel_id`
- Keep only: `default_interval_minutes`, `default_template`

Remove from `config.example.toml`:
- Old heartbeat config lines: `instruction_file` (line 117), `handoff_notes_path` (line 118), `notification_channel_id` (line 120)

#### Integration Checklist

- `save_state()` preserves conversation directories (they're on-disk, not in the serialized JSON)
- Memory injection wired into conversation engine's prompt composition path
- Heartbeat prompt dynamically built at fire time with conversation-specific paths
- `config.example.toml` updated with new heartbeat defaults section
- `/conversation delete` Discord command added (or `/delete` subcommand)
- End-to-end test with running daemon: create conversation, enable heartbeat, verify wake-up cycle

## Implementation Phases (Summary)

### Phase 12A — Memory Substrate & Dynamic Prompt
1. Create `conversations/{id}/` directory in `ConversationStore::create()` and `get_or_create_general()`
2. Seed `memory.md` with mode-appropriate defaults
3. Build `build_memory_prompt()` in conversation engine (not cli-wrapper)
4. Append memory prompt to system prompt when creating new CLI sessions
5. Add 4KB soft limit with truncation notice
6. Tests: directory creation, seeding by mode type, memory prompt building, truncation

### Phase 12B — Per-Conversation Heartbeat & Scheduler
1. Create `ActiveConversations` shared tracker in `crates/core/` — inject into both engine and scheduler at startup
2. Add `get_portal_conversation()` helper to `ConversationEngine`
3. Make `resolve_or_create_portal()` accessible from `scheduler_commands.rs` (already public in `portals.rs`)
4. Rewrite `/heartbeat` command to resolve conversation from portal, match by conversation_id
5. Add `/heartbeat enable [interval]` and `/heartbeat disable` subcommands
6. Create `heartbeat.md` per conversation on enable with default template
7. Wire `ActiveConversations` into `handle_message()` (insert/remove around CLI call)
8. Wire `ActiveConversations` into scheduler's heartbeat skip check
9. Add dynamic heartbeat prompt building in `execute_task()` for `TaskKind::Heartbeat`
10. Remove global heartbeat startup logic from `main.rs:225-251`
11. Remove `any(kind == Heartbeat)` single-heartbeat guard from `main.rs:232`
12. Add phase-shift jitter when creating new heartbeat tasks
13. Tests: enable/disable per conversation, multiple heartbeats, skip_if_running with conversation tracking, phase shifting

### Phase 12C — Migration, Cleanup & Conversation Deletion
1. Add startup migration: validate persisted heartbeat tasks against conversation store, remove orphaned ones
2. Add startup migration warnings for global `heartbeat.md` and old-style config fields
3. Implement `delete_conversation()` in conversation engine (with General protection)
4. Add `ConversationDeleted` event variant and listeners (scheduler removes heartbeat task, server removes session mapping)
5. Store `data_dir` as field in `ConversationEngine` for directory cleanup
6. Remove dead handoff code from `heartbeat.rs` (5 functions + tests)
7. Remove `handoff_notes_path` from `ScheduledTask` + related tests
8. Slim down `HeartbeatConfig` to defaults-only
9. Update `config.example.toml` — remove old heartbeat fields
10. End-to-end testing with running daemon

## Resolved Questions

1. **Portal-conversation relationship** — Many portals can share one conversation. Memory and heartbeat are per-conversation, shared across all attached portals. This supports future multi-platform portals (Telegram, hardware devices).
2. **Memory file size** — 4KB soft limit on injection; agent can read full file via filesystem. No rotation needed.
3. **Memory injection timing** — Only on new CLI session creation, built in conversation engine (not cli-wrapper). Agent has filesystem access for in-session reads.
4. **Heartbeat interval per conversation** — Yes, each conversation can have its own interval (set via `/heartbeat enable [minutes]`), falling back to `default_interval_minutes` from config.
5. **Migration** — Log warnings for global heartbeat.md, old config fields, and persisted global tasks. No auto-migration.
6. **Dynamic prompt injection** — Memory prompt composed in conversation engine and appended via `--append-system-prompt`. `build_tool_prompt()` stays static.
7. **Handoff file** — Eliminated entirely. Self-modifying `heartbeat.md` replaces `build_heartbeat_prompt()`, `extract_handoff_notes()`, `load/save_handoff_notes()`.
8. **Concurrency** — Shared `ActiveConversations` tracker (in `crates/core`) injected into both conversation engine and scheduler at startup. Replaces the unimplementable `ExecutionQueue.is_conversation_active()` plan.
9. **Multiple heartbeats** — Phase-shifted with random jitter to avoid simultaneous firing. Serialized by existing `ExecutionQueue` mutex if they do overlap.
10. **Conversation deletion** — New `delete_conversation()` API: reject General deletion, re-attach portals via direct `attach()` (not `switch_mode` which no-ops), remove store entry, remove directory, broadcast `ConversationDeleted` for session + heartbeat cleanup.
11. **Session cleanup on delete** — Handled via event listener in server wiring layer, not by engine directly. Keeps `cli-wrapper` out of conversation engine's dependency tree. `SessionManager::remove` is already public (`session.rs:108`).
12. **Legacy heartbeat detection** — Persisted heartbeat tasks from before migration are detected by validating their `conversation_id` against the conversation store. Orphaned IDs (from `heartbeat_task_from_config`'s random generation) are removed.

## Files Affected

| File | Action | Phase |
|------|--------|-------|
| `crates/conversation/src/engine.rs` | Add `build_memory_prompt()`, memory dir creation, seed on conversation create | 12A |
| `crates/conversation/src/store.rs` | Create `conversations/{id}/` dir in `create()` and `get_or_create_general()` | 12A |
| `crates/core/src/types.rs` (or new file) | Add `ActiveConversations` shared tracker | 12B |
| `crates/conversation/src/engine.rs` | Add `get_portal_conversation()`, wire `ActiveConversations` into `handle_message()` | 12B |
| `crates/discord/src/scheduler_commands.rs` | Rewrite `/heartbeat` — resolve conversation, match by conversation_id, add enable/disable | 12B |
| `crates/scheduler/src/engine.rs` | Wire `ActiveConversations`, remove single-heartbeat guard | 12B |
| `crates/scheduler/src/execution.rs` | Dynamic heartbeat prompt building for `TaskKind::Heartbeat` | 12B |
| `crates/server/src/main.rs` | Create + inject `ActiveConversations`, remove global heartbeat startup (lines 225-251) | 12B |
| `crates/conversation/src/engine.rs` | Add `delete_conversation()`, `ConversationDeleted` event, store `data_dir` field | 12C |
| `crates/conversation/src/store.rs` | Add `remove()` method | 12C |
| `crates/server/src/main.rs` | Add `ConversationDeleted` listener for session cleanup | 12C |
| `crates/scheduler/src/heartbeat.rs` | Remove `build_heartbeat_prompt`, `extract/load/save_handoff_notes`, `heartbeat_task_from_config` + tests | 12C |
| `crates/scheduler/src/task.rs` | Remove `handoff_notes_path` field + related tests | 12C |
| `crates/core/src/config.rs` | Slim `HeartbeatConfig` to defaults-only | 12C |
| `config.example.toml` | Remove old heartbeat fields, update to defaults-only | 12C |
