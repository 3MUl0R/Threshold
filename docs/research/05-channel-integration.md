# Channel Integration & Message Pipeline — Patterns & Architecture

## Summary

OpenClaw uses a plugin-based channel architecture where each messaging platform
(Telegram, Discord, WhatsApp, etc.) is an extension that registers with the core
via a standard SDK contract. Messages are normalized into a common `MsgContext`
format, routed to the appropriate AI agent, and responses are delivered back
through platform-specific outbound adapters.

The system currently supports 30+ platforms, but the patterns are what matter
for our project — we only need 2-3 channels to start.

---

## Message Flow Pipeline

```
1. RECEPTION         Platform-specific handler receives message
                     (webhook, bot framework, WebSocket event)
      │
      ▼
2. DEBOUNCE          Buffer rapid-fire messages (configurable ms)
                     Coalesce media groups
      │
      ▼
3. PREFLIGHT         Validate sender (allowlist, pairing policy)
                     Extract metadata, check for control commands
      │
      ▼
4. ROUTING           Resolve which agent handles this message
                     Build session key based on dmScope config
      │
      ▼
5. NORMALIZATION     Convert platform message → MsgContext
                     Build envelope with sender/timestamp context
      │
      ▼
6. AUTO-REPLY CHECK  Group: require mention or allowlist bypass
                     DM: check policy (open/pairing/closed)
      │
      ▼
7. DISPATCH          Load session, build context, call AI agent
                     Stream response tokens
      │
      ▼
8. CHUNKING          Split response for platform character limits
                     Preserve markdown across chunks
      │
      ▼
9. DELIVERY          Send via platform-specific outbound adapter
                     Handle media, threading, reactions
      │
      ▼
10. RECORDING        Append to session transcript (JSONL)
                     Update session metadata
```

---

## Plugin Architecture

### Channel Plugin Contract

Each channel implements this interface:

```typescript
type ChannelPlugin = {
  id: string;                              // "discord", "telegram", etc.
  meta: ChannelMeta;                       // Display name, icon
  capabilities: ChannelCapabilities;       // What this platform supports

  config: ChannelConfigAdapter;            // Config validation/resolution
  security?: ChannelSecurityAdapter;       // Allowlist, auth gates
  pairing?: ChannelPairingAdapter;         // New user approval flow
  gateway?: ChannelGatewayAdapter;         // Gateway server hooks
  outbound?: ChannelOutboundAdapter;       // Message delivery
  setup?: ChannelSetupAdapter;             // Onboarding flow
  groups?: ChannelGroupAdapter;            // Group/channel discovery
  mentions?: ChannelMentionAdapter;        // Bot mention detection
  actions?: ChannelMessageActionAdapter;   // Pin, react, delete, etc.
  streaming?: ChannelStreamingAdapter;     // Block streaming config
  threading?: ChannelThreadingAdapter;     // Thread reply modes
  auth?: ChannelAuthAdapter;              // Login/QR flows
  heartbeat?: ChannelHeartbeatAdapter;    // Periodic keep-alive messages
  directory?: ChannelDirectoryAdapter;    // User/group lookup
  agentTools?: ChannelAgentToolFactory;   // Channel-specific AI tools
};
```

### Registration

```typescript
// extensions/my-channel/index.ts
const plugin = {
  id: "my-channel",
  name: "My Channel",
  register(api: OpenClawPluginApi) {
    api.registerChannel({ plugin: channelPlugin });
  }
};
```

Plugins are loaded at startup from `extensions/` directories and registered
into a global plugin registry. The gateway iterates enabled channels and
starts their monitors.

---

## Message Normalization (MsgContext)

All platform messages are normalized into ~80+ fields. Key ones:

### Core Message
| Field | Description |
|-------|-------------|
| `Body` | Raw message text |
| `BodyForAgent` | Text with envelope/history context |
| `Channel` | Platform identifier ("discord", "telegram") |
| `ChatType` | "direct" / "group" / "channel" |
| `Timestamp` | Unix milliseconds |

