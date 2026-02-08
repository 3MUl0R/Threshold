# API Key & Secrets Management — Patterns for Secure Storage

## Summary

OpenClaw manages third-party API keys through a layered system: config files,
environment variables, auth profile stores, and OS keychain. The gateway web
UI uses a sentinel-based redaction system to prevent credential leakage.

Keys are **not encrypted at rest** — security relies on filesystem permissions
and OS-level protections. This is the main area where our project should
improve.

---

## Storage Locations

### 1. Config File (Primary)

**Location**: `~/.openclaw/config.json5`

Keys live in typed config sections:

```json5
{
  "messages": {
    "tts": {
      "elevenlabs": { "apiKey": "..." },
      "openai": { "apiKey": "..." }
    }
  },
  "models": {
    "providers": {
      "openai": { "apiKey": "...", "baseUrl": "..." },
      "groq": { "apiKey": "..." },
      "google": { "apiKey": "..." }
    }
  },
  "channels": {
    "discord": { "accounts": [{ "token": "..." }] },
    "telegram": { "accounts": [{ "botToken": "...", "webhookSecret": "..." }] },
    "slack": { "accounts": [{ "botToken": "...", "appToken": "...", "signingSecret": "..." }] }
  }
}
```

**Format**: JSON5 (supports comments, trailing commas).
**Validation**: Zod schemas validate structure at load time.

### 2. Environment Variables (Fallback)

| Service | Env Var(s) |
|---------|-----------|
| ElevenLabs | `ELEVENLABS_API_KEY`, `XI_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Google/Gemini | `GEMINI_API_KEY` |
| Anthropic | `ANTHROPIC_API_KEY`, `ANTHROPIC_OAUTH_TOKEN` |

Env vars are checked as fallbacks when config file keys are empty.

### 3. Auth Profile Store (Per-Agent)

**Location**: `~/.openclaw/agents/<agent-id>/auth-profiles.json`

Structured profiles supporting multiple credential types:
- `type: "api_key"` — plain API key
- `type: "token"` — bearer token with optional expiry
- `type: "oauth"` — access + refresh tokens with `expires_at`

Used for providers that support multiple auth modes (Anthropic, OpenAI).

### 4. OS Keychain (macOS Only)

CLI credentials (Claude Code, Codex) stored in macOS Keychain:
```
security find-generic-password -s "Claude Code-credentials"
```

Falls back to `~/.claude/.credentials.json` if keychain unavailable.
**Only used for CLI credential borrowing**, not for third-party API keys.

---

## Key Resolution Priority

When resolving a key for a provider at runtime:

```
1. Explicit profile ID (user/session specified)
   ↓
2. Config-level auth override (models.providers.<provider>.auth)
   ↓
3. Auth profile store (ordered list)
   ↓
4. Environment variables
   ↓
5. Config-embedded API key (models.providers.<provider>.apiKey)
   ↓
