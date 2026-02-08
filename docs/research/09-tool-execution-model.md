# Tool Execution Model — How AI Agents Get Capabilities

## Summary

OpenClaw gives AI agents tools via a multi-layered system: core built-in tools,
channel-specific tools from plugins, and a policy framework that controls which
tools each agent can access. Tools are standard function-calling definitions
(JSON Schema) adapted per-provider.

The tool permission model is the most complex part — profiles, groups, per-agent
overrides, sandbox restrictions, and owner-only gates. For our project, we want
the capabilities without the complexity.

---

## Tool Architecture Overview

```
Tool Registration
    ├── Core Tools (built-in)
    │     ├── Browser automation
    │     ├── Shell execution (exec, process)
    │     ├── File operations (read, write, edit)
    │     ├── Web (search, fetch)
    │     ├── Messaging (send to channels)
    │     ├── Sessions (list, spawn, history)
    │     ├── Image processing
    │     ├── Memory (search, get)
    │     ├── Cron (scheduled tasks)
    │     └── TTS (text-to-speech)
    │
    ├── Channel Tools (from plugins)
    │     ├── Discord reactions/threads
    │     ├── WhatsApp login
    │     ├── Slack thread management
    │     └── Platform-specific actions
    │
    └── Extension Tools (from plugins)
          └── Resolved at runtime via plugin registry
```

---

## Tool Execution Flow

```
1. AI model requests tool call
     ↓
2. PI Agent Core selects matching tool definition
     ↓
3. Before-tool-call hook runs (can block or modify params)
     ↓
4. Tool policy check (is this tool allowed for this agent/session?)
     ↓
5. tool.execute(toolCallId, params, signal, onUpdate) invoked
     ↓
6. Tool runs (may emit async progress updates)
     ↓
7. Result returned to agent context
     ↓
8. Result guard checks (prevents oversized results from corrupting session)
```

---

## Built-In Tool Catalog

### Core Tools (Always Available Unless Blocked)

| Tool | Description |
|------|-------------|
| `browser` | Web page automation, screenshots |
| `exec` | Shell command execution (with PTY/elevated options) |
| `process` | Background task execution (configurable timeout/cleanup) |
| `read` | File reading |
| `write` | File writing |
| `edit` | File editing (text replacement) |
| `apply_patch` | Apply code patches |
| `web_search` | Web search |
| `web_fetch` | Fetch URL content |
| `message` | Send messages to channels/groups |
| `sessions` | List, spawn, send to, check history/status of sessions |
| `image` | Image processing/analysis |
| `memory_search` | Search memory store |
| `memory_get` | Retrieve memory entries |
| `cron` | Schedule recurring tasks |
| `canvas` | UI rendering/drawing |
| `tts` | Text-to-speech generation |
| `agents_list` | List available agents |
| `gateway` | Manage gateway/agent settings |
| `nodes` | Execute commands on connected OpenClaw nodes |

### Sandboxed Variants

When running in a sandbox (Docker container), file operations get wrapped:
- `createSandboxedReadTool()` — enforces workspace root, prevents path escaping
- `createSandboxedWriteTool()` — workspace access control (rw/ro)
- Read-only workspace blocks write/edit/apply_patch entirely

---

## Tool Permission Model

### Policy Framework

Tools are controlled by a layered policy system:

```typescript
type SandboxToolPolicy = {
  allow?: string[];    // Explicit allowlist
  deny?: string[];     // Explicit denylist
  profile?: string;    // Named profile
  alsoAllow?: string[]; // Additive to profile
}
```

### Tool Profiles

| Profile | Includes |
|---------|----------|
| `minimal` | read, web_search, web_fetch |
| `coding` | minimal + write, edit, exec, process |
| `messaging` | coding + message, sessions |
| `full` | All tools |

### Tool Groups

Related tools are grouped for policy management:
- `group:fs` — read, write, edit, apply_patch
- `group:runtime` — exec, process
- `group:web` — web_search, web_fetch, browser
- `group:messaging` — message, sessions
- `group:openclaw` — gateway, agents, cron, nodes

### Policy Resolution Hierarchy

```
1. Tool profile policy (if profile specified)
     ↓
2. Agent-specific policy (agents.<agentId>.tools)
     ↓
3. Group-level policy (channel/messaging group restrictions)
     ↓
4. Sandbox policy (workspace rw/ro, container isolation)
     ↓
5. Subagent policy (restricted set for spawned agents)
     ↓
6. Owner-only policy (e.g., whatsapp_login requires admin)
```

---

## Tool Schema Handling

### Definition Format

Tools use JSON Schema for parameter definitions, built with TypeBox:

```typescript
const ExecToolSchema = Type.Object({
  command: Type.String({ description: "Shell command to execute" }),
  timeout: Type.Optional(Type.Number({ description: "Timeout in ms" })),
  cwd: Type.Optional(Type.String({ description: "Working directory" })),
});
```

### Provider-Specific Schema Adaptation

Different AI providers have different schema requirements:

| Provider | Adaptations |
|----------|-------------|
| **Anthropic (Claude)** | Supports full JSON Schema. Param aliases for Claude Code conventions (`file_path` ↔ `path`) |
| **OpenAI** | Standard function calling format |
| **Google (Gemini)** | Strict compliance — strips `patternProperties`, `additionalProperties`, `minLength`, `maxLength`, `pattern`, `format`, `anyOf`, `oneOf` |

