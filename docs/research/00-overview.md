# OpenClaw Reconnaissance — Overview

## What This Is

A best-practices study of the OpenClaw project (v2026.2.6), a multi-channel personal
AI assistant. We're extracting architectural patterns and implementation details
to inform the design of a secure, Rust-based home assistant AI.

## What OpenClaw Does Well

- **Subscription token reuse** — reads OAuth tokens from Claude Code and Codex CLI
  credential stores, making AI access affordable via existing subscriptions
- **Multi-channel gateway** — single assistant reachable from Telegram, Discord,
  WhatsApp, Signal, Slack, and more
- **Provider abstraction** — pluggable backends with failover (Anthropic, OpenAI,
  Google, Bedrock, local Ollama)
- **Session isolation** — configurable per-peer, per-channel, or shared sessions
- **Resilient credential management** — file locking, concurrent refresh detection,
  exponential backoff for rate limits

## What We Want to Avoid

- **Custom context management** — OpenClaw rolls its own compaction/pruning for
  direct API calls. This is token-wasteful and can't compete with what the CLIs
  do internally. Delegate to CLIs for conversations.
- **Extension/skill marketplace** — the plugin system is the primary attack surface.
  Third-party code execution is a non-starter for a secure home assistant.
- **Over-broad provider support** — dozens of providers with portal/proxy integrations
  add complexity and attack surface. We need two: Claude CLI and Codex CLI.
- **Massive session metadata** — 20+ per-session toggle fields. A home assistant
  needs session key, timestamps, and CLI session ID.

## Core Architectural Insight

There are two paths to use an LLM:

1. **Direct API** — you manage context, tokens, tool execution, everything.
   Full control but full responsibility. Token-hungry if not done perfectly.

2. **CLI wrapper** — spawn `claude` or `codex` as a subprocess.
   They manage context, caching, compaction, auth. You just pass messages
   and read responses. Less control but dramatically better token efficiency.

**Our approach**: CLI wrapper for conversations, direct API (sparingly) for
short stateless inference tasks (classification, extraction, summarization).

---

## Documents

| Doc | Contents |
|-----|----------|
| [01-oauth-credential-flow.md](01-oauth-credential-flow.md) | How subscription tokens are borrowed from CLI credential stores. Credential sources, types, refresh mechanism, profile system. |
| [02-session-management.md](02-session-management.md) | Session storage (JSONL transcripts), lifecycle, context management, compaction, multi-channel isolation. |
| [03-claude-code-cli-wrapper.md](03-claude-code-cli-wrapper.md) | Wrapping `claude` CLI — flags, I/O, session continuity, env isolation, model aliases, error handling. |
| [04-codex-cli-wrapper.md](04-codex-cli-wrapper.md) | Wrapping `codex` CLI — subcommand pattern, JSONL streaming, thread-based sessions, auth differences. |
| [05-channel-integration.md](05-channel-integration.md) | Messaging platform pipeline — plugin architecture, message normalization, routing, auto-reply, outbound delivery. Includes portal/conversation architecture sketch for our project. |
| [06-voice-audio-architecture.md](06-voice-audio-architecture.md) | TTS providers (ElevenLabs/OpenAI/Edge), STT (OpenAI Realtime), telephony, wake words, mobile voice, incremental TTS. Privacy-first audio architecture for room portals. |
| [07-elevenlabs-integration.md](07-elevenlabs-integration.md) | Deep dive on ElevenLabs TTS — API details, voice config, AI-controlled directives, auto-summarization, platform-specific formats, mobile direct-call pattern, telephony audio conversion. |
| [08-api-key-management.md](08-api-key-management.md) | Third-party API key storage — config files, env vars, auth profiles, keychain. Sentinel-based redaction in web UI. Encryption-at-rest gap. Rust keychain sketch. |
| [09-tool-execution-model.md](09-tool-execution-model.md) | How AI agents get tools — core tools, channel tools, plugin tools. Policy framework (profiles, groups, per-agent). Tool schema adaptation per provider. |
| [10-playwright-browser-automation.md](10-playwright-browser-automation.md) | Playwright CLI (`@playwright/cli`) — token-efficient browser automation for AI agents. Session persistence, CLI commands, network filtering. Comparison with Playwright MCP. |

---

## Key Design Principles for Our Project

### 1. CLI-First Architecture
Use `claude` and `codex` CLIs as the primary inference engines. They handle:
- Context window management and compaction
- Token optimization and caching
- Authentication and token refresh
- Tool permission management

### 2. Secure by Default
- No third-party extensions or plugins
- No marketplace or remote code installation
- All data stays on user's machine
- Minimal network surface (only CLI → provider API)
- Rust backend for memory safety and long-term stability

### 3. Borrow, Don't Own, Auth
- Read tokens from `~/.claude/.credentials.json` and `~/.codex/auth.json`
- Never implement our own OAuth dance with Anthropic/OpenAI
- If tokens expire, prompt user to re-auth via CLI
- Clear `ANTHROPIC_API_KEY` when spawning CLI subprocesses

### 4. Conversations, Not Channels
- **Conversations are first-class entities**, not derived from channels
- **Portals** (Discord, voice speakers, web UI, phone) attach/detach from conversations
- A **default General conversation** is always running — accessible from any portal
- **Mode switching** via slash commands creates specialized sessions (coding, research)
- Multiple portals can be in the same conversation simultaneously
- CLI manages conversation history internally per session
- Append-only JSONL for our own audit trail

### 5. Minimal Direct API Usage
- Classification / routing: "which agent should handle this?"
- Small extraction tasks: "what's the meeting time from this email?"
- Summary generation: "give me a one-line summary of this thread"
- Never for multi-turn conversations — that's the CLI's job

### 6. Privacy-First Voice
- Wake word detection: local only (OpenWakeWord / Porcupine)
- Speech-to-text: local first (Whisper.cpp), cloud as opt-in fallback (Groq Whisper)
- Text-to-speech: local first (Piper), cloud for quality upgrade
- Raw audio never leaves the home network
- Only transcribed text goes to AI providers via CLI's encrypted channel

### 7. Native Integrations Only
- All integrations built into the codebase — no marketplace, no plugins
- Enable/disable via config — Gmail, image generation, browser, calendar
- Open source, auditable, validated by the community
- Playwright CLI for browser automation (token-efficient, session-persistent)
- Groq for fast cloud Whisper STT when local hardware is insufficient

### 8. Secrets in Keychain, Not Config
- API keys stored in OS keychain (`keyring` crate), not plaintext config
- Config file holds non-sensitive settings only
- Sentinel-based redaction in web UI (pattern from OpenClaw)
- Env var fallback for containerized/CI deployments
- Audit logging for all credential access
