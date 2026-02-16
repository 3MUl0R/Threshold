# Milestone 4 Implementation Plan: Discord Portal + Server Binary

**Status:** Planning
**Complexity:** Large
**Dependencies:** Milestones 1-3 complete

## Overview

This milestone creates the first runnable system: a Discord bot that can hold conversations, switch modes, and maintain persistent sessions across restarts. It includes two crates:
- `discord`: Discord bot implementation with poise/serenity
- `server`: Main binary that wires everything together

## Implementation Phases

### Phase 4.1: Discord Bot Setup

**Objective:** Build the Discord bot framework using poise (wraps serenity).

**Files to create:**
- `crates/discord/Cargo.toml`
- `crates/discord/src/lib.rs`
- `crates/discord/src/bot.rs`

**Key types:**
```rust
pub struct BotData {
    pub engine: Arc<ConversationEngine>,
    pub config: DiscordConfig,
    pub outbound: Arc<DiscordOutbound>,
}

type Context<'a> = poise::Context<'a, BotData, ThresholdError>;
type FrameworkError<'a> = poise::FrameworkError<'a, BotData, ThresholdError>;
```

**Main entry point:**
```rust
pub async fn build_and_start(
    engine: Arc<ConversationEngine>,
    config: DiscordConfig,
    token: &str,
    cancel: CancellationToken,
) -> Result<Arc<DiscordOutbound>>
```

**IMPORTANT:** This function must return the DiscordOutbound handle immediately after setup, BEFORE starting the blocking event loop. The server binary needs the handle to share with heartbeat/scheduler. The event loop runs in a spawned task that monitors the cancellation token.

**Framework setup:**
- Register commands: general(), coding(), research(), conversations(), join()
- Set up event handler for message events
- Register pre_command hook for authorization
- Initialize DiscordOutbound in setup closure
- Configure GatewayIntents: GUILD_MESSAGES, MESSAGE_CONTENT, DIRECT_MESSAGES

**Testing:**
- Basic framework builder smoke test
- Verify BotData structure initialization

---

### Phase 4.2: Security Middleware

**Objective:** Implement authorization checks for guild + user allowlist.

**Files to create:**
- `crates/discord/src/security.rs`

**Authorization function:**
```rust
pub fn is_authorized(
    config: &DiscordConfig,
    guild_id: Option<u64>,
    user_id: u64,
) -> bool
```

**Rules:**
- User MUST be in allowed_user_ids
- Guild messages: guild_id MUST match config.guild_id
- DMs: allowed if user is in allowlist (no guild check)
- Unauthorized messages are silently ignored (no response)

**Enforcement points:**
1. Message handler (before processing messages)
2. Poise pre_command hook (before executing slash commands)
3. DM handling (route allowlisted DMs to General conversation)

**Testing:**
- Test authorized guild message (in allowlist + correct guild)
- Test unauthorized guild message (in allowlist but wrong guild)
- Test unauthorized user (not in allowlist)
- Test authorized DM (in allowlist, no guild)
- Test unauthorized DM (not in allowlist)

---

### Phase 4.3: Message Handler

**Objective:** Listen for Discord messages and route them through the conversation engine.

**Files to create:**
- `crates/discord/src/handler.rs`

**Event handler:**
```rust
pub async fn event_handler(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    framework: poise::FrameworkContext<'_, BotData, ThresholdError>,
    data: &BotData,
) -> Result<(), ThresholdError>
```

**Message processing pipeline:**
1. Ignore bot messages (including our own)
2. Authorization check (call is_authorized)
3. Find or create portal for channel (call resolve_or_create_portal)
4. Show typing indicator (msg.channel_id.start_typing)
5. Send message to conversation engine (engine.handle_message)
6. Response delivery handled by background listener (Phase 4.6)

**Pre-command hook:**
```rust
async fn pre_command(ctx: Context<'_>) -> Result<(), ThresholdError> {
    // Authorization check before command execution
    // Log command invocation
}
```

