# Product Plan — Home Assistant AI Platform

## Vision

A secure, self-hosted AI assistant that lives on your home network. Talk to it
from any room, any device, any messaging platform — and it's always the same
conversation unless you tell it otherwise. Your data stays on your machine.
Your audio never leaves your network. The intelligence comes from Claude and
Codex CLIs running as subprocesses, borrowing your existing subscriptions.

Built in Rust for long-term stability. Runs for months without restarts.
No plugins from the internet. No marketplace. No third-party code execution.
Every integration is native, open source, and auditable.

---

## Core Architecture

### CLI-First Inference

The assistant does NOT call AI provider APIs directly for conversations.
Instead, it spawns `claude` and `codex` as subprocesses:

```
User message → Rust server → CLI subprocess (claude/codex) → Response
```

**Why this matters:**
- CLIs handle context window management, compaction, and caching internally
- Token efficiency is dramatically better than raw API calls
- Authentication is borrowed from existing CLI credentials — no OAuth dance
- Tool permissions managed by the CLIs themselves
- We get improvements for free as CLIs are updated

**Direct API usage** is reserved for short, stateless tasks only:
- Message classification / routing ("which agent handles this?")
- Small extraction ("what's the meeting time from this email?")
- Summary generation ("one-line summary of this thread")
- Never for multi-turn conversations

### Credential Borrowing

The server reads existing CLI credentials rather than managing its own:

- **Claude**: `~/.claude/.credentials.json` or macOS Keychain
- **Codex**: `~/.codex/auth.json` or macOS Keychain `"Codex Auth"`

If tokens expire, the user re-authenticates via the CLI directly.
The server clears `ANTHROPIC_API_KEY` from the environment when spawning
CLI subprocesses to ensure subscription-rate billing.

### Rust Backend

Single binary server built with:
- `tokio` — async runtime for WebSocket server, subprocess management, I/O
- `serde` / `serde_json` — typed deserialization of CLI output
- `anyhow` — ergonomic error handling in application code
- `cpal` / `rodio` / `symphonia` — audio I/O and processing
- `keyring` — OS keychain for API key storage
- `axum` or `warp` — HTTP/WebSocket gateway server
- `serenity` / `poise` — Discord bot integration

Cross-compiles to ARM for Raspberry Pi room portals. Single binary
deployment — no runtime dependencies on target devices.

---

## Conversations & Portals

### Conversations Are First-Class

A conversation is an independent entity with its own CLI session, history,
and context. Conversations are NOT derived from channels or platforms.

```rust
struct Conversation {
    id: Uuid,
    mode: ConversationMode,
    cli_session_id: Option<String>,  // Claude/Codex session ID
    cli_provider: CliProvider,       // Claude or Codex
    created_at: DateTime<Utc>,
    last_active: DateTime<Utc>,
}

enum ConversationMode {
    General,                         // Default always-on assistant
    Coding { project: String },      // Development context
    Research { topic: String },      // Deep research session
    Creative { context: String },    // Writing, brainstorming
    // Extensible — new modes added to codebase, not plugins
}
```

**Default General conversation** is always running. It's the home screen.
Every portal starts attached to General unless explicitly switched.

### Portals Attach/Detach

A portal is any interface through which a user interacts with the assistant.
Portals attach to conversations dynamically.

```rust
struct Portal {
    id: Uuid,
    portal_type: PortalType,
    conversation_id: Uuid,          // Currently attached conversation
    connected_at: DateTime<Utc>,
    capabilities: PortalCapabilities,
}

enum PortalType {
    Discord { channel_id: u64, guild_id: u64 },
    Voice { device_id: String, room: String },
    Web { session_token: String },
    Phone { number: String },
}

struct PortalCapabilities {
    can_send_text: bool,
    can_send_media: bool,
    can_play_audio: bool,
    can_record_audio: bool,
    can_display_rich: bool,         // Markdown, embeds, etc.
}
```

**Multiple portals in the same conversation simultaneously:**
When the AI responds, ALL portals attached to that conversation receive the
response. Voice portals speak it. Text portals display it. This is the core
of the "talk to your house" experience.

### Mode Switching

Users switch conversations via commands:

```
/general          → Attach to the default General conversation
/coding myproject → Create or resume a Coding conversation for "myproject"
/research quantum → Create or resume a Research conversation on "quantum"
/conversations    → List all active conversations
/join <id>        → Attach current portal to a specific conversation
```

