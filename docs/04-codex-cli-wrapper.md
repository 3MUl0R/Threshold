# Codex CLI Wrapper — Patterns & Implementation Details

## Summary

OpenClaw wraps the OpenAI `codex` CLI similarly to how it wraps Claude Code,
but with notable differences in output format (JSONL streaming vs JSON),
session semantics (`thread_id` vs `session_id`), and resume behavior.
The Codex wrapper uses a subcommand pattern (`codex exec`) rather than flags.

---

## Command Invocation

### New Session
```bash
codex exec \
  --json \
  --color never \
  --sandbox read-only \
  --skip-git-repo-check \
  --model gpt-5.3-codex \
  "User's message here"
```

### Resume Session
```bash
codex exec resume <thread-id> \
  --color never \
  --sandbox read-only \
  --skip-git-repo-check
```

### Flag Reference

| Flag | Purpose |
|------|---------|
| `exec` | Execute subcommand |
| `resume <thread-id>` | Resume a previous thread (positional arg) |
| `--json` | JSONL streaming output |
| `--color never` | Disable ANSI color codes |
| `--sandbox read-only` | Restrict filesystem access |
| `--skip-git-repo-check` | Skip git validation |
| `--model <id>` | Model selection |
| `--image <path>` | Image attachment (repeatable) |

---

## Key Differences from Claude CLI

| Aspect | Claude CLI | Codex CLI |
|--------|-----------|-----------|
| **Base command** | `claude -p` | `codex exec` |
| **Output format** | Single JSON object | JSONL stream |
| **Resume output** | JSON | Plain text |
| **Session field** | `session_id` | `thread_id` |
| **Session mode** | `always` (auto-generate) | `existing` (only reuse) |
| **Resume syntax** | `--resume <id>` flag | `exec resume <id>` subcommand |
| **Env cleanup** | Clears `ANTHROPIC_API_KEY` | No env cleanup needed |
| **System prompt** | `--append-system-prompt` | Via stdin/arg (standard) |
| **Image support** | `--image` flag | `--image` flag (identical) |

---

## Output Parsing

### Initial Run (JSONL)
Each line is an independent JSON object:
```json
{"item": {"type": "message", "text": "Hello"}, "thread_id": "thread-123", "usage": {...}}
{"item": {"type": "message", "text": " world"}}
```

Parsing logic:
1. Split stdout by newlines
2. Parse each line as JSON
3. Filter for entries where `item.type` contains `"message"`
4. Concatenate `item.text` fields with newlines
5. Extract `thread_id` from first entry that has it
6. Collect usage data if present

### Resume Run (Plain Text)
- Raw stdout returned as-is
- No session ID extraction (keeps existing thread_id)
- No structured parsing

This asymmetry is important — resumed sessions lose structured output metadata.

---

## Session Management

### Session Mode: `"existing"`
Unlike Claude CLI's `"always"` mode, Codex only reuses existing session IDs.
- If no `cliSessionId` exists → no session arg passed, Codex creates new thread
- Thread ID extracted from first JSONL response → stored for next turn
- Next turn uses `exec resume <thread-id>`

### Resume is a subcommand, not a flag
```
Claude:  claude --resume <id> "message"
Codex:   codex exec resume <id> "message"
```

The `{sessionId}` placeholder in `resumeArgs` is replaced before execution:
```typescript
resumeArgs: ["exec", "resume", "{sessionId}", "--color", "never", "--sandbox", "read-only", ...]
// becomes:  ["exec", "resume", "thread-abc123", "--color", "never", "--sandbox", "read-only", ...]
```

---

## Authentication

### Credential Storage

**macOS Keychain** (primary on Darwin):
- Service: `"Codex Auth"`
- Account: `"cli|<sha256-hash-of-codex-home>"`
- Contents: `{ "tokens": { "access_token": "...", "refresh_token": "..." }, "last_refresh": "..." }`

**Filesystem** (fallback):
- Path: `~/.codex/auth.json` (override via `CODEX_HOME` env var)
- Contents: `{ "tokens": { "access_token": "...", "refresh_token": "..." }, "account_id": "..." }`

### Token Expiry Heuristic
Codex doesn't store an explicit expiry timestamp. OpenClaw uses:
- Keychain: `last_refresh` timestamp + 1 hour
- File: file modification time (`mtime`) + 1 hour

This is a **heuristic** — the actual token lifetime may differ.

### No Env Var Clearing Needed
Unlike Claude CLI, there's no `OPENAI_API_KEY` clearing. Codex CLI uses
its own auth mechanism (OAuth via `~/.codex/auth.json`) regardless of
environment variables.

---

## Usage Monitoring

OpenClaw can fetch Codex subscription usage:
- Endpoint: `https://chatgpt.com/backend-api/wham/usage`
- Auth: `Authorization: Bearer <access_token>`
- Multi-account: `ChatGPT-Account-Id: <account_id>` header
- Returns: rate limits, plan type, credits balance

---

## Model Selection

Default model: `openai-codex/gpt-5.3-codex`

No model aliases configured by default (unlike Claude's opus/sonnet/haiku mapping).
Users can add aliases via config:
```json5
{
  "cliBackends": {
    "codex-cli": {
      "modelAliases": {
        "gpt-5": "gpt-5.3-codex"
      }
    }
  }
}
```

---

## Process Management

### Serialization
Same as Claude CLI — sequential execution per provider by default.
All Codex runs queue behind each other to prevent concurrent session conflicts.

### Suspended Process Cleanup (Unix/Linux only)
- Kills suspended (`T` status) `codex exec resume` processes
- Pattern-matched via `ps -ax` output
- Threshold: >10 suspended processes triggers cleanup
- Uses `pkill -f` for targeted process termination

### Not supported on Windows
Process cleanup logic is skipped entirely on Windows.

---

## Takeaways for Our Project

### What to adopt
- **Subcommand pattern** — `exec` + `resume` is clean and explicit.
- **JSONL streaming** — better for real-time UI updates than waiting for
  complete JSON response.
- **Thread-based sessions** — simple mental model, thread_id from first response.
- **Sandbox mode** — `--sandbox read-only` is a good security default.
  Consider what sandbox level is appropriate for home assistant tasks.

### What to simplify
- **Token expiry heuristic** — the mtime + 1 hour approach is fragile.
  Better to just try the token and handle 401 responses gracefully.
- **Account ID handling** — unless we need multi-account support (unlikely
  for a personal home assistant), skip this complexity.

### What to watch out for
- **Resume drops structured output** — plain text on resume means we lose
  usage tracking mid-conversation. Design UI accordingly.
- **No explicit session creation** — Codex mode is `"existing"` only,
  meaning the first message creates a thread implicitly and we grab
  the thread_id from the response.
- **Auth is OAuth-only** — no API key fallback like Anthropic. User must
  have authenticated via `codex` CLI first.