Schema cleaning happens at tool registration, not at call time.

---

## Channel-Specific Tools

Channel plugins provide tools via the `agentTools` field:

```typescript
// Plugin contract
ChannelPlugin {
  agentTools?: ChannelAgentToolFactory | ChannelAgentTool[];
}

// Factory pattern — tools resolved at runtime with current config
type ChannelAgentToolFactory = (params: { cfg?: OpenClawConfig }) => ChannelAgentTool[];
```

Example: Discord provides reaction tools, WhatsApp provides login tools,
Slack provides thread management tools.

Channel tools are aggregated by `listChannelAgentTools()` at agent initialization.

---

## Tool Invocation via Gateway HTTP

External systems can invoke tools directly:

```
POST /tools/invoke
Headers:
  x-openclaw-message-channel: discord
  x-openclaw-account-id: bot-123
  Authorization: Bearer <token>

Body:
{
  "tool": "exec",
  "args": { "command": "ls -la" },
  "sessionKey": "agent:default:main",
  "dryRun": false
}
```

Policy resolution runs before execution — the same permission model applies.

---

## Hooks & Interception

### Before-Tool-Call Hook

Runs before every tool execution:
- Can block the tool call (return deny)
- Can modify parameters
- Registered via plugin system
- Used for: rate limiting, parameter sanitization, audit logging

### Tool Result Guards

Wraps session manager to protect against:
- Oversized results (prevents context corruption)
- Malformed results (ensures valid JSON)
- Session state inconsistency

---

## Browser Automation

The browser tool deserves special mention:

- Uses **Docker container** for sandboxed browser execution
- Three proxy modes: sandbox, host, or connected OpenClaw nodes
- Policy control: `gateway.nodes.browser.mode` = auto/manual/off
- Screenshots, page interaction, form filling
- **Not using Playwright MCP** — custom implementation

---

## Takeaways for Our Project

### What to adopt

- **Tool profiles** — `minimal`, `coding`, `full` presets are a clean UX pattern.
  Users shouldn't have to configure individual tool permissions.
- **JSON Schema for tool definitions** — standard function-calling format works
  with all providers. Use TypeBox or equivalent for type-safe schema generation.
- **Tool groups** — logical grouping (`fs`, `runtime`, `web`) simplifies policy.
- **Before-tool-call hooks** — essential for audit logging and rate limiting.
  Every tool invocation should be logged.
- **Result size guards** — prevent a single tool result from blowing up the
  context window. Simple max-length check.

### What to simplify

- **No plugin-provided tools** — all tools are native, built into the codebase.
  No runtime tool discovery or dynamic loading. This eliminates the entire
  `ChannelAgentToolFactory` pattern and the associated security surface.
- **No per-agent, per-channel, per-group policy stacking** — single-user system.
  One global tool policy with overrides per conversation mode.
- **No sandbox complexity** — tools run on the home server with the user's
  permissions. Docker sandboxing adds complexity we don't need for a
  single-user system.
- **No provider-specific schema cleaning** — target Claude and Codex CLIs
  primarily. They handle schema internally. Only need schema adaptation for
  direct API calls (minimal use case).

### What to add

- **Playwright MCP or CLI** — browser automation as a native tool. Microsoft's
  Playwright MCP server provides 70+ browser tools using accessibility tree
  snapshots (no vision model needed). Available as `@playwright/mcp` npm
  package. Consider CLI wrapper for token efficiency.
- **Native integration tools** — Gmail, Google Drive, image generation (Google
  Nano/Banana), calendar. Each is a Rust module, not a plugin. Enabled/disabled
  via config. Open source, auditable.
- **Groq Whisper tool** — fast cloud STT as an alternative to local Whisper.
  Groq's inference hardware provides very high throughput for Whisper models.
- **Local LLM routing** — tool that routes specific tasks to local models
  (Ollama, llama.cpp) when hardware supports it. Classification, summarization,
  and STT are good candidates for local inference.

### Tool architecture sketch for our system

```rust
/// Every tool implements this trait
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;  // JSON Schema
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult>;
}

/// Context provided to every tool invocation
struct ToolContext {
    conversation_id: Uuid,
    portal_source: PortalId,
    user_permissions: ToolPermissions,
    abort: CancellationToken,  // tokio_util
}

/// Result with size guard
struct ToolResult {
    content: String,          // Max 100KB, truncated if larger
    artifacts: Vec<Artifact>, // Files, images, etc.
}

/// Built-in tool registry — no dynamic loading
struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    fn new(config: &Config) -> Self {
        let mut tools = HashMap::new();

        // Always available
        tools.insert("exec", Arc::new(ExecTool::new()));
        tools.insert("read", Arc::new(ReadTool::new()));
        tools.insert("write", Arc::new(WriteTool::new()));
        tools.insert("web_search", Arc::new(WebSearchTool::new()));
        tools.insert("web_fetch", Arc::new(WebFetchTool::new()));

        // Enabled via config
        if config.integrations.gmail.enabled {
            tools.insert("gmail", Arc::new(GmailTool::new(&config)));
        }
        if config.integrations.image_gen.enabled {
            tools.insert("image_gen", Arc::new(ImageGenTool::new(&config)));
        }
        if config.integrations.playwright.enabled {
            tools.insert("browser", Arc::new(PlaywrightTool::new(&config)));
        }

        Self { tools }
    }
}
```
