# Milestone 15 — Multi-Platform Portal Support

**Crates:** `core`, `conversation`, `scheduler`, `discord`, `server`
**Complexity:** Medium
**Dependencies:** Milestone 3 (conversation engine), Milestone 4 (Discord bot), Milestone 12 (memory/heartbeat), Milestone 14 (streaming/broadcast)

## What This Milestone Delivers

Infrastructure for connecting multiple communication platforms to the same conversation. Today, only Discord portals exist. This milestone generalizes the portal system so that Teams, Slack, or any future platform can plug in as a first-class portal — sharing the same conversation context, memory, audit trail, and CLI session.

1. **Primary portal per conversation** — Each conversation designates one portal as primary. Scheduled tasks, heartbeats, and other non-user-initiated output goes to the primary portal by default.
2. **Portal source tagging** — Every message in the system carries metadata about which portal (and platform) it came from. The audit trail logs it. The agent's context includes a lightweight `[via Discord]` tag.
3. **Scheduler portal targeting** — Scheduled tasks can optionally target a specific portal for output, overriding the primary default.
4. **Platform-agnostic portal abstraction** — `PortalType` is extended with display names and source labels. New platforms only need to implement a portal listener — no changes to the conversation engine.

### What This Does NOT Do

- **No new platform crate** — This milestone builds the infrastructure. Actual Teams/Slack integration would be a separate future milestone that adds a `crates/teams/` or `crates/slack/` crate.
- **No cross-platform message relay** — Messages sent in Discord don't appear in Teams. The agent has full context, but platform UIs remain independent. When a user sends a message from a specific portal, the agent responds to that same portal (origin-only delivery). Other portals on the conversation do not echo the response.
- **No portal-level permissions** — All portals on a conversation have equal access. Role-based portal restrictions are out of scope.

---

## Architecture

### Current State

```
Discord Channel ──→ Portal (PortalType::Discord) ──→ Conversation
                                                       ├── memory.md
                                                       ├── audit trail
                                                       └── Claude CLI session
```

Portals are already platform-agnostic in concept — `PortalType` is an enum with `Discord` as the only variant. The conversation engine routes by `ConversationId`, not by platform. Broadcast events go to all portal listeners on a conversation.

### Current Gaps

1. **No primary portal designation.** All portals are equal. Scheduled task output broadcasts to every portal listener on a conversation — fine with one Discord portal, noisy with multiple platforms.

2. **No portal source metadata on messages.** The timestamp injection (`engine.rs:441`) adds `[2026-02-25 12:00 UTC]` but not where the message came from. The audit trail already records `portal_id` and `portal_type` on `UserMessage` events (`conversation/src/audit.rs:15-20`) but doesn't use a structured `MessageSource` type.

3. **`DeliveryTarget` is Discord-specific.** The scheduler's `DeliveryTarget::DiscordChannel` and `DiscordDm` variants (`scheduler/src/task.rs:61-67`) are hardcoded to Discord. Scheduled tasks can't target a generic portal.

4. **`PortalType` has no display/label method.** There's no way to get a human-readable source name like "Discord" or "Teams" from a `PortalType` variant.

5. **`ScheduledTask` has an unused `portal_id` field.** The existing `portal_id: Option<PortalId>` field (`task.rs:34`) is never set to `Some(...)` in any code path. This milestone repurposes it as the portal targeting mechanism (see Scheduler Portal Targeting below).

### New Flow

```
Discord Channel ──→ Portal A (primary) ──┐
                                          ├──→ Conversation
Teams Channel   ──→ Portal B           ──┘      ├── memory.md (shared)
                                                 ├── audit trail (shared, portal-tagged)
                                                 └── Claude CLI session (shared)

User message from Discord → agent responds to Discord only (origin portal)
User message from Teams   → agent responds to Teams only (origin portal)
Scheduled task fires      → output goes to primary (Portal A) unless overridden
Agent has full context from both platforms
```

---

## Delivery Semantics

Three distinct delivery modes, each with clear rules:

| Source | Delivery Rule | `DeliveryFilter` value |
|--------|--------------|----------------------|
| **User message** (via portal) | Respond to **origin portal only** | `Portal(portal_id)` |
| **Scheduled task** (no portal override) | Deliver to **primary portal** | `PrimaryOnly` |
| **Scheduled task** (with portal override) | Deliver to **specified portal** | `Portal(portal_id)` |

