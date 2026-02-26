# Milestone 16 — Daemon Management & Self-Update

**Crates:** `core`, `server`, `scheduler`, `conversation`
**Complexity:** Medium-High
**Dependencies:** Milestone 1 (daemon), Milestone 6 (scheduler), Milestone 14 (streaming/broadcast)

## What This Milestone Delivers

Infrastructure for agents to rebuild, restart, and manage the Threshold daemon — enabling autonomous self-improvement without human intervention. The system is designed for a "run from source" deployment model where the full source repository is always available and agents can modify code, recompile, and restart the running system.

1. **PID file & daemon discovery** — The daemon writes a PID file on startup, enabling other processes to find and signal the running instance. A `threshold daemon status` command reports whether the daemon is running and its health.
2. **Health check endpoint** — The daemon API gains a `Health` command returning uptime, version, and readiness via the existing Unix socket protocol. The web `/status` endpoint already provides HTTP-level health; this extends the socket API for CLI-level checks.
3. **Graceful restart command** — `threshold daemon restart` orchestrates a full stop → build → start cycle. In supervised mode (wrapper/launchd), it delegates the restart to the supervisor. In standalone mode, it handles the full lifecycle directly.
4. **Restart follow-on hook** — Before restarting, the agent can register a "follow-on" task: a prompt to inject into a conversation once the new daemon is ready. This gives agents continuity through restarts. Hooks are processed directly by the conversation engine on startup — no scheduler dependency.
5. **Agent-triggered restart** — The agent triggers restart by calling `threshold daemon restart` via shell execution (the same pattern used for `threshold schedule`, `threshold gmail`, and other CLI commands). No new built-in tool is needed.
6. **launchd integration** — `threshold daemon install` creates a macOS launchd plist for auto-start on boot. A wrapper script (`scripts/threshold-wrapper.sh`) handles rebuild-before-restart logic.

### What This Does NOT Do

- **No hot-reload** — Code changes require a full restart. Rust's compiled nature makes hot-reload impractical and the graceful restart is fast enough (seconds).
- **No rolling updates** — There's one daemon instance per machine. No load balancer or blue-green deployment.
- **No remote management** — All commands execute locally. Remote restart would require SSH or a future authenticated API.
- **No automatic code changes** — The restart tool rebuilds whatever source is on disk. The agent must make code changes and run tests before triggering the restart.
- **No systemd support** — This milestone targets macOS (launchd). Linux systemd support would be a straightforward follow-up using the same patterns.
- **No new built-in tool** — The agent triggers restarts via shell execution of the `threshold` CLI, consistent with how it already uses other CLI subcommands. The `ToolRegistry` is not wired into the conversation engine's streaming pipeline, so a built-in tool would not be invokable.

---

## Architecture

### Current State

```
terminal ──→ threshold daemon ──→ [Discord, Scheduler, Web, ConversationEngine]
                                        │
                                      Ctrl+C
                                        │
                                  cancel.cancel() → save_state() → exit
```

**How the daemon currently stops:** `tokio::signal::ctrl_c()` fires in the main `tokio::select!` loop (`server/src/main.rs:463`). The `CancellationToken` is cancelled, all subsystems receive the cancellation and wind down, `engine.save_state()` persists conversations + portals to disk, and the process exits. Note: SIGTERM is **not** currently handled — only Ctrl+C triggers clean shutdown. Phase 16A adds SIGTERM handling.

**What's missing:**
1. No PID file — other processes can't find the daemon.
2. No programmatic stop — only Ctrl+C or raw `kill` works.
3. No rebuild integration — the human must run `cargo build` separately.
4. No restart continuity — after restart, the agent doesn't know it happened unless the human tells it.
5. No auto-start on boot — the human must manually run `threshold daemon` after every reboot.

### New Flow

```
Agent (in conversation):
  "I've fixed the bug. Let me restart to pick up the changes."
  ──→ Runs: threshold daemon restart --follow-on-conversation <id> \
             --follow-on-prompt "Restart complete. Verifying changes."

threshold daemon restart (CLI process, outside the daemon):
  ├── 1. Read PID from $DATA_DIR/threshold.pid
  ├── 2. Write follow-on hook to $DATA_DIR/state/restart-hooks.json
  ├── 3. Send SIGTERM to daemon PID
  ├── 4. Wait for daemon process to exit (poll PID, timeout 30s)
  ├── 5. Run `cargo build -p threshold` from repo root
  ├── 6. Start new daemon process (detached)
  ├── 7. Poll Health command on Unix socket until ready (timeout 60s)
  └── 8. Print success: "Daemon restarted (PID 12345)"

New daemon starts:
  ├── Load state from disk (conversations, portals, schedules)
  ├── Check $DATA_DIR/state/restart-hooks.json
  ├── For each hook: call send_to_conversation(conversation_id, prompt)
  ├── Remove successfully processed hooks (preserve failed ones for retry)
  └── Agent's conversation receives: "Restart complete. Verifying changes."
```

