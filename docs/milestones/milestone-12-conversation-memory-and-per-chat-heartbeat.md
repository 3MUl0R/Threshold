# Milestone 12: Per-Conversation Memory & Heartbeat

## Problem Statement

Two critical gaps exposed during runtime testing:

1. **No persistent context across session resets.** When a Claude CLI session dies or is reset (BUG-001, daemon restart, timeout), all conversation context is lost. The agent starts fresh with no memory of what was being worked on, what decisions were made, or what tools/patterns the user prefers.

2. **Heartbeat is global, not per-conversation.** The current heartbeat system has a single global heartbeat task. Each conversation needs its own independent heartbeat — a recurring wake-up cycle where the agent checks in, reviews its instructions, does work if needed, and goes back to sleep.

### Terminology

- **Conversation** — The persistent logical unit. One conversation = one Discord channel thread = one memory file = one optional heartbeat. A conversation may have many CLI sessions over its lifetime (sessions are ephemeral; conversations are not).
- **CLI session** — An ephemeral Claude CLI subprocess. Sessions are created and destroyed as needed; session IDs are decoupled from conversation IDs (BUG-001 fix).
- **Heartbeat** — A per-conversation recurring wake-up. Optional, opt-in via `/heartbeat enable`.

## Design

### Phase 12A: Memory Substrate & Dynamic Prompt

The foundation: per-conversation directories, memory files, and dynamic prompt injection at session creation time.

#### Storage

```
~/.threshold/conversations/{conversation_id}/
  memory.md       # persistent agent memory for this conversation
  heartbeat.md    # heartbeat instructions (created when heartbeat is enabled)
```

The directory is created automatically when a conversation is first established.

#### Memory Lifecycle

1. **Initialization** — When a new conversation is created (via `/coding`, `/research`, `/general`, or a new Discord channel message), `memory.md` is seeded with default starter content based on the conversation type.

2. **Injection** — The contents of `memory.md` are injected into the system prompt when a **new CLI session** is created for that conversation (via `--append-system-prompt`). This happens only at session creation time, not on every message.

3. **Agent access** — The agent can read and update `memory.md` at any time via filesystem access. The system prompt tells the agent where its memory file lives.

4. **Persistence** — The file persists across CLI session resets, daemon restarts, and conversation resumptions. When a session is destroyed, the next session for that conversation re-injects the current memory contents.

5. **Growth** — The default content is a seed. Over time the agent appends decisions, progress, blockers, and context. The file grows organically.

#### Dynamic Prompt Injection

`build_tool_prompt()` is currently static/global — it cannot inject per-conversation paths. Rather than refactoring it to be dynamic, memory injection uses a separate mechanism:

- `send_message()` in `claude.rs` already accepts a `system_prompt` parameter
- When creating a new CLI session, the conversation engine builds a **conversation-specific system prompt supplement** containing:
  - The memory file path
  - The current contents of `memory.md` (so the agent has it immediately without needing to read the file)
  - Instructions for maintaining the memory file
- This supplement is appended to the existing static tool prompt via `--append-system-prompt`