**Rationale:** User-initiated responses go back to the portal that sent the message — this avoids cross-platform relay (a Discord message triggering a response in both Discord and Teams). Scheduled tasks have no origin portal, so they go to the primary unless overridden.

---

## Primary Portal

### Design

Each conversation gets an optional `primary_portal: Option<PortalId>` field. Behavior:

- **Default assignment:** The first portal attached to a conversation becomes primary.
- **Startup backfill:** On engine startup, if a conversation has `primary_portal: None` but has attached portals, the oldest registered portal (by `connected_at`) is assigned as primary. This handles migration of existing conversations.
- **Explicit override:** A Discord command (`/primary`) or future platform equivalent claims primary for that portal.
- **Fallback:** If the primary portal is detached or deleted, the oldest remaining registered portal (by `connected_at`) becomes primary. If no portals remain, `primary_portal` becomes `None`.
- **Scheduled task routing:** When a scheduled task broadcasts output, portal listeners check: if the task has a `portal_id` override, only that portal delivers. Otherwise, only the primary portal delivers. This replaces the current "broadcast to all" behavior for scheduled/heartbeat output.

### Data Model Changes

```rust
// crates/core/src/types.rs — add field to Conversation
pub struct Conversation {
    pub id: ConversationId,
    pub mode: ConversationMode,
    pub cli_provider: CliProvider,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    /// The portal that receives scheduled/unprompted output by default.
    #[serde(default)]
    pub primary_portal: Option<PortalId>,
}
```

### Assignment Logic

**Lock ordering:** The engine holds `portals` then `conversations`, or each independently — never `conversations` then `portals`. This matches the existing lock discipline in the codebase.

```rust
// In ConversationEngine — when a portal is attached:
async fn maybe_set_primary(&self, conversation_id: ConversationId, portal_id: PortalId) {
    let changed = {
        let mut conversations = self.conversations.write().await;
        if let Some(conv) = conversations.get_mut(&conversation_id) {
            if conv.primary_portal.is_none() {
                conv.primary_portal = Some(portal_id);
                true
            } else { false }
        } else { false }
    }; // conversations lock dropped

    if changed {
        if let Err(e) = self.save_conversations().await {
            tracing::warn!("Failed to persist primary portal assignment: {}", e);
        }
    }
}

// In ConversationEngine — when a portal is detached:
async fn maybe_reassign_primary(&self, conversation_id: ConversationId, detached_portal: PortalId) {
    // Read portals FIRST (before acquiring conversations lock)
    let next_primary = {
        let portals = self.portals.read().await;
        portals
            .get_portals_for_conversation(&conversation_id)
            .iter()
            .filter(|p| p.id != detached_portal)
            .min_by_key(|p| p.connected_at)
            .map(|p| p.id)
    }; // portals lock dropped here

    let changed = {
        let mut conversations = self.conversations.write().await;
        if let Some(conv) = conversations.get_mut(&conversation_id) {
            if conv.primary_portal == Some(detached_portal) {
                conv.primary_portal = next_primary;
                true
            } else { false }
        } else { false }
    }; // conversations lock dropped

    if changed {
        if let Err(e) = self.save_conversations().await {
            tracing::warn!("Failed to persist primary portal reassignment: {}", e);
        }
    }
}

// Explicit override:
pub async fn set_primary_portal(
    &self,
    conversation_id: ConversationId,
    portal_id: PortalId,
) -> Result<()> {
    // Verify portal is attached to this conversation (portals lock only)
    {
        let portals = self.portals.read().await;
        let portal = portals.get(&portal_id)
            .ok_or(ThresholdError::PortalNotFound { id: portal_id.0 })?;
        if portal.conversation_id != conversation_id {
            return Err(ThresholdError::InvalidInput {
                message: "Portal is not attached to this conversation".into(),
            });
        }
    } // portals lock dropped

    let mut conversations = self.conversations.write().await;
    if let Some(conv) = conversations.get_mut(&conversation_id) {
        conv.primary_portal = Some(portal_id);
    }
    conversations.save().await?;
    Ok(())
}
```

### Startup Backfill

On engine initialization, after loading conversations and portals from disk:

```rust
// In ConversationEngine::new() or init():
async fn backfill_primary_portals(&self) {
    // Phase 1: Collect backfill assignments while holding locks briefly.
    let assignments: Vec<(ConversationId, PortalId)> = {
        let portals = self.portals.read().await;
        let conversations = self.conversations.read().await;
        conversations.all()
            .filter(|conv| conv.primary_portal.is_none())
            .filter_map(|conv| {
                portals
                    .get_portals_for_conversation(&conv.id)
                    .iter()
                    .min_by_key(|p| p.connected_at)
                    .map(|p| (conv.id, p.id))
            })
            .collect()
    }; // both locks dropped here

    if assignments.is_empty() {
        return;
    }

    // Phase 2: Apply assignments under conversations write lock (no portals lock needed).
    {
        let mut conversations = self.conversations.write().await;
        for (conv_id, portal_id) in &assignments {
            if let Some(conv) = conversations.get_mut(conv_id) {
                if conv.primary_portal.is_none() {
                    conv.primary_portal = Some(*portal_id);
                }
            }
        }
    } // conversations lock dropped

    // Phase 3: Persist outside any lock.
    if let Err(e) = self.save_conversations().await {
        tracing::warn!("Failed to persist backfilled primary portals: {}", e);
    }
}
```

---

## Portal Source Tagging

### Message Source Metadata

Add a `source` field to conversation audit entries that identifies where a message came from.

```rust
// crates/core/src/types.rs — new enum
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageSource {
    /// Message from a user via a portal.
    Portal {
        portal_id: PortalId,
        platform: String,    // "Discord", "Teams", "Slack", etc.
    },
    /// Message from the scheduler (scheduled task, heartbeat).
    Scheduler { task_name: String },
    /// Message from an internal system action.
    System,
}
```

### PortalType Display Labels

```rust
// crates/core/src/types.rs — add method to PortalType
impl PortalType {
    /// Human-readable platform name for display and tagging.
    pub fn platform_name(&self) -> &'static str {
        match self {
            PortalType::Discord { .. } => "Discord",
            // Future variants:
            // PortalType::Teams { .. } => "Teams",
            // PortalType::Slack { .. } => "Slack",
        }
    }
}
```

### Timestamp Injection Enhancement

The current timestamp injection (`engine.rs:441`) becomes:

```rust
// Current:
let timestamped_content = format!(
    "[{} {}]\n{}",
    now.format("%Y-%m-%d %H:%M"),
    now.format("%Z"),
    content,
);

// New (when message source is known):
let source_label = match &message_source {
    MessageSource::Portal { platform, .. } => format!(" via {}", platform),
    MessageSource::Scheduler { task_name } => format!(" via Scheduler:{}", task_name),
    MessageSource::System => " via System".to_string(),
};
let timestamped_content = format!(
    "[{} {}{}]\n{}",
    now.format("%Y-%m-%d %H:%M"),
    now.format("%Z"),
    source_label,
    content,
);
// Result: "[2026-02-25 12:00 UTC via Discord]\nHello!"
```

### Audit Trail Enrichment

The audit trail currently uses `ConversationAuditEvent` in `crates/conversation/src/audit.rs`. The `UserMessage` variant already carries `portal_id` and `portal_type: String`. We enhance this by adding an optional `source: Option<MessageSource>` field to variants that don't already have portal context:

```rust
// crates/conversation/src/audit.rs — add source to AssistantMessage and other variants
ConversationAuditEvent::AssistantMessage {
    content: String,
    usage: Option<Usage>,
    duration_ms: u64,
    timestamp: DateTime<Utc>,
    #[serde(default)]
    source: Option<MessageSource>,  // NEW — which portal/scheduler triggered this response
},
```

For `UserMessage`, the existing `portal_id` and `portal_type` fields already serve this purpose. New code should populate the `MessageSource` from these fields for consistency, but no schema change is needed for `UserMessage`.

---

## Scheduler Portal Targeting

### Problem

Currently, scheduled task output broadcasts to all portal listeners on a conversation. With multiple platforms, a daily email summary would post to Discord AND Teams AND Slack simultaneously. This is noisy and undesirable in most cases.

### Solution: Repurpose Existing `portal_id` Field

`ScheduledTask` already has `portal_id: Option<PortalId>` (`task.rs:34`) which is never set to `Some(...)` in any code path. Rather than adding a new `target_portal` field and creating ambiguous dual fields, we repurpose `portal_id` as the portal targeting mechanism:

```rust
// crates/scheduler/src/task.rs — existing field, now documented
pub struct ScheduledTask {
    // ... existing fields ...

    /// Portal to exclusively receive this task's output.
    /// If None, output goes to the conversation's primary portal.
    /// If Some, output goes only to this specific portal.
    #[serde(default)]
    pub portal_id: Option<PortalId>,  // EXISTING field — now actively used
}
```