### Restart Orchestration Modes

There are two modes, determined by how the daemon is running:

**Standalone mode** (`threshold daemon start` invoked directly):
- `threshold daemon restart` handles the full stop → build → start → health-check cycle.
- `threshold daemon stop` sends SIGTERM and waits for exit. The daemon stays down.

**Supervised mode** (running under `scripts/threshold-wrapper.sh` or launchd):
- `threshold daemon restart` writes `$DATA_DIR/state/restart-pending.json`, then sends SIGTERM.
- The wrapper detects `restart-pending.json`, optionally rebuilds, and starts the new daemon.
- `threshold daemon stop` writes a `$DATA_DIR/state/stop-sentinel` file, then sends SIGTERM.
- The wrapper detects the stop sentinel and exits its loop instead of restarting.
- Detection: the wrapper writes a `$DATA_DIR/state/supervised` marker file containing its PID and process start time (e.g., `{"wrapper_pid": 12345, "started_at": "2026-02-25T12:00:00Z"}`). The restart command reads this file, checks if the wrapper PID is alive (`kill(pid, 0)`), and validates wrapper identity by checking the process name (same pattern as `is_threshold_process()`). If the marker is stale (wrapper PID dead, PID recycled to a different process, or identity check fails), the CLI deletes it and proceeds in standalone mode.

This avoids the race condition of both the CLI and the wrapper trying to start a new daemon simultaneously.

### Rejected Alternatives

- **Built-in `restart_daemon` tool via ToolRegistry**: The conversation engine streams Claude CLI output and prepends a tool prompt to the system prompt. It does not wire `ToolRegistry` into the streaming pipeline or route tool calls through it. The agent invokes CLI subcommands via shell execution, so `threshold daemon restart` follows the established pattern.
- **In-process restart**: The daemon cannot restart itself because the async runtime and all subsystems must fully shut down first. An external process (CLI command or wrapper script) must orchestrate the restart.

---

## PID File & Process Discovery

### Design

**PID file location:** `$DATA_DIR/threshold.pid`

Written at daemon startup, deleted on clean shutdown. Contains the process ID as a plain integer.

```rust
// crates/server/src/main.rs — in run_daemon(), after logging init

fn write_pid_file(data_dir: &Path) -> Result<PathBuf> {
    let pid_path = data_dir.join("threshold.pid");
    std::fs::write(&pid_path, std::process::id().to_string())?;
    tracing::info!(pid = std::process::id(), path = %pid_path.display(), "PID file written");
    Ok(pid_path)
}

fn remove_pid_file(pid_path: &Path) {
    if let Err(e) = std::fs::remove_file(pid_path) {
        tracing::warn!(path = %pid_path.display(), error = %e, "Failed to remove PID file");
    }
}

fn read_pid_file(data_dir: &Path) -> Option<u32> {
    let pid_path = data_dir.join("threshold.pid");
    std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}
```

### Stale PID Detection

A PID file may be stale if the daemon crashed without cleanup. On startup:

1. Read existing PID file
2. Check if process with that PID is alive (`kill(pid, 0)` on Unix)
3. If alive, verify it's a Threshold process (check process name via `sysctl` on macOS)
4. If it's a running Threshold daemon → error: `DaemonAlreadyRunning { pid }`
5. If the PID doesn't exist or isn't Threshold → stale PID file → overwrite it

```rust
fn check_existing_daemon(data_dir: &Path) -> Result<()> {
    if let Some(pid) = read_pid_file(data_dir) {
        // Check if process is alive (0 signal = existence check)
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if alive {
            if is_threshold_process(pid) {
                return Err(ThresholdError::DaemonAlreadyRunning { pid });
            }
            tracing::warn!(pid, "PID file exists for non-Threshold process, overwriting");
        } else {
            tracing::info!(pid, "Stale PID file found, overwriting");
        }
    }
    Ok(())
}
```

This mirrors the existing stale socket detection in `daemon_api.rs:114-115` (`handle_stale_socket`) — same pattern, applied to the PID file.

---

## Health Check

### Daemon API Extension

Add a `Health` command to the existing daemon API (`scheduler/src/daemon_api.rs`):

```rust
// crates/scheduler/src/daemon_api.rs — extend DaemonCommand enum
pub enum DaemonCommand {
    // ... existing: ScheduleCreate, ScheduleList, ScheduleDelete, ScheduleToggle ...
    Health,
}
```

The `DaemonApi` needs access to health state beyond the scheduler. Add a `HealthConfig` struct for static fields and compute dynamic fields per request:

```rust
// crates/core/src/types.rs — new struct (static fields only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub version: String,
}
```