6. AWS SDK default chain (Bedrock only)
```

For TTS specifically, simpler resolution:
```
Config key → ELEVENLABS_API_KEY → XI_API_KEY → undefined (skip provider)
```

---

## Gateway Web UI — Credential Handling

### Sentinel-Based Redaction

The gateway UI never shows raw credentials. Instead:

1. **On read**: All sensitive fields replaced with `__OPENCLAW_REDACTED__`
2. **On display**: Sensitive fields rendered as password inputs (masked)
3. **On write**: Sentinel values restored to originals before disk write

**Sensitive field detection** — pattern-based regex matching:
- Fields matching `/token/i`, `/password/i`, `/secret/i`, `/api.?key/i`
- Works for new fields without code changes

### Round-Trip Safety

```
User requests config → Server sends redacted snapshot
User edits (leaves redacted fields alone) → Server submits
Server restores originals for unchanged sentinels → Writes to disk
```

This prevents:
- Credential leakage through HTTP responses
- Accidental overwrite of credentials with sentinel values
- Race conditions (config hash validation)

---

## Provider-Specific Key Handling

| Provider | Config Path | Env Var | Auth Modes | Notes |
|----------|-------------|---------|------------|-------|
| ElevenLabs | `messages.tts.elevenlabs.apiKey` | `ELEVENLABS_API_KEY`, `XI_API_KEY` | api-key | Custom baseUrl support |
| OpenAI TTS | `messages.tts.openai.apiKey` | `OPENAI_API_KEY` | api-key | Custom endpoint via `OPENAI_TTS_BASE_URL` |
| OpenAI (models) | `models.providers.openai.apiKey` | `OPENAI_API_KEY` | oauth, api-key | OAuth profiles supported |
| Groq | `models.providers.groq.apiKey` | `GROQ_API_KEY` | api-key | No profile support |
| Google (Gemini) | `models.providers.google.apiKey` | `GEMINI_API_KEY` | api-key | gcloud ADC fallback |
| Anthropic | Auth profiles or CLI | `ANTHROPIC_API_KEY` | oauth, api-key | CLI credential borrowing preferred |
| Discord | `channels.discord.accounts[].token` | — | stored | Per-account bot token |
| Telegram | `channels.telegram.accounts[].botToken` | — | stored | Supports `tokenFile` path |
| Slack | `channels.slack.accounts[].botToken` | — | stored | Multiple token types |

---

## Security Properties

### What works well

- **Pattern-based sensitive field detection** — new fields auto-detected
- **Sentinel redaction** — prevents credential leakage through web UI
- **Config hash validation** — detects concurrent modifications
- **Keychain integration** — better than plaintext for macOS CLI creds
- **Env var fallback** — supports 12-factor app patterns
- **Multiple auth modes** — flexible for different provider requirements

### Limitations

- **No encryption at rest** — plain JSON5 on disk
- **No credential validation at config time** — invalid keys only fail at runtime
- **Filesystem permission dependent** — security relies on OS file permissions
- **No audit logging** — no record of credential access
- **Env vars visible to child processes** — spawned subprocesses inherit env

---

## Takeaways for Our Project

### What to adopt

- **Layered key resolution** — config file → env var → fallback. Standard pattern,
  easy for users to understand.
- **Sentinel redaction in web UI** — simple, effective. Prevents the most common
  credential leak vector.
- **Pattern-based sensitive field detection** — future-proof without code changes.
- **Per-provider config sections** — clean separation, easy to add new providers.

### What to improve

- **Encrypt at rest** — use the OS keychain for ALL API keys, not just CLI tokens.
  Rust has excellent keychain crates (`keyring`, `secret-service`). Plain JSON config
  should never contain secrets.
- **Validate keys on entry** — make a test API call when a key is first configured.
  Fail fast instead of at runtime.
- **Audit logging** — record when credentials are accessed, from which portal, for
  which operation. Append-only JSONL, same pattern as session transcripts.
- **Separate secrets file** — keep non-sensitive config in `config.toml`, secrets in
  keychain or a separate encrypted store. Don't mix configuration and credentials.
- **Env var isolation** — clear third-party API keys from env when spawning CLI
  subprocesses (same principle as clearing `ANTHROPIC_API_KEY`). Prevent key leakage
  to subprocess environments.

### Rust implementation sketch

```rust
use keyring::Entry;

struct SecretStore {
    service_name: String,  // "our-assistant"
}

impl SecretStore {
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let entry = Entry::new(&self.service_name, key)?;
        entry.set_password(value)?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        let entry = Entry::new(&self.service_name, key)?;
        match entry.get_password() {
            Ok(val) => Ok(Some(val)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

// Resolution: keychain → env var → None
fn resolve_api_key(store: &SecretStore, key: &str, env_var: &str) -> Option<String> {
    store.get(key).ok().flatten()
        .or_else(|| std::env::var(env_var).ok())
}
```