### Identity
| Field | Description |
|-------|-------------|
| `SenderId` | Platform user ID |
| `SenderName` | Display name |
| `SenderUsername` | @handle or equivalent |
| `From` | Human label: "Alice (123)" |
| `To` | Chat/channel/DM target ID |

### Session Routing
| Field | Description |
|-------|-------------|
| `SessionKey` | Resolved agent session key |
| `AccountId` | Multi-account identifier |
| `WasMentioned` | Whether bot was @mentioned |
| `CommandAuthorized` | Passed auth gate for commands |

### Media
| Field | Description |
|-------|-------------|
| `MediaPath` | Local file path (downloaded) |
| `MediaUrl` | Remote URL |
| `MediaType` | MIME type |
| `MediaPaths` | Array for multi-media |

### Threading
| Field | Description |
|-------|-------------|
| `MessageThreadId` | Thread/topic ID |
| `ReplyToId` | Platform reply-to message ID |

---

## Routing & Agent Resolution

### Route Resolution Priority

When a message arrives, the system resolves which agent handles it:

1. **Direct peer binding** — explicit mapping of user/chat ID to agent
2. **Parent peer binding** — thread inherits parent channel's binding
3. **Guild binding** — Discord server-level (all channels in a guild)
4. **Team binding** — MS Teams team-level
5. **Account binding** — all messages for this bot account
6. **Default** — global default agent

### Session Key Construction

Based on `dmScope` config:

| Scope | Key Pattern | Behavior |
|-------|-------------|----------|
| `main` | `agent:<id>:main` | ALL messages → one session |
| `per-peer` | `agent:<id>:dm:<peerId>` | Per user, cross-channel |
| `per-channel-peer` | `agent:<id>:<channel>:dm:<peerId>` | Per user per platform |
| `per-account-channel-peer` | `agent:<id>:<channel>:<account>:dm:<peerId>` | Full isolation |

### Identity Linking (Cross-Channel)

```yaml
session:
  identityLinks:
    alice:
      - "telegram:123456789"
      - "discord:987654321"
```

With `dmScope: per-peer`, Alice gets the same session whether she messages
via Telegram or Discord.

---

## Auto-Reply Decision Logic

### DM Policy
- `"open"` — reply to anyone
- `"pairing"` — allowlist only, new users must be approved via `/pair`
- `"closed"` — never reply to DMs

### Group Activation
- `"mention"` (default) — require @bot mention
- `"always"` — respond to every message (dangerous in large groups)

### Allowlist Matching
Checked in order: exact ID → normalized slug → parent/wildcard → `"*"` wildcard.

---

## Outbound Delivery

### Platform Character Limits

| Platform | Limit | Chunking Strategy |
|----------|-------|-------------------|
| Discord | 2,000 chars | Paragraph boundaries, preserve markdown |
| Telegram | 4,096 chars | Paragraph boundaries, preserve markdown |
| WhatsApp | 1,600 chars | Paragraph boundaries |

### Outbound Adapter Interface

```typescript
type ChannelOutboundAdapter = {
  deliveryMode: "direct" | "gateway" | "hybrid";
  textChunkLimit?: number;
  sendText: (ctx) => Promise<OutboundDeliveryResult>;
  sendMedia: (ctx) => Promise<OutboundDeliveryResult>;
  sendPoll?: (ctx) => Promise<ChannelPollResult>;
};
```

Each delivery returns a result with the platform's message ID, which
can be used for threading replies.

---

## Gateway Server

### Architecture
- HTTP + WebSocket server (Hono framework)
- Binary protocol with frame-based messaging
- RPC-style method handlers with scope-based authorization

