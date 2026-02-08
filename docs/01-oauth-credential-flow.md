# OAuth Credential Flow — Patterns & Architecture

## Summary

OpenClaw achieves "use your existing Anthropic subscription" by borrowing OAuth tokens
from the Claude Code CLI's credential store. This avoids requiring users to purchase
separate API keys. The same pattern is applied to Codex CLI (OpenAI) credentials.

The key insight: the CLIs handle the OAuth dance and token refresh, and OpenClaw reads
the resulting tokens from well-known filesystem locations to make direct API calls.

---

## Credential Sources (Priority Order)

When resolving Anthropic credentials, OpenClaw checks in this order:

1. **macOS Keychain** (Darwin only)
   - Service: `"Claude Code-credentials"`, Account: `"Claude Code"`
   - Contains JSON with `claudeAiOauth` object
   - Queried via `security find-generic-password` with a 5-second timeout

2. **Claude CLI Credentials File** (`~/.claude/.credentials.json`)
   - Filesystem fallback when keychain is unavailable or on Linux/WSL
   - Same `claudeAiOauth` structure as keychain

3. **Auth Profiles Store** (`~/.openclaw/state/auth-profiles.json`)
   - OpenClaw's own credential store, supports multiple profiles per provider
   - Used when credentials are entered directly (API key or setup-token)

### Credential File Structure

```json
{
  "claudeAiOauth": {
    "accessToken": "string — the OAuth access token",
    "refreshToken": "string — used to get new access tokens",
    "expiresAt": 1700000000000
  }
}
```

For Codex CLI:
- macOS Keychain service: `"Codex Auth"`, account: `"cli|<sha256-of-codex-home>"`
- Filesystem: `~/.codex/auth.json` (overridable via `CODEX_HOME` env var)
- Structure: `{ "tokens": { "access_token": "...", "refresh_token": "..." }, "last_refresh": "..." }`

---

## Three Credential Types

OpenClaw's auth profile system supports three distinct credential types:

### API Key (`type: "api_key"`)
- Static string, no expiry, no refresh
- User manually rotates
- Simplest but requires separate purchase

### Token (`type: "token"`)
- Generated via `claude setup-token` command
- Optional expiry, not auto-refreshable
- Bridge between API key simplicity and OAuth

### OAuth (`type: "oauth"`)
- Access token + refresh token + expiry timestamp
- Auto-refreshable when expired
- This is what subscription tokens use

---

## Token Refresh Mechanism

When an OAuth token is expired:

1. **File locking** — acquires lock on `auth-profiles.json` using `proper-lockfile`
   - 10 retries, exponential backoff (100ms–10s), 30s stale timeout
   - Prevents concurrent refresh race conditions

2. **Provider-specific refresh** — calls the appropriate OAuth refresh endpoint

3. **Concurrent refresh detection** — if refresh fails, re-reads the file to check
   if another process already refreshed it (solves thundering herd)

4. **Sub-agent inheritance** — if a sub-agent's credentials are expired,
   falls back to the main agent's fresh credentials

### Expiry Buffer
Chutes implementation refreshes 5 minutes before actual expiry. This pattern
prevents edge-case failures when a token expires mid-request.

---

## Auth Profile System

Profiles are named as `<provider>:<identifier>`:
- `anthropic:default` — default Anthropic profile
- `anthropic:claude-cli` — imported from Claude CLI
- `openai-codex:codex-cli` — imported from Codex CLI

### Profile Ordering & Failover

When multiple profiles exist for a provider:

1. **Explicit user order** (config) takes priority
2. **Type preference**: OAuth > Token > API Key
3. **Round-robin by last-used** (oldest-used first)
4. **Cooldown-aware**: rate-limited profiles pushed to end

### Cooldown Schedule (Exponential Backoff)
| Error Count | Cooldown |
|------------|----------|
| 1st | 1 minute |
| 2nd | 5 minutes |
| 3rd | 25 minutes |
| 4th+ | max 1 hour |
| Billing errors | 5 hours (max 24h) |

Failure window: 24 hours (older errors don't count).

---

## External CLI Credential Sync

OpenClaw auto-syncs credentials from external CLI tools on startup:

| CLI Tool | Source Path | Profile ID |
|----------|-----------|------------|
| Claude CLI | `~/.claude/.credentials.json` + macOS keychain | `anthropic:claude-cli` |
| Codex CLI | `~/.codex/auth.json` + macOS keychain | `openai-codex:codex-cli` |
| Qwen Portal | `~/.qwen/oauth_creds.json` | `qwen-portal:qwen-cli` |
| MiniMax Portal | `~/.minimax/oauth_creds.json` | `minimax-portal:minimax-cli` |

Sync uses a 15-minute TTL cache. Near-expiry tokens (within 10 minutes) trigger re-sync.

---

## Takeaways for Our Project

### What to adopt
- **Borrow CLI tokens from well-known paths** — the killer feature. Users authenticate
  once via `claude` or `codex` CLI, and our app reads those tokens.
- **File-based locking for token refresh** — prevents race conditions when multiple
  processes share credentials.
- **Concurrent refresh detection** — re-read after failed refresh to check if another
  process succeeded.
- **Expiry buffer** — refresh before actual expiry to avoid mid-request failures.

### What to simplify
- **We don't need the full auth profile system** — OpenClaw supports dozens of providers
  with round-robin failover. We only need Claude CLI and Codex CLI credential reading.
- **Skip the marketplace provider complexity** — no Chutes, Qwen, MiniMax, etc.
- **Rust-native keychain access** — instead of shelling out to `security` on macOS,
  use a crate like `keyring` for cross-platform credential access.

### What to be careful about
- **Never store tokens in plaintext config** — always use OS keychain or
  appropriately-permissioned files.
- **Token refresh is the CLI's job** — our app should read tokens, not try to refresh
  Anthropic OAuth tokens directly. If expired, prompt user to re-auth via CLI.
- **Codex token expiry is guesswork** — OpenClaw uses file mtime + 1 hour as a heuristic
  since Codex doesn't store explicit expiry. Worth validating this assumption.