**Response delivery:**
- Background task per active portal subscribes to engine's broadcast channel
- Filters events by conversation_id **AND tracks PortalAttached events**
- When PortalAttached event received for this portal_id, update tracked conversation_id
- This ensures listener continues working after mode switches (/coding, /research, /join)
- Sends AssistantMessage content to Discord via channel.say()
- Handles artifacts (images, etc.) via send_with_attachments()
- Handles Error events by sending error message to Discord
- Handles lag (RecvError::Lagged) gracefully without dying

**Testing:**
- Test message from authorized user is processed
- Test message from unauthorized user is ignored
- Test bot messages are ignored
- Test typing indicator is shown
- Test portal creation for new channels
- Test portal reuse for existing channels

---

### Phase 4.4: Message Chunking

**Objective:** Split messages respecting Discord's 2000-character limit.

**Files to create:**
- `crates/discord/src/chunking.rs`

**Main function:**
```rust
pub fn chunk_message(content: &str, max_len: usize) -> Vec<String>
```

**Split priorities (in order):**
1. Paragraph boundary (double newline: `\n\n`)
2. Single newline (`\n`)
3. Sentence boundary (`. `, `! `, `? `)
4. Word boundary (space)
5. Hard cut (last resort)

**Special handling:**
- Never split inside markdown code blocks (``` ... ```)
- If code block straddles boundary, extend chunk to include closing ```
- Or start new code block in next chunk with opening ```
- Trim whitespace from chunk boundaries
- Never produce empty chunks

**Algorithm:**
1. Track if we're inside a code block (count ``` markers)
2. If inside code block, don't split until after closing ```
3. Otherwise, search backwards from max_len for best split point
4. Apply split priorities in order
5. Handle edge case: code block alone exceeds max_len (hard cut with continuation markers)

**Testing:**
- Test short message (no split)
- Test message split at paragraph boundary
- Test message split at sentence boundary
- Test message with code block (preserved)
- Test code block straddling boundary (extended)
- Test very long code block (hard cut with continuation)
- Test message with multiple code blocks
- Test empty string
- Test whitespace trimming

---

### Phase 4.5: Slash Commands

**Objective:** Implement Poise slash commands for mode switching and conversation management.

**Files to create:**
- `crates/discord/src/commands.rs`

**Commands:**

1. `/general` - Switch to General conversation
   - No parameters
   - Calls engine.switch_mode(&portal_id, ConversationMode::General)
   - Responds with confirmation message

2. `/coding [project]` - Start or resume coding conversation
   - Required parameter: project (String)
   - Calls engine.switch_mode(&portal_id, ConversationMode::Coding { project })
   - Responds with "Switched to **Coding** conversation for `project`."

3. `/research [topic]` - Start or resume research conversation
   - Required parameter: topic (String)
   - Calls engine.switch_mode(&portal_id, ConversationMode::Research { topic })
   - Responds with "Switched to **Research** conversation for `topic`."

4. `/conversations` - List all active conversations
   - No parameters
   - Calls engine.list_conversations()
   - Formats output: ID, mode, last_active timestamp
   - Responds with formatted list

5. `/join [id]` - Join specific conversation by ID
   - Required parameter: id (String)
   - Parse UUID from string
   - Calls engine.join_conversation(&portal_id, &conversation_id)
   - Responds with confirmation or error

**Helper function:**
```rust
async fn resolve_portal(ctx: Context<'_>) -> PortalId {
    // Extract guild_id and channel_id from context
    // Call resolve_or_create_portal
}
```

**Error handling:**
- Invalid UUID for /join: return user-friendly error
- Engine errors: propagate to poise error handler

**Testing:**
- Test /general command switches mode
- Test /coding creates new conversation or resumes existing
- Test /research creates new conversation or resumes existing
- Test /conversations lists correct conversations
- Test /join with valid UUID switches conversation
- Test /join with invalid UUID returns error