### Delivery Routing

The broadcast event gains a `delivery_target` field so portal listeners can self-filter:

```rust
// crates/conversation/src/engine.rs — new enum + extend ConversationEvent
#[derive(Debug, Clone)]
pub enum DeliveryFilter {
    /// Only deliver to this specific portal (user-initiated or explicit target).
    Portal(PortalId),
    /// Only deliver to the primary portal of the conversation.
    PrimaryOnly,
}

pub enum ConversationEvent {
    AssistantMessage {
        conversation_id: ConversationId,
        run_id: RunId,
        content: String,
        artifacts: Vec<Artifact>,
        usage: Option<Usage>,
        timestamp: DateTime<Utc>,
        /// Who should deliver this message. Required — every response has a target.
        delivery_target: DeliveryFilter,
    },
    Error {
        conversation_id: ConversationId,
        run_id: Option<RunId>,
        error: String,
        delivery_target: DeliveryFilter,
    },
    Acknowledgment {
        conversation_id: ConversationId,
        run_id: RunId,
        content: String,
        delivery_target: DeliveryFilter,
    },
    StatusUpdate {
        conversation_id: ConversationId,
        run_id: RunId,
        summary: String,
        elapsed_secs: u64,
        delivery_target: DeliveryFilter,
    },
    Aborted {
        conversation_id: ConversationId,
        run_id: RunId,
        delivery_target: DeliveryFilter,
    },
    // Unchanged — lifecycle events broadcast to all listeners:
    ConversationCreated { conversation: Conversation },
    PortalAttached { portal_id: PortalId, conversation_id: ConversationId },
    PortalDetached { portal_id: PortalId, conversation_id: ConversationId },
    ConversationDeleted { conversation_id: ConversationId },
}
```

**`delivery_target` is non-optional (required) on all response event variants.** This ensures every code path that emits a response must explicitly choose a target — no silent broadcast-all fallback. The compiler enforces this: any new event emission site that forgets `delivery_target` will fail to build.

Lifecycle events (`PortalAttached`, `PortalDetached`, `ConversationCreated`, `ConversationDeleted`) do not have `delivery_target` — they are always broadcast to all listeners.

### Portal Listener Filtering

Each portal listener checks the delivery target on every event before sending:

```rust
// In portal_listener (discord/handler.rs):
// Helper function used by all event handlers:
async fn should_deliver(
    delivery_target: &DeliveryFilter,
    my_portal_id: &PortalId,
    conversation_id: &ConversationId,
    engine: &ConversationEngine,
) -> bool {
    match delivery_target {
        DeliveryFilter::Portal(target_id) => target_id == my_portal_id,
        DeliveryFilter::PrimaryOnly => {
            engine.is_primary_portal(my_portal_id, conversation_id).await
        }
    }
}

// Applied to AssistantMessage, StatusUpdate, Acknowledgment, Error, Aborted:
ConversationEvent::AssistantMessage {
    conversation_id: cid,
    delivery_target,
    content,
    ..
} if cid == conversation_id => {
    if should_deliver(&delivery_target, &my_portal_id, &cid, &engine).await {
        for chunk in chunk_message(&content, 2000) {
            channel_id.say(&http, &chunk).await.ok();
        }
    }
}
```

### Engine Integration

The delivery target is set based on message origin:

```rust
// In ConversationEngine::handle_message() — user-initiated:
// Respond to origin portal only (the portal that sent the message)
let delivery_target = DeliveryFilter::Portal(portal_id);

// In ConversationEngine::send_to_conversation() — scheduler-initiated:
let delivery_target = if let Some(target_portal) = task_portal_id {
    DeliveryFilter::Portal(target_portal)
} else {
    DeliveryFilter::PrimaryOnly
};
```

### Portal-Conversation Validation

When a scheduled task specifies `portal_id`, the scheduler must validate that the portal belongs to the same conversation before delivery. An invalid `portal_id` (wrong conversation, deleted portal) should return a clear error rather than silently dropping the message:

```rust
// In scheduler execution, before calling send_to_conversation():
if let Some(portal_id) = &task.portal_id {
    let valid = engine.portal_belongs_to_conversation(portal_id, &task.conversation_id).await;
    if !valid {
        tracing::warn!(
            task_name = %task.name,
            portal_id = %portal_id.0,
            conversation_id = %task.conversation_id,
            "Scheduled task portal_id does not belong to this conversation; falling back to primary"
        );
        // Fall back to primary instead of silently dropping
        task_portal_id = None;
    }
}
```