The `DaemonApi` constructor gains a `HealthConfig` parameter (not `Arc<RwLock<>>` — these fields never change after startup). Scheduler task counts are computed dynamically per health request by calling `scheduler.list_tasks()`, so they always reflect the current state. Fields that require cross-crate queries (conversation count, Discord status) are deferred to a future milestone — this keeps the health check lightweight and avoids coupling the daemon API to the conversation engine or Discord crate.

Health response uses the existing `DaemonResponse` envelope (`daemon_api.rs:42`), with health payload in the `data` field:

```json
{
    "version": 1,
    "status": "ok",
    "data": {
        "pid": 12345,
        "uptime_secs": 3600,
        "version": "0.1.0",
        "scheduler_task_count": 8,
        "scheduler_enabled_count": 3
    }
}
```

### CLI Status Command

```bash
$ threshold daemon status [--data-dir <path>]

Threshold Daemon
  Status:    Running
  PID:       12345
  Uptime:    2h 30m 15s
  Version:   0.1.0
  Scheduler: 8 tasks (3 enabled)
```

Implementation: connects to the Unix socket at `$DATA_DIR/threshold.sock`, sends `Health`, and formats the response. If the socket doesn't exist or connection fails, falls back to PID file check:

```
$ threshold daemon status
Threshold Daemon
  Status:    Not running (stale PID file for PID 12345)
```

---

## Graceful Restart

### Signal Handling

Currently the daemon only handles `ctrl_c`. Extend to handle `SIGTERM`:

```rust
// crates/server/src/main.rs — in the tokio::select! block
tokio::select! {
    // ... existing task handles ...
    _ = tokio::signal::ctrl_c() => {
        tracing::info!("Shutdown signal received (Ctrl+C).");
    }
    _ = sigterm_signal() => {
        tracing::info!("Shutdown signal received (SIGTERM).");
    }
}

async fn sigterm_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate())
        .expect("Failed to register SIGTERM handler");
    sigterm.recv().await;
}
```

### Restart Command (Standalone Mode)

```
threshold daemon restart [--skip-build] [--data-dir <path>] \
    [--follow-on-conversation <uuid> --follow-on-prompt <text>]
```

**Steps:**

1. **Resolve data dir** — from `--data-dir`, `THRESHOLD_DATA_DIR`, or default `~/.threshold`
2. **Verify daemon is running** — Read PID file, check process alive, validate it's a Threshold process (same `is_threshold_process()` check used at startup). Error if PID is stale or belongs to a different process.
3. **Detect supervised mode** — Check for `$DATA_DIR/state/supervised` marker. If present, delegate to supervised restart (see below).
4. **Write follow-on hook** (if provided) — Atomic write to `$DATA_DIR/state/restart-hooks.json` (write to temp file, then rename)
5. **Send SIGTERM** — `kill(pid, SIGTERM)` (only after identity validation in step 2)
6. **Wait for exit** — Poll process existence, timeout after 30 seconds
7. **Build** (unless `--skip-build`) — Run `cargo build -p threshold` from the repository root
8. **Start new daemon** — Spawn `threshold daemon start` as a detached process
9. **Wait for healthy** — Poll `Health` command via Unix socket, timeout after 60 seconds
10. **Report success** — Print new PID, build time, startup time

### Restart Command (Supervised Mode)

When the supervised marker is detected:

1. Write `restart-pending.json` to `$DATA_DIR/state/` (atomic write)
2. Write follow-on hook (if provided)
3. Send SIGTERM to daemon PID
4. Print: "Restart signal sent. The supervisor will rebuild and restart the daemon."
5. Exit — the wrapper script handles the rest

### Stop Command

```
threshold daemon stop [--data-dir <path>]
```

1. Read PID file, validate process identity (same `is_threshold_process()` check)
2. If supervised mode: write `$DATA_DIR/state/stop-sentinel` file
3. Send SIGTERM (only after identity validation)
4. Wait for process exit (timeout 30s)
5. Print: "Daemon stopped."

Under supervised mode, the stop sentinel tells the wrapper script to exit its loop instead of restarting the daemon.

---

## Restart Follow-On Hooks

### Design

A follow-on hook is a file-based message queue that survives daemon restarts. Before triggering a restart, the caller writes a hook to disk. On startup, the daemon reads hooks, processes them sequentially, and atomically rewrites the file with only failed hooks (deleting the file entirely if all succeed).

**File location:** `$DATA_DIR/state/restart-hooks.json`

**Schema:**

```rust
// crates/core/src/types.rs — new struct
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartHook {
    /// Conversation to resume after restart.
    pub conversation_id: ConversationId,
    /// Prompt to inject into the conversation.
    pub prompt: String,
    /// When the hook was created.
    pub created_at: DateTime<Utc>,
    /// Who requested the restart (for audit trail).
    pub requested_by: Option<String>,
}
```

### File Robustness

All hook/pending file writes use atomic write-temp-then-rename:

```rust
fn write_hooks_atomic(path: &Path, hooks: &[RestartHook]) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to temp file, then rename for atomicity
    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(hooks)?;
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}
```

### Startup Processing

In `run_daemon()`, after all subsystems are initialized and healthy, hooks are processed directly via the conversation engine — no scheduler dependency:

```rust
// crates/server/src/main.rs — after engine, scheduler, and discord are initialized

async fn process_restart_hooks(
    data_dir: &Path,
    engine: &Arc<ConversationEngine>,
) -> Result<()> {
    let hooks_path = data_dir.join("state/restart-hooks.json");
    if !hooks_path.exists() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&hooks_path)?;
    let hooks: Vec<RestartHook> = serde_json::from_str(&raw)?;

    if hooks.is_empty() {
        std::fs::remove_file(&hooks_path)?;
        return Ok(());
    }

    tracing::info!(count = hooks.len(), "Processing restart follow-on hooks");

    let mut failed_hooks = Vec::new();
    for hook in hooks {
        let conversation_id = hook.conversation_id;
        let prompt = &hook.prompt;
        // Process sequentially to track success/failure per hook
        match engine.send_to_conversation(&conversation_id, prompt).await {
            Ok(_) => tracing::info!(
                conversation_id = %conversation_id.0,
                "Restart follow-on delivered"
            ),
            Err(e) => {
                tracing::error!(
                    conversation_id = %conversation_id.0,
                    error = %e,
                    "Failed to deliver restart follow-on — preserving hook for retry"
                );
                failed_hooks.push(hook);
            }
        }
    }

    // Rewrite file with only failed hooks; delete if all succeeded
    if failed_hooks.is_empty() {
        std::fs::remove_file(&hooks_path)?;
    } else {
        write_hooks_atomic(&hooks_path, &failed_hooks)?;
        tracing::warn!(
            remaining = failed_hooks.len(),
            "Some restart hooks failed — preserved for next startup"
        );
    }
    Ok(())
}
```

### One-Shot Scheduled Tasks (Independent Enhancement)

As a standalone improvement in this milestone, the scheduler gains a `one_shot` field for tasks that should auto-delete after first execution. This is **not** used by the follow-on hook system (which processes hooks directly via the conversation engine). It's a general-purpose scheduler enhancement that enables "run once at next opportunity" patterns for future use cases.

```rust
// crates/scheduler/src/task.rs — add field to ScheduledTask
pub struct ScheduledTask {
    // ... existing fields ...

    /// If true, this task is deleted after its first successful execution.
    /// Useful for one-time triggers (e.g., "run this prompt once at next opportunity").
    #[serde(default)]
    pub one_shot: bool,
}
```

In the scheduler's completion handler (`engine.rs`), after a task completes:

```rust
// Inside the completion handling match arm in Scheduler::run()
if task.one_shot {
    if let Some(pos) = self.tasks.iter().position(|t| t.id == task_id) {
        let removed = self.tasks.remove(pos);
        tracing::info!(task_id = %removed.id, name = %removed.name, "One-shot task completed and removed");
        self.persist().await;
    }
}
```

Note: The `ScheduledTask::new()` constructor already defaults all optional fields. The `one_shot` field is added to the struct with `#[serde(default)]`, so existing `schedules.json` files deserialize without changes (defaulting to `false`).

---

## Wrapper Script & launchd

### Wrapper Script

`scripts/threshold-wrapper.sh` is a simple loop that runs the daemon and handles restart/stop signals via sentinel files.

```bash
#!/bin/bash
# scripts/threshold-wrapper.sh — restart loop with build support
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA_DIR="${THRESHOLD_DATA_DIR:-$HOME/.threshold}"
STATE_DIR="$DATA_DIR/state"

mkdir -p "$STATE_DIR"

# Write supervised marker with wrapper PID and start time so CLI can verify liveness
echo "{\"wrapper_pid\": $$, \"started_at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"}" > "$STATE_DIR/supervised"

cleanup() {
    rm -f "$STATE_DIR/supervised"
}
trap cleanup EXIT

while true; do
    # Check for stop sentinel — exit loop instead of restarting
    if [ -f "$STATE_DIR/stop-sentinel" ]; then
        rm -f "$STATE_DIR/stop-sentinel"
        echo "[wrapper] Stop sentinel found. Exiting."
        break
    fi

    # Check if a rebuild was requested
    if [ -f "$STATE_DIR/restart-pending.json" ]; then
        SKIP_BUILD=$(python3 -c "
import json, sys
try:
    d = json.load(open('$STATE_DIR/restart-pending.json'))
    print(str(d.get('skip_build', False)).lower())
except: print('false')
" 2>/dev/null || echo "false")
        rm -f "$STATE_DIR/restart-pending.json"

        if [ "$SKIP_BUILD" != "true" ]; then
            echo "[wrapper] Building from source..."
            (cd "$REPO_ROOT" && cargo build -p threshold) || {
                echo "[wrapper] Build failed. Starting with existing binary."
            }
        fi
    fi

    echo "[wrapper] Starting daemon..."
    EXIT_CODE=0
    "$REPO_ROOT/target/debug/threshold" daemon start || EXIT_CODE=$?

    # Check for stop sentinel again (may have been written during shutdown)
    if [ -f "$STATE_DIR/stop-sentinel" ]; then
        rm -f "$STATE_DIR/stop-sentinel"
        echo "[wrapper] Stop sentinel found after exit. Exiting."
        break
    fi

    if [ $EXIT_CODE -ne 0 ] && [ ! -f "$STATE_DIR/restart-pending.json" ]; then
        echo "[wrapper] Daemon exited with code $EXIT_CODE. Waiting 5s before restart..."
        sleep 5
    fi
done

echo "[wrapper] Wrapper exiting."
```