---

### Phase 4.6: Channel-as-Portal Mapping

**Objective:** Map Discord channels to portals, automatically creating new portals in General conversation.

**Files to create:**
- `crates/discord/src/portals.rs`

**Main function:**
```rust
pub async fn resolve_or_create_portal(
    engine: &ConversationEngine,
    guild_id: u64,
    channel_id: u64,
) -> PortalId
```

**Logic:**
1. Acquire read lock on portals
2. Search for existing portal with find_by_discord_channel(guild_id, channel_id)
3. If found, return portal.id
4. Drop read lock
5. Create new portal with register_portal(PortalType::Discord { guild_id, channel_id })
6. New portals start in General conversation
7. Return new portal_id

**Portal listener lifecycle:**
- Created when portal first sends a message
- Subscribes to engine's broadcast channel
- **Tracks current conversation_id dynamically:**
  - Starts with initial conversation_id
  - Watches for `ConversationEvent::PortalAttached { portal_id, conversation_id }`
  - When PortalAttached event matches this portal_id, updates tracked conversation_id
  - This ensures listener continues working after mode switches (/coding, /research, /join)
- Filters AssistantMessage and Error events by tracked conversation_id
- Runs until channel closed or portal unregistered
- Handles lag gracefully (RecvError::Lagged)

**Portal listener function:**
```rust
async fn portal_listener(
    portal_id: PortalId,
    mut conversation_id: ConversationId,  // mutable, updated on PortalAttached
    channel_id: serenity::ChannelId,
    receiver: broadcast::Receiver<ConversationEvent>,
    http: Arc<serenity::Http>,
    outbound: Arc<DiscordOutbound>,
)
```

**Testing:**
- Test new channel creates portal in General conversation
- Test existing channel reuses portal
- Test multiple channels create separate portals
- Test portal attached to correct conversation after mode switch

---

### Phase 4.7: Agent-Initiated Discord Actions

**Objective:** Allow system to push messages to Discord (for heartbeat, cron, etc.).

**Files to create:**
- `crates/discord/src/outbound.rs`

**Main type:**
```rust
pub struct DiscordOutbound {
    http: Arc<serenity::Http>,
}
```

**Methods:**

1. `new(http: Arc<serenity::Http>) -> Self`
   - Constructor

2. `send_to_channel(&self, channel_id: u64, content: &str) -> Result<()>`
   - Send text message to channel
   - Convert channel_id to serenity::ChannelId
   - Call channel_id.say(&self.http, content)

3. `send_dm(&self, user_id: u64, content: &str) -> Result<()>`
   - Send DM to user
   - Create DM channel with user
   - Send message

4. `create_channel(&self, guild_id: u64, name: &str, topic: &str) -> Result<u64>`
   - Create new text channel in guild
   - Set channel topic
   - Return channel_id

5. `send_with_attachments(&self, channel_id: u64, content: &str, attachments: Vec<(String, Vec<u8>)>) -> Result<()>`
   - Send message with file attachments
   - attachments: Vec<(filename, data)>
   - Use serenity's AttachmentType::Bytes

**Error handling:**
- Convert serenity errors to ThresholdError
- Log all outbound actions

**Testing:**
- Unit test: DiscordOutbound construction
- Integration test (requires Discord): send_to_channel
- Integration test (requires Discord): send_with_attachments
- Mock test: verify correct serenity API calls

---

### Phase 4.8: Server Binary

**Objective:** Wire everything together into the main binary.

**Files to create:**
- `crates/server/Cargo.toml`
- `crates/server/src/main.rs`

**Main function structure:**
```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Load config
    // 2. Initialize logging
    // 3. Initialize secret store
    // 4. Verify Claude CLI
    // 5. Create conversation engine
    // 6. Shared cancellation token
    // 7. Shared outbound handle (Arc<RwLock<Option<Arc<DiscordOutbound>>>>)
    // 8. Build all task futures
    // 9. Run all tasks concurrently with tokio::select!
    // 10. Graceful shutdown
}
```