### CLI / Discord Interface

The CLI schedule commands already accept `--conversation-id` (via the `Resume` subcommand in `schedule.rs`). We add `--portal-id` alongside it:

```bash
# CLI: create a scheduled task targeting a specific portal
threshold schedule resume \
  --name daily-email-summary \
  --conversation-id <uuid> \
  --cron "0 0 12 * * *" \
  --timezone America/Los_Angeles \
  --portal-id <portal-uuid> \
  --prompt "Check email and summarize"

# CLI: list portals (new subcommand — runs out-of-process, queries daemon API)
threshold portal list
```

Discord:
```
/primary                                  — Make this channel the primary portal
/schedule ... portal:here                 — Target this channel's portal specifically
```

### Daemon API for Portal Commands

CLI subcommands run out-of-process and communicate with the daemon over the Unix socket API (`~/.threshold/threshold.sock`). The `threshold portal list` command follows the same pattern as `threshold schedule list`:

```rust
// crates/scheduler/src/daemon_api.rs — extend DaemonCommand
pub enum DaemonCommand {
    // ... existing variants (Health, Drain, Undrain, Schedule*) ...
    /// List all portals (optionally filtered by conversation).
    PortalList { conversation_id: Option<String> },
}
```

The `DaemonApi` needs access to the conversation engine to query portals. Pass an `Arc<ConversationEngine>` alongside the existing `SchedulerHandle`:

```rust
// crates/scheduler/src/daemon_api.rs — add engine field
pub struct DaemonApi {
    scheduler: Option<SchedulerHandle>,
    engine: Option<Arc<ConversationEngine>>,  // NEW — for portal queries
    health_config: HealthConfig,
    daemon_state: Arc<DaemonState>,
    socket_path: PathBuf,
}

// Dispatch handler:
DaemonCommand::PortalList { conversation_id } => {
    match &self.engine {
        Some(engine) => {
            let portals = engine.list_portals(conversation_id.as_deref()).await;
            DaemonResponse::ok(serde_json::to_value(&portals).unwrap_or_default())
        }
        None => DaemonResponse::error("not_available", "Engine not initialized"),
    }
}
```

CLI handler follows the existing schedule.rs pattern:

```rust
// crates/server/src/portal.rs (new file)
pub async fn handle_portal_command(command: PortalCommands) -> anyhow::Result<()> {
    let client = DaemonClient::new()?;
    let daemon_command = match &command {
        PortalCommands::List { conversation_id, .. } => {
            DaemonCommand::PortalList { conversation_id: conversation_id.clone() }
        }
    };
    let response = client.send_command(&daemon_command).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}
```

Server wiring in `main.rs`:

```rust
// crates/server/src/main.rs — pass engine to DaemonApi
let daemon_api = DaemonApi::new(
    scheduler_cmd_handle,
    Some(engine.clone()),  // NEW — for portal queries
    health_config,
    daemon_state,
    socket_path,
);
```

---

## Implementation Phases

### Phase 15A — Primary Portal Designation

**Goal:** Every conversation has a primary portal. The infrastructure for targeted delivery exists.

**Changes:**

| File | Change |
|------|--------|
| `crates/core/src/types.rs` | Add `primary_portal: Option<PortalId>` to `Conversation` (with `#[serde(default)]` for backward compat) |
| `crates/conversation/src/engine.rs` | Add `maybe_set_primary()` on portal attach. Add `maybe_reassign_primary()` on portal detach (lock-safe: portals read first, then conversations write). Add `set_primary_portal()` for explicit override. Add `is_primary_portal()` query. Add `backfill_primary_portals()` for startup migration. |
| `crates/conversation/src/store.rs` | Ensure `save()` persists the new field. Add `all_mut()` for backfill iteration. |
| `crates/discord/src/commands.rs` | Add `/primary` slash command — resolves portal, calls `set_primary_portal()`. |
| `crates/discord/src/bot.rs` | Register `/primary` command. |
| Test struct literal sites | Add `primary_portal: None` to all `Conversation` struct literals (see Struct Literal Update Sites below). |