### Restart Pending File

Written by `threshold daemon restart` in supervised mode:

```json
{
    "skip_build": false,
    "requested_at": "2026-02-25T23:00:00Z",
    "requested_by": "agent:general-assistant"
}
```

### launchd Plist

`threshold daemon install` creates a launchd plist that runs the wrapper script:

```bash
$ threshold daemon install [--data-dir <path>]
Created launchd service: com.threshold.daemon
  Plist: ~/Library/LaunchAgents/com.threshold.daemon.plist
  Log:   ~/.threshold/logs/launchd-stdout.log

To start now: launchctl load ~/Library/LaunchAgents/com.threshold.daemon.plist
```

**Plist template:**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.threshold.daemon</string>

    <key>ProgramArguments</key>
    <array>
        <string>{repo_root}/scripts/threshold-wrapper.sh</string>
    </array>

    <key>WorkingDirectory</key>
    <string>{repo_root}</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>THRESHOLD_CONFIG</key>
        <string>{config_path}</string>
        <key>THRESHOLD_DATA_DIR</key>
        <string>{data_dir}</string>
        <key>PATH</key>
        <string>{path_with_cargo}</string>
    </dict>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <false/>
    <!-- KeepAlive is false because the wrapper script handles restart logic.
         launchd starts the wrapper; the wrapper loops internally. -->

    <key>StandardOutPath</key>
    <string>{data_dir}/logs/launchd-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{data_dir}/logs/launchd-stderr.log</string>
</dict>
</plist>
```

### Uninstall Command

```bash
$ threshold daemon uninstall
Unloading launchd service...
Removing plist: ~/Library/LaunchAgents/com.threshold.daemon.plist
Service removed. The daemon will no longer start automatically.
```

---

## CLI Subcommand Changes

### Current

```
threshold daemon [--config <path>]
threshold schedule <list|create|delete|toggle|resume>
threshold gmail <subcommand>
threshold imagegen <subcommand>
```

### New

```
threshold daemon start [--config <path>]           # Renamed from bare `daemon`
threshold daemon stop [--data-dir <path>]           # Send SIGTERM, wait for exit
threshold daemon restart [--data-dir <path>]        # Stop + optional build + start
                         [--skip-build]
                         [--follow-on-conversation <uuid>]
                         [--follow-on-prompt <text>]