**Startup verification sequence:**
1. Load config → fail fast if missing/invalid
2. Init logging → get logs ASAP
3. Init secret store → fail fast if keychain unavailable
4. Verify CLI: `claude --version` → fail fast if not installed
5. Create conversation engine → load persisted state
6. Resolve Discord token from secrets → fail fast if missing
7. Spawn concurrent tasks → Discord, heartbeat (no-op), scheduler (no-op)
8. Log readiness → "Threshold is ready."

**Discord task:**
- Check if config.discord is Some
- Resolve discord-bot-token from secrets
- Call discord::build_and_start(engine, config, token, cancel) → returns outbound immediately
- Publish DiscordOutbound to shared slot
- Wait for cancellation token (bot event loop runs in spawned background task)

**Heartbeat task (no-op for now):**
- Wait for cancellation token
- Milestone 6 will implement actual heartbeat logic

**Scheduler task (no-op for now):**
- Wait for cancellation token
- Milestone 7 will implement actual scheduler logic

**Graceful shutdown:**
1. Cancel all tasks (cancel.cancel())
2. Save engine state (engine.save_state())
3. Log "Threshold shut down cleanly."

**Key design decision: Shared DiscordOutbound**
- DiscordOutbound is created during Discord bot setup
- Published into Arc<RwLock<Option<Arc<DiscordOutbound>>>>
- Heartbeat and scheduler read from this slot
- If Discord not configured, slot stays None
- Avoids circular dependency where heartbeat/scheduler need DiscordOutbound but it's created inside Discord setup

**Testing:**
- Integration test: Start server, verify it connects to Discord
- Integration test: Send message, verify response
- Integration test: SIGINT triggers graceful shutdown
- Unit test: Config loading error handling
- Unit test: Missing secret error handling

---

## Implementation Order

0. **Pre-phase**: Add missing ConversationEngine methods
   - `save_state() -> Result<()>`
   - `portals() -> Arc<RwLock<PortalRegistry>>`
1. **Phase 4.1**: Discord bot framework setup
2. **Phase 4.2**: Security middleware (blocking all other phases)
3. **Phase 4.4**: Message chunking (utility needed by Phase 4.3)
4. **Phase 4.7**: Outbound (needed by Phase 4.3 listener)
5. **Phase 4.6**: Portal mapping (needed by Phase 4.3)
6. **Phase 4.3**: Message handler (depends on 4.2, 4.4, 4.6, 4.7)
7. **Phase 4.5**: Slash commands (depends on 4.6)
8. **Phase 4.8**: Server binary (wires everything together)

## Dependency Analysis

**External crates:**
- `poise = "0.6"` - Discord bot framework
- `serenity = { version = "0.12", features = ["client", "gateway", "model"] }` - Discord API
- `tokio-util = "0.7"` - For CancellationToken
- `anyhow = "1"` - Error handling in main

**Internal crates:**
- `threshold-core` - ThresholdError, PortalType, ConversationMode, ConversationId, PortalId, DiscordConfig, SecretStore (already exists)
- `threshold-conversation` - ConversationEngine, ConversationEvent
- `threshold-cli-wrapper` - ClaudeClient

**Already implemented (from Milestones 1-3):**
- ✅ `SecretStore` in threshold-core (Milestone 1)
- ✅ `ConversationEngine::list_conversations()` (Milestone 3)
- ✅ `ThresholdConfig::data_dir()` helper method (Milestone 1)
- ✅ `ThresholdConfig.log_level` field (Milestone 1)
- ✅ `ConversationEvent::PortalAttached` for tracking mode switches (Milestone 3)

**Need to add to ConversationEngine:**
- `save_state() -> Result<()>` - Save both conversations and portals to disk
- `portals() -> Arc<RwLock<PortalRegistry>>` - Expose portals for portal listener management