When you switch, the current portal detaches from its conversation and
attaches to the new one. The old conversation stays alive — you can
return to it later, and it remembers everything.

### Session Persistence

Two layers of persistence:

1. **CLI-managed session** — the Claude/Codex CLI maintains its own session
   internally. We store the `session_id` / `thread_id` and pass it when
   resuming conversations. The CLI handles context, compaction, history.

2. **Audit trail** — append-only JSONL log of all messages and events.
   This is OUR record, not the CLI's. Used for:
   - Displaying conversation history in the web UI
   - Cross-portal message replay ("what did we talk about in the kitchen?")
   - Debugging and accountability
   - Never fed back to the CLI as context (the CLI manages its own)

```
~/.assistant/
├── config.toml                    # Non-sensitive configuration
├── conversations/
│   ├── general.jsonl              # Audit trail for General
│   ├── coding-myproject.jsonl     # Audit trail for coding session
│   └── research-quantum.jsonl
└── state/
    ├── portals.json               # Active portal registry
    └── conversations.json         # Conversation metadata
```

---

## Portal Implementations

### Discord

**Primary text interface.** Users interact via Discord channels.

- Built with `serenity`/`poise` crate for Rust
- Each Discord channel is a portal that can be attached to a conversation
- Slash commands for mode switching (`/coding`, `/research`, `/general`)
- Bot mention or DM triggers the assistant
- Supports text, images, file attachments, embeds
- Message chunking respects Discord's 2000-char limit with markdown preservation
- Threading support — replies in threads keep context clean

**Default behavior:** All channels in the Discord server start attached to
the General conversation. `/coding myproject` in a specific channel detaches
that channel and creates/resumes a coding session.

### Voice Portals (Room Speakers)

**The "talk to your house" experience.** Raspberry Pi or similar devices
with microphone + speaker in each room.

Hardware per portal:
- Raspberry Pi (or old phone, or any ARM/x86 device)
- USB microphone or microphone array
- Speaker (built-in, USB, or Bluetooth)
- Network connection (Wi-Fi or Ethernet)

Software on portal device:
- Thin Rust client binary (cross-compiled for ARM)
- Local wake word detection (OpenWakeWord or Porcupine)
- Local STT if hardware supports it (Whisper.cpp) — otherwise streams to server
- Local TTS playback (Piper for local, or receives audio from server)
- WebSocket connection to home server

**State machine per voice portal:**
```
IDLE
  │ Wake word detected locally
  ▼
LISTENING
  │ Speech → local STT or stream to server
  ▼
PROCESSING
  │ Transcript sent to server → CLI → AI response
  ▼
SPEAKING
  │ TTS playback (interruptible via barge-in)
  ▼
IDLE
```

**Barge-in:** User can speak during AI response. TTS queue is cleared,
audio buffer flushed, and the system immediately switches to LISTENING.

**Room awareness:** Each portal knows its room name. The AI can be told
"I'm in the kitchen" contextually. Useful for smart home integration later.

### Web UI

**Gateway web interface** bound to localhost (`127.0.0.1`). Not accessible
off-device unless the user explicitly sets up a tunnel (SSH, Tailscale, etc.).