**Tests:**
- `engine::first_portal_becomes_primary` — Attach portal to conversation, verify it's set as primary.
- `engine::primary_reassigned_on_detach` — Detach primary portal, verify oldest remaining portal becomes primary.
- `engine::primary_not_reassigned_if_not_primary` — Detach a non-primary portal, verify primary unchanged.
- `engine::explicit_set_primary` — Call `set_primary_portal()`, verify it overrides.
- `engine::set_primary_rejects_unattached_portal` — Try to set primary to a portal on a different conversation, verify error.
- `engine::backfill_sets_primary_for_existing_conversations` — Load conversations with `None` primary but attached portals, run backfill, verify primary set.
- `store::primary_portal_persists` — Save and reload, verify primary survives round-trip.
- `store::backward_compat_no_primary` — Load old JSON without `primary_portal`, verify it deserializes as `None`.

### Phase 15B — Portal Source Tagging

**Goal:** Every message carries source metadata. Audit trail and agent context are enriched with portal source info.

**Changes:**

| File | Change |
|------|--------|
| `crates/core/src/types.rs` | Add `MessageSource` enum. Add `PortalType::platform_name()` method. |
| `crates/conversation/src/engine.rs` | Update `handle_message()` — resolve portal type, build `MessageSource::Portal`, pass to timestamp injection. Update `send_to_conversation()` — build `MessageSource::Scheduler`. |
| `crates/conversation/src/engine.rs` | Update timestamp injection format: `[YYYY-MM-DD HH:MM TZ via Platform]`. |
| `crates/conversation/src/audit.rs` | Add `source: Option<MessageSource>` to `AssistantMessage`, `Error`, `Acknowledgment`, `StatusUpdate` variants (with `#[serde(default)]` for backward compat). `UserMessage` already has `portal_id` + `portal_type`. |
| `crates/conversation/src/engine.rs` | Pass `MessageSource` to audit trail writes. |

**Tests:**
- `engine::portal_source_in_timestamp` — Send message via portal, verify `[... via Discord]` in timestamped content.
- `engine::scheduler_source_in_timestamp` — Call `send_to_conversation()`, verify `[... via Scheduler:task-name]` in timestamped content.
- `types::portal_type_platform_name` — Verify `Discord` variant returns `"Discord"`.
- `audit::source_field_round_trip` — Write audit event with `MessageSource`, read it back, verify it deserializes.
- `audit::backward_compat_no_source` — Load old audit JSONL without `source`, verify it parses (requires adding `Deserialize` to `ConversationAuditEvent`).

### Phase 15C — Delivery Filtering & Portal Targeting

**Goal:** User-initiated messages respond to origin portal only. Scheduled task output goes to the primary portal by default or a specified portal. All event types support delivery filtering.

**Changes:**

| File | Change |
|------|--------|
| `crates/conversation/src/engine.rs` | Add `DeliveryFilter` enum. Add `delivery_target: DeliveryFilter` (non-optional) to `AssistantMessage`, `StatusUpdate`, `Acknowledgment`, `Error`, and `Aborted` event variants. |
| `crates/conversation/src/engine.rs` | Update `handle_message()` — set `delivery_target: DeliveryFilter::Portal(portal_id)` (origin portal). |
| `crates/conversation/src/engine.rs` | Update `send_to_conversation()` — accept optional `portal_id` parameter. Set `delivery_target` to `Portal(id)` or `PrimaryOnly`. |
| `crates/discord/src/handler.rs` | Update portal listener — add `should_deliver()` helper. Check `delivery_target` on all event types (`AssistantMessage`, `StatusUpdate`, `Acknowledgment`, `Error`, `Aborted`) before sending to Discord. |
| `crates/scheduler/src/execution.rs` | Pass `task.portal_id` through to `send_to_conversation()`. Validate portal belongs to the task's conversation before delivery (see Portal-Conversation Validation below). |
| `crates/server/src/schedule.rs` | Add `--portal-id` CLI flag to `Resume` (and other) schedule subcommands. |
| `crates/scheduler/src/daemon_api.rs` | Add `PortalList` variant to `DaemonCommand`. Add `engine: Option<Arc<ConversationEngine>>` field to `DaemonApi`. Dispatch `PortalList` to `engine.list_portals()`. |
| `crates/server/src/portal.rs` | **New file.** CLI handler for `threshold portal list` — builds `DaemonCommand::PortalList`, sends to daemon, prints result. Follows `schedule.rs` pattern. |
| `crates/server/src/main.rs` | Add `Portal` subcommand enum. Wire to `portal::handle_portal_command()`. Pass `engine.clone()` to `DaemonApi::new()`. |
| `crates/conversation/src/engine.rs` | Add `list_portals()` method (public, used by daemon API). |
| `crates/discord/src/scheduler_commands.rs` | Add `portal` option to `/schedule` Discord command — resolves current channel's portal and passes it. |