### Key RPC Methods
| Method | Description |
|--------|-------------|
| `chat.send` | Send message to AI agent |
| `chat.history` | Retrieve session transcript |
| `chat.subscribe` | Subscribe to real-time session updates |
| `send` | Send message to a messaging platform |
| `channels.status` | Get all channel connection statuses |
| `sessions.list` | List active sessions |

### Client Types
- `"cli"` — command-line interface
- `"android"` / `"ios"` — mobile apps
- `"observer"` — read-only monitoring
- `"executor"` — can trigger actions

### Authorization Scopes
- `operator.admin` — full access
- `operator.read` — read-only
- `operator.write` — send messages
- `operator.pairing` — approve new users

---

## Envelope Formatting

Messages sent to the AI include an "envelope" with context:

```
[Discord Alice id:12345 2024-02-08 14:30:45 UTC] hi there
[+5m] Alice: what's up?
```

Configurable:
- Timezone: "local", "utc", "user", or IANA zone
- Include/exclude timestamps
- Show elapsed time between messages

---

## Takeaways for Our Project

### What to adopt

- **Plugin contract pattern** — even with only 2-3 channels, a clean interface
  keeps the core decoupled. Define a `ChannelPlugin` trait in Rust.
- **Message normalization** — a common `Message` struct that all channels
  produce. Don't let platform-specific types leak into core logic.
- **Outbound adapter pattern** — each channel knows its own delivery rules
  (character limits, media formats, threading). Core just sends payloads.
- **Gateway WebSocket** — essential for your room-portal vision. Mobile/desktop
  devices connect via persistent WebSocket for real-time streaming.
- **Envelope context** — telling the AI "this came from Discord, user Alice,
  at 2:30 PM" is valuable for contextual responses.

### What to redesign for shared sessions

OpenClaw's session model is **per-channel or per-user**, not per-conversation.
Your vision is fundamentally different:

**OpenClaw model**:
```
Discord Alice  ──→  Session A
Telegram Alice ──→  Session A  (via identity linking)
Voice Portal   ──→  Session A  (via identity linking)
```
This works for "same user, same session" but every message is isolated
to the channel it came from. There's no concept of "join a conversation
from a different portal."

**Your model**:
```
Default conversation (always running):
  ├── Discord portal ──→ reads/writes same conversation
  ├── Kitchen speaker ──→ reads/writes same conversation
  └── Phone app ──────→ reads/writes same conversation

Coding session (explicitly started):
  ├── VS Code terminal ──→ separate session
  └── Discord #coding ───→ same coding session
```

Key differences:
1. **Conversations are first-class**, not derived from channels
2. **Portals attach/detach** from conversations dynamically
3. **Mode switching** via slash command creates a new session context
4. **Default conversation** is always available, always the same session

### What to simplify

- **30+ channel plugins** → start with Discord + voice portal + web UI
- **Allowlist/pairing** → single-user system, you ARE the allowlist
- **Multi-account routing** → one account per channel is enough
- **Debouncing** → simpler with a Rust async pipeline (tokio channels)

### Session architecture sketch for our system

```rust
enum ConversationMode {
    General,           // Default always-on assistant
    Coding { project: String },
    Research { topic: String },
    // extensible
}

struct Conversation {
    id: Uuid,
    mode: ConversationMode,
    cli_session_id: Option<String>,  // Claude/Codex session
    created_at: DateTime,
    last_active: DateTime,
}

struct Portal {
    id: Uuid,
    channel: ChannelType,       // Discord, Voice, Web, etc.
    conversation_id: Uuid,      // Which conversation this portal is in
    connected_at: DateTime,
}

// User says "/coding myproject" in Discord:
// 1. Create new Conversation { mode: Coding, cli_session_id: new_uuid() }
// 2. Detach Discord portal from General conversation
// 3. Attach Discord portal to Coding conversation
// 4. All messages from Discord now go to coding session
//
// User says "/general" or "/home":
// 1. Detach from Coding conversation
// 2. Reattach to General conversation
// 3. Coding conversation stays alive (can be resumed later)
```