Features:
- Real-time conversation view (WebSocket streaming)
- Conversation switching
- Portal status dashboard (which portals are active, what they're attached to)
- Configuration management (with sentinel-based redaction for secrets)
- Tool output display (browser screenshots, file contents, etc.)
- Voice interface (browser microphone → STT → AI → TTS → browser speaker)

Built as a simple SPA — the server serves static files and provides a
WebSocket + REST API. No heavy frontend framework needed initially.

### Phone (Future)

Phone call integration via Twilio or similar:
- Inbound calls → STT → AI → TTS → caller hears response
- Outbound calls triggered by the AI ("call me when the build finishes")
- Audio pipeline: mu-law 8kHz ↔ PCM conversion
- Separate TTS config for telephony (different voice/quality tradeoffs)

---

## Voice Architecture

### Privacy-First Principle

```
Audio captured in room
    ↓ (stays local)
Wake word detected locally (never leaves device)
    ↓
Speech-to-text locally (Whisper.cpp on device or server)
    ↓ (only text leaves your network)
Text sent to Claude/Codex CLI
    ↓
Response text received
    ↓ (only text)
Text-to-speech locally (Piper on device or server)
    ↓ (stays local)
Audio played on room speaker
```

**Raw audio NEVER leaves the home network.** Only transcribed text goes to
AI providers, and even that goes through the CLI's encrypted channel.

### Speech-to-Text (STT)

**Local-first options:**
- **Whisper.cpp** — best accuracy, runs on CPU or GPU. Preferred if hardware
  supports it (Pi 4+ or server with decent CPU).
- **Vosk** — lighter weight, faster, many languages. Good for constrained devices.

**Cloud fallback (opt-in only):**
- **Groq Whisper** — extremely fast cloud inference. Groq's custom hardware
  provides high throughput for Whisper models. Good option when local hardware
  is insufficient but user is comfortable with cloud STT.
- **OpenAI Whisper API** — standard cloud fallback.

**Configuration:**
```toml
[voice.stt]
provider = "whisper-local"    # "whisper-local", "vosk", "groq", "openai"
model = "base.en"             # Whisper model size
language = "en"               # Language hint
device = "cpu"                # "cpu" or "cuda"

[voice.stt.groq]
# Only used if provider = "groq"
# API key stored in OS keychain, not here
model = "whisper-large-v3-turbo"
```

### Text-to-Speech (TTS)

**Local-first options:**
- **Piper TTS** — fast local neural TTS. Many voices, multiple languages.
  Runs well on Raspberry Pi. Default choice.
- System TTS — OS-native speech synthesis as ultimate fallback.

**Cloud upgrade (opt-in):**
- **ElevenLabs** — highest quality, most natural. Per-character billing.
  Best for users who want premium voice quality.
- **OpenAI TTS** — good quality, low latency. Multiple voices.
- **Edge TTS** — free, Microsoft neural voices. No API key needed.
  Good middle ground between local and paid cloud.

**Fallback chain:**
```
Configured provider (user's choice)
    ↓ on failure
Next available provider (has API key)
    ↓ on failure
Piper TTS (local, always available)
    ↓ on failure
System TTS (OS native, always available)
```

**Incremental TTS:** Don't wait for the full AI response before speaking.
Parse sentence boundaries as text streams from the CLI and generate TTS
per segment. Start speaking the first sentence while the AI is still
generating the rest. This is the single biggest latency improvement.

**AI-controlled voice directives:** The AI can adjust voice parameters
contextually (calmer voice for evening, energetic for morning) using
embedded directives in its responses.

### Wake Word Detection

**Always local. Never cloud.**

- **OpenWakeWord** — lightweight, customizable trigger words. Free.
- **Porcupine** — Picovoice's engine. Free tier available.

Configuration:
```toml
[voice.wake]
engine = "openwakeword"       # "openwakeword" or "porcupine"
words = ["hey assistant", "computer", "claude"]
sensitivity = 0.5             # 0-1, higher = more sensitive (more false positives)
```

Wake word detection runs continuously on the portal device. Only after
detection does the microphone feed go to STT. Passive listening uses
minimal CPU — no audio data is processed or transmitted until triggered.

---

## Multi-Agent System

### Agent Model

One default agent, with the ability to create specialized agents:

```rust
struct Agent {
    id: String,                      // "default", "coder", "researcher"
    name: String,                    // Human-friendly name
    cli_provider: CliProvider,       // Claude or Codex
    system_prompt: Option<String>,   // Appended to CLI system prompt
    tools: ToolPolicy,              // Which tools this agent can use
    default_mode: ConversationMode,
}

enum CliProvider {
    Claude {
        model: Option<String>,       // "opus", "sonnet", "haiku"
        flags: Vec<String>,          // Additional CLI flags
    },
    Codex {
        model: Option<String>,
        approval_mode: String,       // "suggest", "auto-edit", "full-auto"
    },
}
```

### Default Agent

The default agent handles everything unless a specialized agent is invoked.
It uses Claude CLI with a system prompt that describes the home assistant
role, available tools, and user preferences.

### Specialized Agents

Created via configuration, not code:

```toml
[[agents]]
id = "coder"
name = "Code Assistant"
cli_provider = "codex"
system_prompt = "You are a coding assistant. Focus on code quality..."
tools = "coding"                  # Tool profile

[[agents]]
id = "researcher"
name = "Research Assistant"
cli_provider = "claude"
model = "opus"
system_prompt = "You are a deep research assistant..."
tools = "full"
```

### Agent Routing

When a message arrives, the server decides which agent handles it:

1. **Explicit binding** — portal is in a mode that maps to a specific agent
   (e.g., `/coding` mode → coder agent)
2. **Conversation's agent** — conversation was started with a specific agent
3. **Default agent** — catches everything else

No AI-based routing for v1. The user explicitly chooses via mode switching.
AI-based routing ("which agent should handle this?") is a future enhancement
using minimal direct API calls.

---

## Tool System

### Architecture

Every tool is a native Rust module implementing a common trait:

```rust
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;  // JSON Schema for params
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult>;
}
```

Tools are registered at startup based on configuration. No dynamic loading,
no runtime discovery, no plugins.

### Tool Categories

**Always available:**
| Tool | Description |
|------|-------------|
| `exec` | Shell command execution |
| `read` | Read file contents |
| `write` | Write/create files |
| `edit` | Edit existing files |
| `web_search` | Web search |
| `web_fetch` | Fetch and parse URL content |

**Enabled via config:**
| Tool | Description | Requires |
|------|-------------|----------|
| `browser` | Playwright CLI browser automation | `playwright-cli` installed |
| `gmail` | Read/send email via Gmail API | Google API key |
| `calendar` | Google Calendar read/write | Google API key |
| `image_gen` | Image generation (Google Imagen) | Google API key |
| `tts` | Text-to-speech generation | TTS provider configured |
| `cron` | Schedule recurring tasks | — |
| `screenshot` | Capture screen content | — |
| `memory` | Persistent memory store | — |

### Tool Policies

Profiles control which tools each agent can access:

| Profile | Tools |
|---------|-------|
| `minimal` | read, web_search, web_fetch |
| `coding` | minimal + write, edit, exec |
| `full` | All enabled tools |

Single-user system — one global policy with per-agent overrides.
No per-channel, per-group, per-sandbox stacking.

### Browser Automation (Playwright CLI)

The AI gets browser control via Playwright CLI subprocess:

```bash
playwright-cli open https://example.com
playwright-cli click <ref>
playwright-cli fill <ref> "search query"
playwright-cli screenshot
```

Key features:
- Token-efficient CLI commands (not MCP's 70+ tool schemas)
- Persistent named sessions across tool calls
- Network origin filtering (`allowedOrigins` / `blockedOrigins`)
- Headless by default
- Cookie/localStorage/state save and load
- Disabled by default — user must explicitly enable

### Audit Logging

Every tool invocation is logged:
```jsonl
{"ts":"2026-02-08T14:30:00Z","tool":"exec","params":{"command":"ls -la"},"agent":"default","conversation":"general","portal":"discord-123","duration_ms":45,"success":true}
{"ts":"2026-02-08T14:30:01Z","tool":"browser","params":{"action":"goto","args":"https://example.com"},"agent":"default","conversation":"general","portal":"web","duration_ms":1200,"success":true}
```

---

## Integrations

### Gmail

Read and send email through the Gmail API:
- Read inbox, search messages, get message content
- Send/reply to emails (with user confirmation)
- Draft composition
- Label management
- Requires Google API credentials (stored in keychain)

### Google Calendar

Read and manage calendar events:
- List upcoming events
- Create/update/delete events
- Check availability
- Set reminders
- Natural language scheduling ("schedule a meeting with Bob next Tuesday at 2")

### Image Generation

Generate images via Google's Imagen/Nano APIs:
- Text-to-image generation
- Image editing/inpainting
- Requires Google API credentials
- Results delivered as file attachments in portals

### Memory System

Persistent memory that survives across conversations:
- Key-value store for facts, preferences, notes
- Semantic search across stored memories
- The AI can remember things ("remember that I prefer dark mode")
- The AI can recall things ("what did I say about the Johnson project?")
- Stored locally as indexed JSONL

### Cron / Scheduled Tasks

The AI can schedule recurring actions:
- "Check this webpage every morning and tell me if it changes"
- "Remind me every Friday to review my pull requests"
- "Run this command at 3 AM and report the output"
- Stored in config, executed by the server's async runtime
- Results delivered to the portal that created the schedule (or default)

---

## Security Model

### No External Code Execution

- No plugin marketplace
- No extension installation from URLs
- No dynamic code loading
- All integrations are compiled into the binary
- Enable/disable via config — nothing is installed at runtime

### Secrets Management

API keys stored in OS keychain, never in config files:

```
OS Keychain (keyring crate)
    ├── elevenlabs-api-key
    ├── google-api-key
    ├── groq-api-key
    └── discord-bot-token

Config file (config.toml) — non-sensitive only:
    ├── voice settings
    ├── portal configuration
    ├── tool enable/disable flags
    └── agent definitions
```

Resolution order: keychain → env var → not configured.
Env var fallback for containerized deployments where keychain isn't available.

### Web UI Security

- **Localhost only** — gateway binds to `127.0.0.1` by default
- Remote access requires explicit tunnel (SSH, Tailscale, Cloudflare Tunnel)
- **Sentinel-based redaction** — API keys in config never shown in web UI
  (replaced with `__REDACTED__`, restored on write)
- **Session authentication** — web UI requires local authentication
- **No credentials in HTTP responses** — ever

### Network Surface

Minimal outbound connections:
- CLI → AI provider API (Anthropic, OpenAI) — encrypted, managed by CLI
- Optional: ElevenLabs, Groq, Google APIs — only when configured
- Discord bot WebSocket — only when Discord portal is enabled
- No inbound connections from the internet (unless user sets up tunnel)

### Tool Sandboxing

- `exec` tool runs commands with the server process's user permissions
- Browser automation restricted via network origin filtering
- File read/write restricted to configured directories
- No Docker sandbox (unnecessary complexity for single-user system)
- All tool invocations audit-logged

---

## Configuration

### File Structure

```
~/.assistant/
├── config.toml                    # Main configuration (no secrets)
├── conversations/                 # Conversation audit trails
│   ├── general.jsonl
│   └── ...
├── state/
│   ├── portals.json
│   └── conversations.json
├── memory/
│   └── store.jsonl               # Persistent memory
├── logs/
│   ├── server.log
│   └── tools.jsonl               # Tool audit log
└── playwright-cli.json           # Browser automation config
```

### Main Config

```toml
[server]
bind = "127.0.0.1"
port = 3000
log_level = "info"

[cli.claude]
# model = "sonnet"               # Override default model
# Additional flags passed to claude CLI
flags = ["--dangerously-skip-permissions"]

[cli.codex]
approval_mode = "auto-edit"

# --- Portals ---

[portals.discord]
enabled = true
# Bot token stored in keychain as "discord-bot-token"

[portals.voice.kitchen]
enabled = true
device_id = "rpi-kitchen"
wake_words = ["hey assistant", "computer"]
stt_provider = "whisper-local"
tts_provider = "piper"

[portals.voice.office]
enabled = true
device_id = "rpi-office"
wake_words = ["hey assistant", "claude"]
stt_provider = "groq"            # Cloud STT for this room
tts_provider = "elevenlabs"      # Premium TTS for this room

[portals.web]
enabled = true

# --- Voice ---

[voice.stt.whisper]
model = "base.en"
device = "cpu"

[voice.stt.groq]
model = "whisper-large-v3-turbo"

[voice.tts.piper]
voice = "en_US-lessac-medium"
speed = 1.0

[voice.tts.elevenlabs]
voice_id = "pMsXgVXv3BLzUgSXRplE"
model = "eleven_multilingual_v2"
stability = 0.5
speed = 1.0

[voice.wake]
engine = "openwakeword"
sensitivity = 0.5

# --- Tools ---

[tools.browser]
enabled = false                    # Explicitly opt-in
headless = true

[tools.gmail]
enabled = false

[tools.calendar]
enabled = false

[tools.image_gen]
enabled = false
provider = "google"

[tools.cron]
enabled = true

[tools.memory]
enabled = true

# --- Agents ---

[[agents]]
id = "default"
name = "Assistant"
cli_provider = "claude"
tools = "full"

[[agents]]
id = "coder"
name = "Code Assistant"
cli_provider = "codex"
tools = "coding"
system_prompt = "Focus on code quality, testing, and clear documentation."
```

---

## Gateway WebSocket Protocol

### Connection

Portals connect to the server via WebSocket:

```
ws://127.0.0.1:3000/ws?portal_type=discord&portal_id=abc123
```

### Message Types

**Client → Server:**
```jsonl
{"type": "message", "text": "What's the weather?", "conversation_id": "general"}
{"type": "switch", "conversation_id": "coding-myproject"}
{"type": "command", "command": "/coding", "args": "myproject"}
{"type": "audio", "format": "pcm_16000", "data": "<base64>"}
```

**Server → Client:**
```jsonl
{"type": "text_chunk", "text": "The weather in", "conversation_id": "general"}
{"type": "text_chunk", "text": " Seattle is", "conversation_id": "general"}
{"type": "text_done", "full_text": "The weather in Seattle is...", "conversation_id": "general"}
{"type": "tool_start", "tool": "web_search", "params": {...}}
{"type": "tool_result", "tool": "web_search", "result": "..."}
{"type": "audio_chunk", "format": "pcm_16000", "data": "<base64>"}
{"type": "portal_update", "portals": [...]}
{"type": "error", "message": "CLI session expired", "code": "auth_expired"}
```

### Streaming

The server streams AI responses token-by-token via WebSocket. Each portal
decides how to handle the stream:
- **Text portals** (Discord, web): accumulate and display
- **Voice portals**: extract sentence boundaries and generate incremental TTS

---

## Data Flow Diagrams

### Text Message (Discord)

```
Discord user types message
    ↓
serenity bot receives event
    ↓
Resolve portal → conversation mapping
    ↓
Append to audit trail (JSONL)
    ↓
Spawn/resume CLI subprocess with session ID
    CLI: claude -p "user message" --session-id abc --output-format json
    ↓
Stream response tokens
    ↓
Broadcast to all portals attached to this conversation
    ├── Discord portal: send as Discord message (chunked if needed)
    ├── Voice portal (if any): incremental TTS → speaker
    └── Web portal (if any): WebSocket text chunks
    ↓
Append response to audit trail
```

### Voice Message (Room Portal)

```
Room microphone listening passively
    ↓
OpenWakeWord detects "hey assistant"
    ↓
Switch to LISTENING state
    ↓
Capture audio → local Whisper.cpp STT
    ↓
Transcript: "What time is my next meeting?"
    ↓
Send transcript to server via WebSocket
    ↓
Server resolves portal → conversation (General)
    ↓
Append to audit trail
    ↓
CLI subprocess: claude -p "What time is my next meeting?" ...
    ↓
AI calls calendar tool → retrieves next meeting
    ↓
Response: "Your next meeting is at 2 PM with the design team"
    ↓
Server streams response to all portals in General conversation
    ├── Kitchen portal: Piper TTS → "Your next meeting is at 2 PM..."
    ├── Discord (if watching): text message
    └── Web UI: text display
    ↓
Kitchen speaker plays audio
    ↓
Return to IDLE state
```

---

## Operational Considerations

### Process Management

The server manages multiple long-lived processes:
- One or more CLI subprocesses (one per active conversation)
- WebSocket connections to portals
- Background task scheduler (cron)
- Optional: Playwright CLI browser sessions

CLI subprocesses are pooled — idle conversations can have their CLI process
suspended and resumed later with session ID. Active conversation limit
configurable (default: 5 concurrent CLI processes).

### Resource Management

- **Memory**: CLI subprocesses are the main consumers. Each `claude` process
  uses ~100-200MB. Cap concurrent processes to limit total memory.
- **CPU**: STT (Whisper) is CPU-intensive. Schedule heavy STT on the server,
  not on Pi portals, if hardware is limited.
- **Disk**: JSONL audit trails grow over time. Configurable rotation/archival.
- **Network**: Minimal — only CLI↔API traffic and portal WebSocket connections.

### Monitoring

- Server health endpoint: `GET /health`
- Portal connection status in web UI
- CLI process status (running, suspended, errored)
- Conversation activity metrics
- Tool invocation counts
- Error rates and types

### Graceful Shutdown

On SIGTERM:
1. Stop accepting new messages
2. Finish in-progress CLI responses (with timeout)
3. Flush all audit trail buffers
4. Close WebSocket connections (with "server shutting down" message)
5. Save conversation state
6. Exit

### Startup

1. Load config
2. Verify CLI installations (`claude --version`, `codex --version`)
3. Verify CLI credentials are valid
4. Start WebSocket server
5. Connect Discord bot (if enabled)
6. Resume active conversations from saved state
7. Start cron scheduler
8. Announce readiness to all connected portals

---

## Future Possibilities

These are NOT in scope for initial versions but represent natural growth:

- **Smart home integration** — control lights, locks, thermostats via tools
- **AI-based agent routing** — classify messages and auto-route to the best agent
- **Multi-user support** — family members with separate identities and permissions
- **Mobile app** — native iOS/Android app as a portal (direct ElevenLabs TTS)
- **Telegram/WhatsApp portals** — additional messaging platforms
- **Local LLM fallback** — Ollama/llama.cpp for when cloud is unavailable
- **Email digest** — daily summary of emails, calendar, and tasks
- **Voice identification** — know which family member is speaking
- **Proactive notifications** — AI initiates messages ("your package was delivered")
- **Plugin system** (carefully) — if demand exists, a sandboxed, reviewed plugin
  system with strict capabilities. But not until the core is rock-solid.