```rust
// In conversation engine, when creating a new CLI session:
fn build_memory_prompt(conversation_id: &str, data_dir: &Path) -> String {
    let memory_path = data_dir.join("conversations").join(conversation_id).join("memory.md");
    let memory_contents = std::fs::read_to_string(&memory_path).unwrap_or_default();
    format!(
        "### Conversation Memory\n\
         Your persistent memory file is at: {path}\n\
         This file persists across session resets. Update it when you make important \
         decisions, complete milestones, or need to preserve information across sessions.\n\n\
         Current contents:\n{contents}",
        path = memory_path.display(),
        contents = memory_contents,
    )
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

- **Soft limit**: When memory exceeds 4KB, the injection includes only the first 4KB with a note: `"[Memory truncated — full file at {path}. Read it directly for complete context.]"`
- **No rotation**: The agent manages its own memory. If it grows too large, the agent can reorganize or prune it.
- The agent always has filesystem access to read the full file regardless of truncation.

### Phase 12B: Per-Conversation Heartbeat & Scheduler

Refactor the heartbeat system from global to per-conversation, with scheduler fixes.

#### Key Changes from Global Heartbeat

| Aspect | Current (Global) | New (Per-Conversation) |
|--------|------------------|------------------------|
| Scope | One heartbeat for the entire daemon | One heartbeat per conversation (optional) |
| Identity | Global task, no conversation link | `conversation_id` is the single key everywhere |
| Config | `[heartbeat]` in config.toml | `/heartbeat enable` in Discord + per-conversation config |
| Instructions | Global `~/.threshold/heartbeat.md` | Per-conversation `~/.threshold/conversations/{id}/heartbeat.md` |
| Handoff notes | Separate `handoff.md` | Eliminated — agent uses `heartbeat.md` directly (self-modifying) |
| Default state | Enabled globally | Disabled per conversation (opt-in) |
| Control | Config file only | `/heartbeat enable\|disable\|status` per channel |

#### Heartbeat File

Each conversation's `heartbeat.md` is self-modifying — the agent reads it on wake-up and updates it with status:

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

1. The scheduler creates a `ResumeConversation` action targeting the conversation's ID
2. The prompt is minimal: `"[Heartbeat] Check your heartbeat file at {heartbeat_path} and your memory file at {memory_path}."`
3. The agent reads both files, decides what to do, and acts
4. If the agent does work, it updates the heartbeat file with status
5. `skip_if_running = true` prevents overlapping heartbeats (see Concurrency below)

#### Concurrency: skip_if_running

The current `skip_if_running` check has a potential race window between checking and executing. Fix:

- Use the existing `ExecutionQueue` (Mutex) as the concurrency gate — if a conversation is already being processed by the queue, the heartbeat task detects this and skips
- The scheduler checks `execution_queue.is_conversation_active(conversation_id)` before enqueuing
- No separate lock needed; the execution queue already serializes all CLI invocations per conversation

#### Discord Slash Commands

The existing `/heartbeat` command becomes per-channel:

- `/heartbeat enable [interval_minutes]` — Enable heartbeat for this channel's conversation (default: 30 min). Creates `heartbeat.md` with defaults if it doesn't exist. Adds a `ScheduledTask` with `kind: Heartbeat` and `conversation_id`.
- `/heartbeat disable` — Disable heartbeat for this channel's conversation. Removes the scheduled task.
- `/heartbeat status` — Show heartbeat status for this channel's conversation (enabled/disabled, interval, last run, next run).
- `/heartbeat pause` / `/heartbeat resume` — Temporarily pause/resume without deleting config.

#### Scheduler Changes

- Remove the single-heartbeat assumption. The scheduler can have multiple `TaskKind::Heartbeat` tasks, each tied to a different `conversation_id`.
- Remove the startup code that creates a global heartbeat from `[heartbeat]` config.
- The `[heartbeat]` config section becomes optional defaults only (`default_interval_minutes`, `default_heartbeat_template`).
- Each heartbeat task stores only its `conversation_id` — no dual ID. The conversation ID is the single key used to find the memory dir, heartbeat file, and CLI session mapping.

#### Config Changes

```toml
[heartbeat]
# Default interval for new per-conversation heartbeats (minutes)
default_interval_minutes = 30
# Template file for new heartbeat.md files (optional)
# default_template = "~/.threshold/templates/heartbeat.md"
```

### Phase 12C: Migration, Cleanup & Integration

Handle the transition from global heartbeat to per-conversation, clean up old artifacts, and ensure everything works together.

#### Migration Strategy

1. **Global heartbeat.md** — If `~/.threshold/heartbeat.md` exists at startup and there are existing conversations, log a warning: `"Global heartbeat.md found. Per-conversation heartbeats are now configured via /heartbeat enable. See docs for migration."` Do not auto-migrate.
2. **[heartbeat] config section** — If `enabled = true` is set in the old-style config, log a warning at startup. The config section now only provides defaults, not a global heartbeat.
3. **No auto-conversion** — Users opt into per-conversation heartbeats explicitly. The global heartbeat simply stops firing after upgrade.

#### Conversation Deletion Lifecycle

When a conversation is deleted:
1. Remove the conversation directory (`~/.threshold/conversations/{id}/`)
2. Remove any associated scheduled heartbeat task
3. Remove the CLI session mapping
4. Remove the portal mapping

#### Integration Checklist

- `save_state()` preserves conversation directories (they're on-disk, not in the serialized JSON)
- Memory injection wired into `send_message()` new-session path
- Heartbeat prompt includes both memory and heartbeat file paths
- `config.example.toml` updated with new heartbeat defaults section
- End-to-end test with running daemon: create conversation, enable heartbeat, verify wake-up cycle

## Implementation Phases (Summary)

### Phase 12A — Memory Substrate & Dynamic Prompt
1. Create `conversations/{id}/` directory structure on conversation creation
2. Seed `memory.md` with type-appropriate defaults
3. Build `build_memory_prompt()` helper for dynamic injection
4. Inject memory into system prompt when creating new CLI sessions
5. Add 4KB soft limit with truncation notice
6. Tests: directory creation, seeding by type, memory prompt building, truncation

### Phase 12B — Per-Conversation Heartbeat & Scheduler
1. Refactor `/heartbeat` commands to operate per-channel (resolve conversation from portal)
2. Create `heartbeat.md` per conversation on enable
3. Modify scheduler to support multiple heartbeat tasks keyed by `conversation_id`
4. Remove global heartbeat startup logic
5. Add `is_conversation_active()` to execution queue for skip_if_running
6. Update heartbeat prompt to reference conversation-specific files
7. Tests: enable/disable per channel, multiple heartbeats, skip_if_running, concurrent heartbeat prevention

### Phase 12C — Migration, Cleanup & Integration
1. Add startup migration warnings for global heartbeat.md and old config
2. Implement conversation deletion lifecycle (clean up directory, tasks, mappings)
3. Update `config.example.toml` with new heartbeat defaults
4. End-to-end testing with running daemon

## Resolved Questions

1. **Memory file size** — 4KB soft limit on injection; agent can read full file via filesystem. No rotation needed.
2. **Memory injection timing** — Only on new CLI session creation. Agent has filesystem access for in-session reads.
3. **Heartbeat interval per conversation** — Yes, each conversation can have its own interval (set via `/heartbeat enable [minutes]`), falling back to `default_interval_minutes` from config.
4. **Migration** — Log warnings, no auto-migration. Global heartbeat stops firing; users enable per-conversation explicitly.
5. **Per-chat vs per-conversation** — All references use "per-conversation." One conversation = one Discord thread = one memory = one heartbeat.
6. **Dynamic prompt injection** — Handled at session creation via `--append-system-prompt`, not by modifying `build_tool_prompt()`.
7. **Handoff file** — Eliminated. The heartbeat file is self-modifying; the agent updates it directly.

## Files Affected

| File | Action | Phase |
|------|--------|-------|
| `crates/conversation/src/engine.rs` | Add memory dir creation, seeding, `build_memory_prompt()` | 12A |
| `crates/conversation/src/store.rs` | Add conversation dir path helpers | 12A |
| `crates/cli-wrapper/src/claude.rs` | Inject memory prompt on new session creation | 12A |
| `crates/discord/src/scheduler_commands.rs` | Refactor heartbeat commands per-channel | 12B |
| `crates/scheduler/src/heartbeat.rs` | Per-conversation heartbeat creation, remove global | 12B |
| `crates/scheduler/src/engine.rs` | Support multiple heartbeat tasks, add `is_conversation_active()` | 12B |
| `crates/server/src/main.rs` | Remove global heartbeat startup | 12B |
| `crates/core/src/config.rs` | Update HeartbeatConfig to defaults-only | 12B |
| `crates/conversation/src/engine.rs` | Conversation deletion lifecycle | 12C |
| `config.example.toml` | Update heartbeat section | 12C |
