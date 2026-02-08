# Claude Code CLI Wrapper — Patterns & Implementation Details

## Summary

OpenClaw wraps the `claude` CLI as a subprocess, passing messages as arguments
and parsing JSON responses. This delegates all context management, auth, and
token optimization to Anthropic's infrastructure. For our project, this is the
**preferred path for long-running conversations**.

---

## Command Invocation

### New Session
```bash
claude -p \
  --output-format json \
  --dangerously-skip-permissions \
  --model opus \
  --append-system-prompt "<system-prompt>" \
  --session-id "<uuid>" \
  "User's message here"
```

### Resume Session
```bash
claude -p \
  --output-format json \
  --dangerously-skip-permissions \
  --resume "<session-id>" \
  "Follow-up message"
```

### Flag Reference

| Flag | Purpose |
|------|---------|
| `-p` | Pipe/passive mode — no interactive UI |
| `--output-format json` | Structured JSON output |
| `--dangerously-skip-permissions` | Skip tool permission prompts |
| `--model <id>` | Model selection (opus, sonnet, haiku) |
| `--session-id <uuid>` | Session continuity (new sessions) |
| `--resume <uuid>` | Resume existing session |
| `--append-system-prompt <text>` | Inject system prompt |
| `--image <path>` | Attach image (repeatable) |

---

## Input Handling

### Three Input Modes

1. **Argument mode** (default): prompt is the last CLI argument
2. **Stdin mode**: prompt written to stdin when it exceeds `maxPromptArgChars`
   - Stdin is write-closed after transmission
3. **Image attachment**: images written to temp directory, passed via `--image` flag
   - Temp dir: `os.tmpdir()/openclaw-cli-images-*`
   - Files named sequentially: `image-1.png`, `image-2.jpg`, etc.
   - Permissions: `0o600` (owner read/write only)
   - Cleaned up in `finally` block (even on error)

---

## Output Parsing

### JSON Response Structure
```json
{
  "session_id": "uuid",
  "message": "Response text (primary field)",
  "content": "Response text (fallback 1)",
  "result": "Response text (fallback 2)",
  "usage": {
    "input_tokens": 1500,
    "output_tokens": 300,
    "cache_read_input_tokens": 800,
    "cache_write_input_tokens": 200,
    "total_tokens": 1800
  }
}
```

Text extraction searches fields in order: `message` → `content` → `result` → root object.

Session ID fields searched: `session_id`, `sessionId`, `conversation_id`, `conversationId`.

**Fallback**: if JSON parsing fails but output is non-empty, returns raw text.

---

## Session Management

### Session Mode: `"always"`
- Always passes a session ID
- Generates new UUID via `crypto.randomUUID()` if none exists
- Stores session ID per provider in the OpenClaw session entry

### New vs. Resume Decision
```
IF cliSessionId exists AND backend has resumeArgs:
  → Resume mode (use resumeArgs, skip model/system-prompt/session args)
ELSE:
  → New session (use base args, add all flags)
```

### Critical Pattern: Resume strips everything
When resuming, the CLI receives ONLY:
- The resume args template (with `{sessionId}` substituted)
- The user's message

No model flag, no system prompt, no session-id flag. The CLI manages
all of that internally from its own session state.

---

## Environment Isolation

### Why ANTHROPIC_API_KEY is cleared

```typescript
clearEnv: ["ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_OLD"]
```

This is critical. Without clearing:
- The Claude CLI would detect the env var and use it for direct API calls
- This bypasses the CLI's own subscription-based auth
- The user would be billed at API rates instead of subscription rates

By clearing the key, the CLI falls back to its own OAuth credentials
(from `~/.claude/.credentials.json` or macOS keychain).

---

## Model Aliases

OpenClaw normalizes model names before passing to the CLI:

| User Specifies | CLI Receives |
|---------------|-------------|
| `opus`, `opus-4.6`, `opus-4.5`, `opus-4`, `claude-opus-*` | `opus` |
| `sonnet`, `sonnet-4.5`, `sonnet-4.1`, `sonnet-4.0`, `claude-sonnet-*` | `sonnet` |
| `haiku`, `haiku-3.5`, `claude-haiku-3-5` | `haiku` |

Case-insensitive lookup. Exact match takes precedence over lowercase match.

---

## System Prompt Injection

### When system prompts are sent
- Default: `systemPromptWhen: "first"` — only on new sessions
- Resume sessions do NOT re-send the system prompt
- The CLI preserves the system prompt from the initial session

### What the system prompt includes
- Docs path reference
- Runtime info (OS, Node version, shell, model)
- User timezone and time format
- Bootstrap context files
- Model alias reference
- TTS hints

---

## Process Management

### Spawning
- Uses Node.js `child_process.spawn()`
- Working directory: agent's workspace dir
- Stdin: inherited (TTY support)
- Stdout/stderr: piped and accumulated

### Timeout
- Configurable `timeoutMs`
- On timeout: `child.kill("SIGKILL")`
- Settled flag prevents double-resolution of the promise

### Serialization
- All runs for the same provider queue sequentially by default
- Global queue: `CLI_RUN_QUEUE: Map<string, Promise<unknown>>`
- Prevents concurrent modifications to shared provider state
- Can be disabled per-backend: `serialize: false`

### Suspended Process Cleanup (Unix/Linux only)
- Before each run, scans for stopped (`T` status) claude processes
- Uses `ps -ax -o pid=,stat=,command=` to find them
- If count exceeds threshold (default: 10), kills with SIGKILL
- Also runs `pkill -f` for resume processes matching the session pattern

---

## Error Handling

### Classification
Non-zero exit codes are classified into failover reasons:
- `"auth"` → status 401
- `"billing"` → status 402
- `"rate_limit"` → status 429
- `"timeout"` → status 408
- `"format"` → status 400
- `"unknown"` → unclassified

Errors are thrown as `FailoverError` objects which trigger the provider
failover system (try next model/profile in the chain).

---

## Configuration Override

Users can customize the Claude CLI backend:

```json5
{
  "agents": {
    "defaults": {
      "cliBackends": {
        "claude-cli": {
          "command": "/usr/local/bin/claude",
          "clearEnv": ["ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_OLD", "MY_KEY"],
          "serialize": false,
          "maxPromptArgChars": 5000
        }
      }
    }
  }
}
```

---

## Takeaways for Our Project

### What to adopt
- **`-p --output-format json` is the core pattern** — pipe mode + JSON output
  gives us structured, parseable responses from the CLI.
- **Session ID management** — pass `--session-id` on first message, `--resume`
  on subsequent messages. The CLI handles all context internally.
- **Clear ANTHROPIC_API_KEY** — essential to ensure subscription-rate billing.
- **Model aliases** — normalize user-friendly names to CLI-expected values.
- **Sequential execution per provider** — prevents race conditions.

### What to simplify
- **System prompt injection** — for a home assistant, we'll have a fixed system
  prompt. No need for per-session runtime info assembly.
- **Image handling** — write to temp, pass via `--image`, clean up. Simple.
- **Process cleanup** — important on long-running servers. In Rust, use
  `tokio::process::Command` with proper signal handling.

### What to watch out for
- **`--dangerously-skip-permissions`** — OpenClaw uses this for unattended
  operation. For our secure home assistant, we should carefully consider which
  tools the CLI is allowed to use. We may want to configure Claude Code's
  permission system rather than bypassing it entirely.
- **Resume mode is all-or-nothing** — you can't change the model or system
  prompt mid-session. Plan session architecture accordingly.
- **Timeout management** — CLI calls can take a long time for complex tool
  chains. Set generous timeouts but have cancellation support.
