# Known Bugs

## BUG-001: "Session ID already in use" after stale session cleanup

**Status**: Fixed (commit `d561694`)
**Severity**: High — blocks all conversation recovery after daemon restart
**Component**: `crates/cli-wrapper/src/claude.rs`

### Description

When CLI session mappings are lost or cleared (e.g., corrupted state, manual cleanup), the daemon attempts to create a **new** Claude CLI session using `--session-id {conversation_id}`. If Claude CLI still has a session with that ID from a previous run, it rejects the request with:

```
Error: Session ID 69285a00-... is already in use.
```

The conversation becomes permanently stuck — it can't resume (no session mapping) and can't create new (ID conflict).

### Root Cause

`build_new_session_args()` uses the conversation UUID as the CLI session ID (`--session-id {conversation_id}`). This creates a 1:1 coupling between conversation identity and CLI session identity. If the mapping file is lost but Claude CLI retains the session, there's no recovery path.

### Reproduction

1. Start daemon, send messages (creates sessions)
2. Stop daemon
3. Delete or clear `~/.threshold/cli-sessions/cli-sessions.json`
4. Restart daemon
5. Send a message in an existing conversation channel
6. Error: "Session ID ... is already in use"

### Fix

Implemented option 2 — decoupled session IDs from conversation IDs:

1. `build_new_session_args()` now takes a fresh `Uuid::new_v4()` instead of reusing the conversation ID
2. If the CLI still returns "already in use" (edge case), falls back to `--resume` with the same session ID
3. Session mappings stored in `cli-sessions.json` as `conversation_id -> session_id`

This ensures a conversation can always create a new CLI session without conflicting with old ones.

### Related Issues

- The initial timeouts (before this error) were caused by `skip_permissions = false` (default). The daemon ran Claude CLI without `--dangerously-skip-permissions`, so tool use prompts blocked the non-interactive subprocess until the 300s timeout. Fixed by setting `skip_permissions = true` in config.
- macOS Keychain authorization dialogs can also block subprocesses when they access keychain items for the first time (e.g., `threshold gmail` reading OAuth tokens). The user must approve once per binary.

### Workaround

Clear all state files and restart with fresh conversations:

```bash
# Clear sessions, conversations, and portals
echo '{"sessions":{}}' > ~/.threshold/cli-sessions/cli-sessions.json
echo '{"conversations":{}}' > ~/.threshold/conversations.json
echo '{"portals":{}}' > ~/.threshold/portals.json
```

Then restart daemon and start new conversations in Discord.