## Testing Strategy

**Unit tests:**
- Security: authorization checks (5 tests)
- Chunking: message splitting (8 tests)
- Portals: resolve/create logic (4 tests)
- Commands: slash command logic (6 tests)

**Integration tests:**
- Bot connects to Discord (requires test bot token)
- Bot responds to authorized messages
- Bot ignores unauthorized messages
- Mode switching works end-to-end
- State persists across restarts
- Graceful shutdown saves state

**Manual verification:**
- Connect to real Discord server
- Test all slash commands
- Verify typing indicator
- Test long messages are chunked
- Test code blocks preserved
- Verify conversations list
- Test mode switching
- Restart server, verify state persisted

## Error Handling

**Fail-fast on startup:**
- Config missing/invalid → exit with clear error message
- Discord token missing → exit with clear error message
- Claude CLI not installed → exit with clear error message
- Keychain unavailable → exit with clear error message

**Runtime errors:**
- Unauthorized messages → silently ignored (no response)
- Engine errors → send error message to Discord channel
- Network errors (Discord API) → log and retry (serenity handles this)
- Broadcast lag (RecvError::Lagged) → log warning, continue
- Broadcast closed (RecvError::Closed) → shutdown portal listener

## Security Considerations

1. **Authorization enforcement:** Every message and command MUST check is_authorized()
2. **Silent rejection:** Unauthorized requests return no response (don't leak bot existence)
3. **DM allowlist:** Only users in allowed_user_ids can DM the bot
4. **Guild isolation:** Bot only responds in configured guild_id
5. **Token security:** Discord token from secure secret store
6. **Audit logging:** All messages logged to conversation audit trail

## Performance Considerations

1. **Portal listener per channel:** Background task for each active channel
2. **Broadcast channel capacity:** 100 events (configured in engine)
3. **Lag handling:** Portal listeners handle lag gracefully, don't block engine
4. **Lock duration:** Hold locks only for lookup/update, never across await points
5. **Typing indicator:** Start before processing, automatically stops on message send

## Files to Create

```
crates/discord/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── bot.rs
    ├── security.rs
    ├── handler.rs
    ├── commands.rs
    ├── chunking.rs
    ├── portals.rs
    └── outbound.rs

crates/server/
├── Cargo.toml
└── src/
    └── main.rs
```

## Lines of Code Estimate

- discord/bot.rs: ~150 lines
- discord/security.rs: ~80 lines
- discord/handler.rs: ~200 lines
- discord/commands.rs: ~180 lines
- discord/chunking.rs: ~150 lines
- discord/portals.rs: ~120 lines
- discord/outbound.rs: ~130 lines
- discord/lib.rs: ~30 lines
- server/main.rs: ~250 lines

**Total: ~1,290 lines**

## Success Criteria

1. ✅ Server starts and connects to Discord
2. ✅ Bot responds to authorized users in correct guild
3. ✅ Bot ignores unauthorized users
4. ✅ Bot ignores its own messages
5. ✅ Typing indicator shows while processing
6. ✅ `/coding myproject` switches conversation
7. ✅ `/general` switches back
8. ✅ `/conversations` lists all conversations
9. ✅ `/join <id>` attaches channel to conversation
10. ✅ Long messages chunked correctly (>2000 chars)
11. ✅ Code blocks preserved across chunks
12. ✅ State persists across restarts
13. ✅ Graceful shutdown saves state
14. ✅ Fail-fast with clear errors on misconfiguration

## Notes

- This is the first runnable system - major milestone!
- Portal listeners create 1 background task per active channel
- DiscordOutbound is shared across subsystems via Arc<RwLock<Option<...>>>
- Heartbeat and scheduler are no-ops until Milestones 6 & 7
- Security is critical: EVERY entry point must check authorization
- Message handler pipeline is the core flow: portal → conversation → agent → Claude → audit → broadcast → Discord