threshold daemon status [--data-dir <path>]         # Show daemon health info
threshold daemon install [--data-dir <path>]        # Create launchd plist + wrapper
threshold daemon uninstall                          # Remove launchd plist
```

The `daemon` subcommand gains an `action` enum:

```rust
#[derive(clap::Subcommand)]
enum DaemonAction {
    /// Start the daemon (default if no action specified)
    Start {
        #[arg(short, long)]
        config: Option<String>,
    },
    /// Stop the running daemon
    Stop {
        /// Data directory (default: $THRESHOLD_DATA_DIR or ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// Restart the daemon (stop, optional rebuild, start)
    Restart {
        /// Data directory (default: $THRESHOLD_DATA_DIR or ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
        /// Skip cargo build (use existing binary)
        #[arg(long)]
        skip_build: bool,
        /// Conversation ID for follow-on prompt after restart
        #[arg(long)]
        follow_on_conversation: Option<String>,
        /// Follow-on prompt to inject after restart
        #[arg(long)]
        follow_on_prompt: Option<String>,
    },
    /// Show daemon status and health
    Status {
        /// Data directory (default: $THRESHOLD_DATA_DIR or ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// Install as launchd service (macOS)
    Install {
        /// Data directory (default: $THRESHOLD_DATA_DIR or ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// Remove launchd service
    Uninstall,
}
```

All management actions (`stop`, `restart`, `status`, `install`) accept `--data-dir` for non-default data directory locations. The data dir is resolved in order: `--data-dir` flag → `THRESHOLD_DATA_DIR` env var → `~/.threshold` default. The PID file and socket path are derived from the data dir.

**Backward compatibility:** To avoid breaking existing `threshold daemon --config <path>` invocations, `Start` is the default subcommand. If no action is given, it behaves as `threshold daemon start`.

---

## Agent Restart Flow

The agent triggers a restart by running shell commands — the same way it uses `threshold schedule`, `threshold gmail`, and other CLI subcommands:

```
Agent (in a Claude CLI conversation):
  1. Makes code changes (edit files, run tests)
  2. Runs: threshold daemon restart \
       --follow-on-conversation <this-conversation-id> \
       --follow-on-prompt "Restart complete. Verifying the fix took effect."

  Standalone mode:
    3. The CLI command blocks until restart completes (stop + build + start + health check)
    4. Returns: "Daemon restarted (PID 12345, build: 8.2s, startup: 1.3s)"

  Supervised mode:
    3. The CLI writes restart-pending.json and sends SIGTERM
    4. Returns: "Restart signal sent. The supervisor will rebuild and restart."
    5. The wrapper handles build + restart asynchronously

After restart (both modes):
  - New daemon processes the follow-on hook
  - Agent's conversation receives the follow-on prompt
  - Agent continues: "Good, restart succeeded. Let me verify..."
```

The conversation ID is available to the agent via its conversation context. The `memory.md` file for each conversation is at `$DATA_DIR/conversations/{conversation_id}/memory.md`, so the agent can look up or infer its own conversation ID.

---

## Implementation Phases

### Phase 16A — PID File & Signal Handling

**Goal:** Daemon writes a PID file and handles SIGTERM for clean shutdown. Other processes can discover and signal the daemon.

**Changes:**

| File | Change |
|------|--------|
| `crates/server/src/main.rs` | Add `write_pid_file()`, `remove_pid_file()`, `read_pid_file()`, `check_existing_daemon()`, `is_threshold_process()`. Write PID on startup, delete on shutdown. Add SIGTERM handler alongside existing Ctrl+C handler. |
| `crates/core/src/error.rs` | Add `DaemonAlreadyRunning { pid: u32 }` error variant with display message. |
| `Cargo.toml` (server) | Add `libc` dependency for `kill(pid, 0)` process check. |

**Tests:**
- `daemon::pid_file_written_and_cleaned` — Start daemon in test harness, verify PID file exists, stop daemon, verify PID file removed.
- `daemon::stale_pid_file_overwritten` — Write PID file with non-existent PID, start daemon, verify it overwrites.
- `daemon::existing_daemon_detected` — Start a mock daemon process (or use a test helper that simulates `is_threshold_process()` returning true), write its PID to PID file, attempt start, verify `DaemonAlreadyRunning` error.
- `daemon::sigterm_triggers_clean_shutdown` — Start daemon, send SIGTERM, verify `save_state()` called and PID file removed.

### Phase 16B — Health Check & Status Command

**Goal:** Daemon API exposes a health check. CLI can query daemon status.

**Changes:**

| File | Change |
|------|--------|
| `crates/core/src/types.rs` | Add `HealthConfig` struct (pid, started_at, version). |
| `crates/scheduler/src/daemon_api.rs` | Add `DaemonCommand::Health`. Accept `HealthConfig` in `DaemonApi::new()`. Compute task counts dynamically via `list_tasks()`. Return health in `DaemonResponse` envelope. |
| `crates/server/src/main.rs` | Create `HealthConfig` at startup, pass to `DaemonApi`. Restructure `Commands::Daemon` to use `DaemonAction` subcommand enum. Implement `DaemonAction::Status` — connect to socket, send `Health`, format output. |
| `crates/server/src/daemon_client.rs` | Add `send_health_check()` method — connects to socket, sends `Health` command, returns parsed `DaemonResponse` with health JSON payload. |

**Tests:**
- `daemon_api::health_returns_uptime` — Send `Health` command to daemon API, verify response envelope includes pid, uptime, version, task counts in `data` field.
- `daemon_api::health_counts_dynamic` — Add and remove tasks, verify health response reflects current counts.
- `daemon::status_shows_running` — Start daemon, run `threshold daemon status`, verify output.
- `daemon::status_shows_not_running` — No daemon running, run status, verify "Not running" output.
- `daemon_api::health_config_serde_round_trip` — Serialize and deserialize `HealthConfig`.

### Phase 16C — Stop, Restart, and Follow-On Hooks

**Goal:** `threshold daemon stop/restart` commands work in both standalone and supervised modes. Follow-on hooks provide agent continuity through restarts.

**Changes:**

| File | Change |
|------|--------|
| `crates/core/src/types.rs` | Add `RestartHook` struct (conversation_id, prompt, created_at, requested_by). |
| `crates/server/src/main.rs` | Implement `DaemonAction::Stop` (detect supervised mode, write stop sentinel if needed, send SIGTERM, wait for exit). Implement `DaemonAction::Restart` (detect mode, write hooks atomically, write restart-pending if supervised, stop + build + start if standalone, poll health). Add `resolve_data_dir()`, `find_repo_root()`, `write_hooks_atomic()`, `detect_supervised()`, `wait_for_process_exit()`, `wait_for_healthy()` helpers. Add `process_restart_hooks()` — on startup, read hooks, process sequentially via `send_to_conversation()`, preserve failed hooks. |
| `crates/scheduler/src/task.rs` | Add `one_shot: bool` field to `ScheduledTask` (with `#[serde(default)]`). |
| `crates/scheduler/src/engine.rs` | After task completion, if `one_shot`, remove the task from `self.tasks` and persist. |

**Tests:**
- `daemon::stop_sends_sigterm` — Start daemon in background, run stop, verify process exits.
- `daemon::stop_writes_sentinel_in_supervised_mode` — Set supervised marker, run stop, verify sentinel file created.
- `hooks::write_and_read_round_trip` — Write restart hooks atomically, read back, verify fields match.
- `hooks::atomic_write_creates_parent_dirs` — Write hooks to non-existent state dir, verify `create_dir_all` works.
- `hooks::processed_on_startup` — Write hook file, start daemon, verify `send_to_conversation()` called and hook file deleted.
- `hooks::partial_failure_preserves_hooks` — Write multiple hooks, simulate delivery failure on one (e.g., non-existent conversation), verify the failed hook is preserved in the rewritten hooks file while successful hooks are removed.
- `scheduler::one_shot_task_auto_deletes` — Create one-shot task, let it fire, verify it's removed from task list.
- `scheduler::one_shot_backward_compat` — Load old `schedules.json` without `one_shot` field, verify it deserializes (defaults to `false`).

### Phase 16D — Wrapper Script & launchd Integration

**Goal:** Auto-start on boot, supervised restart with optional rebuild.

**Changes:**

| File | Change |
|------|--------|
| `scripts/threshold-wrapper.sh` | New file: wrapper script with restart loop, stop sentinel check, supervised marker, and rebuild-before-restart support. |
| `crates/server/src/main.rs` | Implement `DaemonAction::Install` (generate plist with repo root, config path, data dir; write to `~/Library/LaunchAgents/`). Implement `DaemonAction::Uninstall` (unload + remove plist). Add `generate_plist()` and `find_repo_root()` helpers. |

**Tests:**
- `launchd::plist_generated_correctly` — Generate plist from known paths, verify XML structure, environment variables, and program arguments.
- `launchd::install_writes_plist` — Run install (with test paths), verify file created at expected location.
- `launchd::uninstall_removes_plist` — Create plist, run uninstall, verify file removed.
- `wrapper::stop_sentinel_exits_loop` — Integration test: create stop sentinel, verify wrapper exits.
- `wrapper::restart_pending_triggers_rebuild` — Integration test: create restart-pending.json with `skip_build: false`, verify build runs.
- Note: Actual launchd loading/unloading is manual-test only (requires user login session).

---

## Backward Compatibility

All new persisted fields use serde defaults:

- `ScheduledTask.one_shot: bool` — `#[serde(default)]`, defaults to `false`. Existing tasks are unaffected.
- `RestartHook` — New file, no backward compat concern.
- `HealthConfig` — Static config struct, runtime-only (not persisted across restarts), no backward compat concern.
- `restart-pending.json`, `restart-hooks.json`, `stop-sentinel` — New files, no backward compat concern.
- `threshold.pid` — New file, no backward compat concern.

CLI backward compatibility: The `threshold daemon` command (no subcommand) must continue to work as before, starting the daemon. This is achieved by making `Start` the default subcommand via clap's `#[command(default)]` or by parsing `daemon` without a subcommand as `Start`.

---

## Verification

After all phases:
```bash
cargo test --workspace --lib          # All unit tests pass
cargo build --workspace               # Full compilation
```

Manual testing sequence:
1. Start daemon normally: `threshold daemon start` — verify PID file written
2. Check status: `threshold daemon status` — verify health info displayed
3. Stop daemon: `threshold daemon stop` — verify clean shutdown, PID file removed
4. Restart (standalone): `threshold daemon restart` — verify rebuild, restart, health check
5. Restart with follow-on: `threshold daemon restart --follow-on-conversation <id> --follow-on-prompt "test"` — verify hook processed after restart, conversation receives message
6. Install launchd: `threshold daemon install` — verify plist created
7. Wrapper test: run `scripts/threshold-wrapper.sh`, trigger restart via `threshold daemon restart`, verify wrapper rebuilds and restarts
8. Wrapper stop: run `threshold daemon stop` under wrapper, verify wrapper exits (stop sentinel)
9. Reboot test: restart machine, verify daemon starts automatically
10. Agent restart: in a conversation, agent runs `threshold daemon restart --follow-on-conversation <id> --follow-on-prompt "Verifying..."` — verify daemon restarts and conversation resumes
11. Uninstall: `threshold daemon uninstall` — verify plist removed

---

## Files Affected (Summary)

| File | Action | Phase |
|------|--------|-------|
| `crates/server/src/main.rs` | PID file, SIGTERM, DaemonAction enum, status/stop/restart/install/uninstall commands, restart hook processing, health state creation | 16A, 16B, 16C, 16D |
| `crates/core/src/error.rs` | Add `DaemonAlreadyRunning { pid: u32 }` variant | 16A |
| `crates/core/src/types.rs` | Add `RestartHook` struct, add `HealthConfig` struct | 16B, 16C |
| `crates/scheduler/src/daemon_api.rs` | Add `Health` command, accept `HealthConfig`, compute task counts dynamically | 16B |
| `crates/scheduler/src/task.rs` | Add `one_shot: bool` field (`#[serde(default)]`) | 16C |
| `crates/scheduler/src/engine.rs` | One-shot task auto-deletion after execution | 16C |
| `crates/server/src/daemon_client.rs` | Add `send_health_check()` | 16B |
| `scripts/threshold-wrapper.sh` | New: restart loop wrapper script with stop sentinel, supervised marker, rebuild support | 16D |
| `Cargo.toml` (server) | Add `libc` dependency | 16A |

---

## Resolved Design Questions

1. **Who restarts the daemon after the agent triggers a restart?** — In standalone mode, the `threshold daemon restart` CLI command handles the full cycle (stop, build, start, health check). In supervised mode (wrapper/launchd), the CLI writes `restart-pending.json` and sends SIGTERM; the wrapper script handles the rebuild and restart. The CLI detects supervised mode via a `$DATA_DIR/state/supervised` marker file.

2. **How does the agent maintain continuity through a restart?** — Restart follow-on hooks. The agent passes `--follow-on-conversation` and `--follow-on-prompt` to the restart command. The hook is written to disk before the daemon is stopped. On startup, the new daemon reads the hooks and calls `engine.send_to_conversation()` directly — no scheduler dependency required.

3. **Why not use the scheduler for follow-on hooks?** — The scheduler is optional (can be disabled in config). Follow-on hooks must work regardless. Processing hooks directly via `ConversationEngine::send_to_conversation()` is simpler and always available.

4. **Why not a built-in `restart_daemon` tool?** — The `ToolRegistry` is not wired into the conversation engine's streaming pipeline. The engine prepends a tool prompt to the system prompt and streams Claude CLI output. The agent invokes CLI subcommands via shell execution — `threshold daemon restart` follows this established pattern, consistent with `threshold schedule`, `threshold gmail`, etc.

5. **How do `stop` and `restart` interact with the wrapper script?** — `threshold daemon stop` writes a `stop-sentinel` file before sending SIGTERM. The wrapper checks for this sentinel on each loop iteration and exits instead of restarting. `threshold daemon restart` writes `restart-pending.json` (with optional `skip_build`) instead of the stop sentinel, so the wrapper rebuilds and restarts.

6. **What if the rebuild fails?** — In standalone mode, the restart command reports the build failure and does not start the daemon. The agent sees the error in the command output and can fix the issue. In supervised mode, the wrapper logs the build failure and starts the daemon with the existing (old) binary. The follow-on hook still fires, giving the agent context to investigate.

7. **What prevents infinite restart loops?** — The wrapper script has no built-in loop limit, but restart loops only happen on crashes (non-zero exit). A 5-second delay is added between crash restarts. The stop sentinel provides a reliable way to break out of the loop. A future enhancement could add a crash counter and exponential backoff.

8. **How does the agent know its own conversation ID?** — The conversation ID is part of the agent's context. The `memory.md` path (`$DATA_DIR/conversations/{id}/memory.md`) is visible to the agent, and the conversation ID can also be inferred from environment or system prompt injection.

9. **What about Windows/Linux?** — The PID file, health check, restart command, follow-on hooks, and wrapper script are platform-agnostic (or trivially portable). Only Phase 16D's launchd integration is macOS-specific. Linux systemd support would use the same patterns: PID file, wrapper script as ExecStart, and `systemctl restart` integration.

10. **Why a wrapper script instead of pure launchd KeepAlive?** — launchd's `KeepAlive` would restart the daemon immediately, but it can't run `cargo build` first. The wrapper checks `restart-pending.json` and conditionally rebuilds before restarting. It also handles the stop sentinel logic.

11. **How are health check fields scoped?** — `HealthConfig` stores static fields set at startup: PID, start time, version. Scheduler task counts are computed dynamically per health request by calling `list_tasks()` on the `SchedulerHandle`, so they always reflect the current state. Fields requiring cross-crate queries (Discord connection status, active conversation count) are deferred to a future milestone to avoid coupling the daemon API to every subsystem.