**Tests:**
- `engine::user_message_targets_origin_portal` — `handle_message()` sets `delivery_target: Portal(origin)`, verify only that listener receives.
- `engine::scheduled_task_targets_primary` — `send_to_conversation()` with no portal override, verify `PrimaryOnly`.
- `engine::scheduled_task_targets_specific_portal` — `send_to_conversation()` with portal override, verify `Portal(id)`.
- `handler::listener_filters_by_delivery_target` — Portal listener skips events not targeting it.
- `handler::listener_delivers_when_primary` — Portal listener delivers when it's the primary and target is `PrimaryOnly`.
- `handler::listener_filters_status_and_ack` — Verify `StatusUpdate`, `Acknowledgment`, `Error` events are also filtered (not just `AssistantMessage`).
- `scheduler::portal_mismatch_falls_back_to_primary` — Schedule task with `portal_id` from a different conversation, verify it falls back to primary delivery with a warning log.

---

## Backward Compatibility

All new fields use `Option<T>` with `#[serde(default)]`:

- `Conversation.primary_portal` — `None` for existing conversations. Startup backfill assigns the oldest registered portal as primary. If no portals exist, remains `None` until a portal is attached.
- `ScheduledTask.portal_id` — Already exists and defaults to `None`. No schema change needed. Existing tasks continue to have `None`, meaning output goes to primary portal.
- `MessageSource` in audit trail — `None` for old events. New events include it. Requires adding `Deserialize` to `ConversationAuditEvent` if audit log reading is needed (currently write-only).

The `Conversation` struct literal is constructed in production code and test code. Each phase that adds a field must update all sites — the compiler enforces this via exhaustive struct checking.

**Struct literal update sites for `Conversation`:**

| File | Line | Context |
|------|------|---------|
| `crates/conversation/src/store.rs` | 104 | `create()` — production constructor |
| `crates/core/src/types.rs` | 278 | `tests::conversation_serde_round_trip` |

Note: `get_or_create_general()` delegates to `create()` (`store.rs:171`), so only one production site needs updating. Test helpers that use `create()` indirectly are also covered.

---

## Verification

After all phases:
```bash
cargo test --workspace --lib          # All unit tests pass
cargo build --workspace               # Full compilation
```

Manual testing with running daemon:
1. Start daemon with Discord connected
2. Send a message — verify `[... via Discord]` appears in agent context
3. Verify audit trail entries include `MessageSource::Portal { platform: "Discord" }`
4. Run `/primary` in a channel — verify confirmation message
5. Trigger a scheduled task — verify output goes to primary portal only
6. Create a scheduled task with `--portal-id` — verify output goes to specified portal only
7. Detach the primary portal (switch conversation) — verify primary reassigns to next portal
8. Verify old conversations.json and schedule.json load without errors (backward compat)
9. Verify startup backfill: stop daemon, manually set `primary_portal: null` in conversations.json, restart — verify primary is reassigned

---

## Files Affected (Summary)

| File | Action | Phase |
|------|--------|-------|
| `crates/core/src/types.rs` | Add `primary_portal` to `Conversation`, add `MessageSource` enum, add `PortalType::platform_name()` | 15A, 15B |
| `crates/conversation/src/audit.rs` | Add `source: Option<MessageSource>` to relevant audit event variants | 15B |
| `crates/conversation/src/engine.rs` | Primary portal logic (with lock-safe ordering), startup backfill, source tagging in timestamps, `DeliveryFilter` enum, delivery target on all response events | 15A, 15B, 15C |
| `crates/conversation/src/store.rs` | Persist `primary_portal` field, add `all_mut()` for backfill | 15A |
| `crates/discord/src/commands.rs` | Add `/primary` command | 15A |
| `crates/discord/src/bot.rs` | Register `/primary` | 15A |
| `crates/discord/src/handler.rs` | Delivery target filtering on all event types in portal listener | 15C |
| `crates/discord/src/scheduler_commands.rs` | Add `portal` option to `/schedule` | 15C |
| `crates/scheduler/src/execution.rs` | Pass `task.portal_id` to `send_to_conversation()` with portal-conversation validation | 15C |
| `crates/scheduler/src/daemon_api.rs` | Add `PortalList` to `DaemonCommand`, add `engine` field to `DaemonApi`, dispatch portal queries | 15C |
| `crates/server/src/schedule.rs` | Add `--portal-id` CLI flag to schedule commands | 15C |
| `crates/server/src/portal.rs` | **New file.** CLI handler for `threshold portal list` (daemon API client) | 15C |
| `crates/server/src/main.rs` | Add `Portal` subcommand, wire to `portal.rs`, pass engine to `DaemonApi` | 15C |
| `crates/conversation/src/store.rs` (line 104), `crates/core/src/types.rs` (line 278) | Add `primary_portal: None` to `Conversation` struct literals | 15A |

---

## Resolved Design Questions

1. **Should scheduled output go to all portals or just primary?** — Primary only, by default. Broadcasting identical output to Discord, Teams, and Slack simultaneously is noisy. Users can override with `--portal-id` or a future `--target-all` flag.

2. **Should the agent see `[via Discord]` in its context?** — Yes. It's a lightweight tag (~10 tokens) added to the timestamp injection. It costs almost nothing and enables the agent to be contextually aware ("I see you've switched to Teams" or "as you mentioned on Discord earlier"). Easy to disable via config if it proves noisy.

3. **What happens when the primary portal is from a platform that's offline?** — The message is broadcast as usual. If the platform listener isn't running (e.g., Teams bot is down), the event is simply not consumed. The audit trail still records it. No retry or queuing — the same model as today if Discord is unreachable.

4. **Should primary portal be per-conversation or global?** — Per-conversation. Different conversations may have different primary platforms (e.g., work conversations primary on Teams, personal on Discord). A global default would be too restrictive.

5. **Why `MessageSource` instead of just `PortalId`?** — Scheduled tasks and system actions don't come from a portal. `MessageSource` is a tagged union that covers all origins cleanly. It also carries the platform name, avoiding a portal registry lookup just to get the display label.

6. **How does `/primary` work across platforms?** — Each platform implements its own command (Discord: `/primary`, Teams: equivalent). The command resolves the portal for the current channel and calls `engine.set_primary_portal()`. The conversation engine doesn't know or care which platform made the call.

7. **Why not add `--target-all` now?** — YAGNI. With one platform (Discord), broadcast and single-target are identical. When a second platform is added, we can add `--target-all` in that milestone if needed. The `DeliveryFilter` enum is trivially extensible.

8. **Does `DeliveryFilter::PrimaryOnly` require the portal listener to query the engine?** — Yes, a lightweight `is_primary_portal()` check. This is a read-only lookup on the conversation store, already `Arc<RwLock<>>` shared. The alternative (embedding the primary portal ID in the event) creates a race condition if primary changes between event emission and consumption.

9. **Should user-initiated responses go to all portals or just the origin?** — Origin portal only. If a user sends a message from Discord, the response appears in Discord — not also in Teams. This matches user expectations (you don't expect a text reply to also appear in your email). The agent has the full cross-platform context regardless.

10. **Why repurpose `portal_id` instead of adding `target_portal`?** — `ScheduledTask` already has `portal_id: Option<PortalId>` that is never populated. Adding a second portal field would create ambiguity about which one controls targeting. Repurposing the existing field is cleaner and avoids a migration.

11. **How is lock ordering maintained?** — All pseudocode follows the convention: acquire `portals` lock first (read), drop it, then acquire `conversations` lock (write). This prevents deadlocks from nested lock acquisition. The `maybe_reassign_primary()` function specifically reads portals into a local variable before taking the conversations write lock.

12. **Why is `delivery_target` non-optional instead of `Option<DeliveryFilter>`?** — An `Option` with `None => broadcast-all` creates a dangerous default: any code path that forgets to set the delivery target silently broadcasts to every portal. Making it non-optional means the compiler rejects event emission sites that don't explicitly choose a target. This enforces the origin-only guarantee at compile time.

13. **What happens when `task.portal_id` references a portal from the wrong conversation?** — The scheduler validates portal-conversation membership before delivery. If the portal doesn't belong to the task's conversation, it logs a warning and falls back to primary portal delivery. This prevents silent message drops from misconfigured tasks.

14. **How does `threshold portal list` work if the CLI runs out-of-process?** — Like `threshold schedule list`, it communicates with the daemon over the Unix socket API. A `PortalList` command was added to `DaemonCommand`, dispatched to `engine.list_portals()`. The `DaemonApi` accepts an `Arc<ConversationEngine>` for this purpose.
