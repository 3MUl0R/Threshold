# Milestone 16 — Daemon Management & Self-Update

**Crates:** `core`, `server`, `scheduler`, `conversation`, `cli-wrapper`, `discord`, `web`
**Complexity:** Medium-High
**Dependencies:** Milestone 1 (daemon), Milestone 6 (scheduler), Milestone 14 (streaming/broadcast)

## What This Milestone Delivers

Infrastructure for agents to rebuild, restart, and manage the Threshold daemon — enabling autonomous self-improvement without human intervention. The system is designed for a "run from source" deployment model where the full source repository is always available and agents can modify code, recompile, and restart the running system.

1. **PID file & daemon discovery** — The daemon writes a PID file on startup, enabling other processes to find and signal the running instance. A `threshold daemon status` command reports whether the daemon is running and its health.
2. **Health check endpoint** — The daemon API gains a `Health` command returning uptime, version, and readiness via the existing Unix socket protocol. The web `/status` endpoint already provides HTTP-level health; this extends the socket API for CLI-level checks.
3. **Graceful restart with drain** — `threshold daemon restart` drains in-flight work (active agent conversations finish cleanly), then orchestrates a full stop → build → start cycle. In supervised mode (wrapper/launchd), it delegates the restart to the supervisor. In standalone mode, it handles the full lifecycle directly. A configurable drain timeout ensures restart won't hang forever.
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
             --follow-on-prompt "Restart complete. Verify the changes took effect."

threshold daemon restart (CLI process, outside the daemon):
  ├── 1. Acquire restart lock (flock $DATA_DIR/state/restart.lock)
  ├── 2. Read PID from $DATA_DIR/threshold.pid
  ├── 3. Run `cargo build -p threshold` from repo root
  │      └── If build FAILS → abort restart, print compiler errors, daemon untouched
  ├── 4. Send `Drain` command via Unix socket → daemon stops accepting new work
  ├── 5. Poll Health until active_work == 0 (timeout: --drain-timeout, default 120s)
  │      └── Print progress: "Draining: 2 active runs remaining..."
  ├── 6. Write follow-on hook to $DATA_DIR/state/restart-hooks.json (with drain summary)
  ├── 7. Send SIGTERM to daemon PID → remaining runs (if any) aborted during shutdown
  ├── 8. Wait for daemon process to exit (poll PID, timeout 30s)
  ├── 9. Start new daemon: {repo_root}/target/debug/threshold daemon start (absolute path)
  ├── 10. Poll Health command on Unix socket until ready (timeout 60s)
  └── 11. Print success: "Daemon restarted (PID 12345, 3 runs drained, 0 aborted)"

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
- `threshold daemon stop` drains active work, sends SIGTERM, and waits for exit. The daemon stays down.

**Supervised mode** (running under `scripts/threshold-wrapper.sh` or launchd):
- `threshold daemon restart` writes `$DATA_DIR/state/restart-pending.json`, then sends SIGTERM.
- The wrapper detects `restart-pending.json`, optionally rebuilds, and starts the new daemon.
- `threshold daemon stop` writes a `$DATA_DIR/state/stop-sentinel` file, then sends SIGTERM.
- The wrapper detects the stop sentinel and exits its loop instead of restarting.
- Detection: the wrapper writes a `$DATA_DIR/state/supervised` marker file containing its PID and process start time (e.g., `{"wrapper_pid": 12345, "started_at": "2026-02-25T12:00:00Z"}`). The restart command validates the marker with three checks: (1) `kill(pid, 0)` — is the PID alive? (2) Process name check via `sysctl` — is it a bash/sh process? (3) **Process start time check** — query the process's actual start time via `sysctl(KERN_PROC)` on macOS and compare it against the recorded `started_at`. If the times differ by more than 2 seconds, the PID was recycled. All three checks must pass; if any fails, the marker is stale — the CLI deletes it and proceeds in standalone mode. This three-way validation prevents PID reuse from causing false supervised-mode detection.

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

**Decoupling from scheduler:** Currently, the `DaemonApi` is created and spawned inside the scheduler task in `main.rs` — if the scheduler is disabled (`scheduler_instance` is `None`), the entire block early-returns, and the Unix socket never opens. This means health check, status, and restart's health polling all break when the scheduler is disabled.

**Fix:** The `DaemonApi` must be spawned as an independent top-level task in the `tokio::select!` block, not nested inside the scheduler task. The `SchedulerHandle` becomes `Option<SchedulerHandle>`:

- When the scheduler is enabled, the `DaemonApi` receives `Some(handle)` and scheduler commands work normally.
- When the scheduler is disabled, the `DaemonApi` receives `None` and scheduler commands (`ScheduleCreate`, `ScheduleList`, etc.) return an error response: `{"status": "error", "code": "scheduler_disabled", "message": "Scheduler is not enabled in this configuration"}`.
- The `Health` command always works regardless, returning `scheduler_task_count: null` when the scheduler is disabled.

This ensures the Unix socket is always available for health checks, status queries, and restart health polling.

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

The `DaemonApi` constructor gains a `HealthConfig` parameter and an `Option<SchedulerHandle>`. Static health fields never change after startup. Scheduler task counts are computed dynamically per health request by calling `scheduler.list_tasks()` when available. Fields that require cross-crate queries (conversation count, Discord status) are deferred to a future milestone — this keeps the health check lightweight and avoids coupling the daemon API to the conversation engine or Discord crate.

Health response uses the existing `DaemonResponse` envelope (`daemon_api.rs:42`), with health payload in the `data` field:

```json
{
    "version": 1,
    "status": "ok",
    "data": {
        "pid": 12345,
        "uptime_secs": 3600,
        "version": "0.1.0",
        "draining": false,
        "active_work": 2,
        "scheduler_task_count": 8,
        "scheduler_enabled_count": 3
    }
}
```

When the scheduler is disabled, the task count fields are `null`:

```json
{
    "version": 1,
    "status": "ok",
    "data": {
        "pid": 12345,
        "uptime_secs": 3600,
        "version": "0.1.0",
        "draining": false,
        "active_work": 0,
        "scheduler_task_count": null,
        "scheduler_enabled_count": null
    }
}
```

During drain, Health reflects the draining state and remaining active runs:

```json
{
    "version": 1,
    "status": "ok",
    "data": {
        "pid": 12345,
        "uptime_secs": 3600,
        "version": "0.1.0",
        "draining": true,
        "active_work": 1,
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
  Active:    2 runs
  Scheduler: 8 tasks (3 enabled)
```

When draining (restart/stop in progress):

```
$ threshold daemon status
Threshold Daemon
  Status:    Draining (1 active run)
  PID:       12345
  Uptime:    2h 30m 15s
  Version:   0.1.0
  Scheduler: 8 tasks (3 enabled)
```

When the scheduler is disabled, the status output reflects this:

```
$ threshold daemon status
Threshold Daemon
  Status:    Running
  PID:       12345
  Uptime:    2h 30m 15s
  Version:   0.1.0
  Active:    0 runs
  Scheduler: disabled
```

Implementation: connects to the Unix socket at `$DATA_DIR/threshold.sock`, sends `Health`, and formats the response. When health JSON has `scheduler_task_count: null`, display "Scheduler: disabled" instead of task counts. If the socket doesn't exist or connection fails, falls back to PID file check:

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

### Graceful Drain Before Shutdown

When restart or stop is triggered, in-flight work (active agent conversations, scheduled task executions) must be given time to complete rather than being abruptly terminated. The daemon enters a **draining** state before SIGTERM is sent.

**Shared drain state:**

```rust
// crates/core/src/types.rs
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Debug)]
pub struct DaemonState {
    /// True when the daemon is preparing to shut down.
    draining: AtomicBool,
    /// Number of active work items (conversations, script tasks, etc.).
    active_work: AtomicU32,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            draining: AtomicBool::new(false),
            active_work: AtomicU32::new(0),
        }
    }
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }
    pub fn set_draining(&self, value: bool) {
        self.draining.store(value, Ordering::Release);
    }
    pub fn active_work(&self) -> u32 {
        self.active_work.load(Ordering::Acquire)
    }
    pub fn increment_work(&self) -> u32 {
        self.active_work.fetch_add(1, Ordering::AcqRel) + 1
    }
    pub fn decrement_work(&self) -> u32 {
        let prev = self.active_work.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |n| Some(n.saturating_sub(1)),
        ).unwrap(); // fetch_update with Some always succeeds
        debug_assert!(prev > 0, "active_work underflow: double-decrement bug");
        prev.saturating_sub(1)
    }
}
```

An `Arc<DaemonState>` is created at daemon startup and passed to all subsystems.

**Active work tracking:** `DaemonState.active_work` is a unified counter for ALL in-flight work — not just streaming Claude runs. `ProcessTracker` only tracks streaming Claude CLI processes (for abort support), which misses scheduler Script tasks, NewConversation (non-streaming path), and ScriptThenConversation. Instead, `active_work` is incremented/decremented at each work entry point:

| Entry point | Tracks |
|-------------|--------|
| `ConversationEngine::handle_message()` | User-initiated streaming conversations (Discord messages, Web sends) |
| `ConversationEngine::send_to_conversation()` | Scheduler `ResumeConversation` tasks, restart follow-on hooks |
| `scheduler::exec_script()` | Shell script tasks |
| `scheduler::exec_new_conversation()` | New conversation tasks (non-streaming path) |
| `scheduler::exec_script_then_conversation()` | Composite script → conversation tasks |

**Important:** `handle_message()` and `send_to_conversation()` are **separate streaming paths** — `send_to_conversation()` does NOT call `handle_message()`. Both independently manage locks, streaming, and process tracking. Both need their own `WorkGuard` for `active_work` tracking. `ResumeConversation` tasks go through `exec_resume_conversation()` → `engine.send_to_conversation()`, so they're tracked by the `send_to_conversation()` entry point — no double-counting with scheduler-level tracking (the scheduler does NOT separately instrument `exec_resume_conversation()` since the engine handles it).

`ProcessTracker` is retained for its original purpose (abort support via `/abort`), but `DaemonState.active_work` is the single source of truth for drain completion.

**Implementation note:** Each tracking site must guarantee `decrement_work()` runs on ALL exit paths — success, error, panic, and cancellation. Use a RAII drop guard pattern:

```rust
struct WorkGuard(Arc<DaemonState>);
impl Drop for WorkGuard {
    fn drop(&mut self) { self.0.decrement_work(); }
}

// Usage in handle_message(), send_to_conversation(), exec_script(), etc.:
let _guard = WorkGuard(daemon_state.clone());
daemon_state.increment_work();
// ... do work — guard ensures decrement on any exit path
```

This prevents `active_work` from leaking on error or panic paths, which would cause drain to hang indefinitely.

**Subsystem behavior during drain:**

| Subsystem | Drain behavior |
|-----------|---------------|
| **ConversationEngine** | Checks `is_draining()` before accepting new work in both `handle_message()` and `send_to_conversation()`. Returns `DaemonDraining` error if draining. In-flight conversations (already streaming) continue to completion. |
| **Scheduler** | Checks `is_draining()` before firing tasks in `run_ready_tasks()`. If draining, skips the task — it will fire on its next cron match after restart. |
| **Discord bot** | Checks `is_draining()` before processing incoming messages. If draining, replies: "Threshold is restarting. Your message will not be processed — please retry in a moment." |
| **Web server** | Checks `is_draining()` on action requests (conversation sends, schedule changes). Returns 503 Service Unavailable: "Daemon is restarting." Read-only pages (status, conversation list) continue working. |

**New `DaemonCommand::Drain`:**

```rust
pub enum DaemonCommand {
    // ... existing ...
    Health,
    /// Enter drain mode: stop accepting new work, let in-flight work finish.
    Drain,
    /// Exit drain mode: resume accepting new work. Used for rollback if
    /// restart/stop fails after Drain but before SIGTERM.
    Undrain,
}
```

The `Drain` command sets `DaemonState.draining = true` and returns the current active work count:

```json
{
    "version": 1,
    "status": "ok",
    "data": {
        "draining": true,
        "active_work": 3
    }
}
```

After sending `Drain`, the CLI polls `Health` (which now includes `draining` and `active_work`) until `active_work == 0` or the drain timeout expires. If the timeout expires, the CLI proceeds with SIGTERM — the daemon's shutdown path cancels streaming Claude runs via `ProcessTracker` (which sends `SIGTERM` to tracked child processes) and drops the `CancellationToken`. **Important caveat:** Script tasks (`exec_script()`) spawn children via `tokio::process::Command` without registering them in `ProcessTracker`. When the daemon's Tokio runtime shuts down, the `await` on the child process is cancelled, but the script's child process may continue running as an orphan (reparented to PID 1). This is acceptable because: (1) the drain phase gives scripts time to finish naturally, (2) orphaned scripts are short-lived cron-like jobs (not long-running daemons), and (3) adding a process group kill would add complexity for an edge case that rarely triggers. The drain summary's "aborted" count reflects tasks still tracked by `active_work` at SIGTERM time — it's a best-effort count since Script tasks may actually complete on their own after the daemon exits.

**Drain rollback:** If the restart or stop command fails *after* sending `Drain` but *before* sending SIGTERM (e.g., hook write failure, signal failure), the CLI performs a full rollback:

1. **Clean up control files** — Remove any files written during the failed attempt: `restart-hooks.json` (if written by this attempt), `restart-pending.json` (if written by this attempt), `stop-sentinel` (if written by this attempt). Without this, stale files could trigger unintended behavior: the wrapper could rebuild on next crash restart, or a follow-on hook could fire into the wrong context, or a stop sentinel could cause the wrapper to exit unexpectedly.
2. **Send `Undrain`** — Restore `DaemonState.draining = false` so the daemon resumes accepting new work. Without this, a failed restart would leave the daemon alive but permanently rejecting new work.

The `Undrain` command is a no-op if the daemon isn't draining. The CLI wraps the post-drain steps in error handling that performs both cleanup steps on any failure. Implementation: the CLI tracks which files it wrote (via a `Vec<PathBuf>` or similar) and removes them in the error handler before sending `Undrain`.

**Drain timeout defaults:** 120s for restart (agents often take 30-60s per run), 30s for stop. Configurable via `--drain-timeout <secs>`.

**Socket-down fallback:** If the Unix socket is unreachable (daemon API crashed, socket file missing, connection refused) but the process is alive (PID check passes), the CLI skips the drain phase entirely and proceeds directly to SIGTERM with a warning: "Warning: Could not connect to daemon socket. Skipping drain — active work may be interrupted." This handles the edge case where the daemon is alive but its socket listener is broken. The status command already has a socket-failure fallback (PID file check); stop and restart now follow the same pattern for the drain step.

**Drain summary:** The CLI tracks drain progress and computes a summary: `initial_active_work` (from Drain response) minus `final_active_work` (from last Health poll before SIGTERM) = finished; `final_active_work` = aborted. This summary is prepended to the follow-on prompt (if any) and printed in the CLI output.

**DaemonApi access to DaemonState:** The `DaemonApi` gains an `Arc<DaemonState>` parameter (in addition to `HealthConfig` and `Option<SchedulerHandle>`). It reads `daemon_state.active_work()` for the `active_work` field in Health responses, and `daemon_state.is_draining()` for the `draining` field. Both are lock-free atomic reads — no contention.

### Restart Command (Standalone Mode)

```
threshold daemon restart [--skip-build] [--data-dir <path>] \
    [--drain-timeout <secs>] \
    [--follow-on-conversation <uuid> --follow-on-prompt <text>]
```

**Critical safety invariant:** The build must succeed *before* the running daemon is stopped. If the build fails, the restart is aborted and the running daemon continues undisturbed. This prevents the failure mode where the daemon is stopped, the rebuild fails, and no daemon can be started — leaving the system dead.

**Restart lock:** The restart command acquires an exclusive `flock()` on `$DATA_DIR/state/restart.lock` before doing anything. This serializes concurrent restart attempts — if two agents or users invoke `threshold daemon restart` simultaneously, the second one blocks until the first completes (or fails with a timeout). The lock is held for the entire restart orchestration (build + stop + start + health check) and released automatically when the CLI process exits. The same lock is acquired by `threshold daemon stop` to prevent stop and restart from racing.

**Steps:**

1. **Resolve data dir** — canonical chain: `--data-dir` → `THRESHOLD_DATA_DIR` → `THRESHOLD_CONFIG` (config file) → `~/.threshold`
2. **Acquire restart lock** — `flock()` on `$DATA_DIR/state/restart.lock` (blocking, 60s timeout). If the lock cannot be acquired, error: "Another restart or stop operation is in progress."
3. **Verify daemon is running** — Read PID file, check process alive, validate it's a Threshold process (same `is_threshold_process()` check used at startup). Error if PID is stale or belongs to a different process.
4. **Build first** (unless `--skip-build`) — Run `cargo build -p threshold` from the repository root. If the build fails, **abort the restart immediately** and return the compiler error output. The running daemon is never touched.
5. **Detect supervised mode** — Check for `$DATA_DIR/state/supervised` marker. If present, delegate to supervised restart (see below).
6. **Drain active work** — Send `Drain` command via Unix socket. The daemon enters drain mode (stops accepting new work). Record `initial_active_work` from the response.
7. **Wait for drain** — Poll `Health` until `active_work == 0` (timeout: `--drain-timeout`, default 120s). Print progress: "Draining: 2 active runs remaining..." If timeout expires, record `final_active_work` and proceed — remaining runs will be aborted during shutdown.
8. **Write follow-on hook** (if provided) — Atomic write to `$DATA_DIR/state/restart-hooks.json` (write to temp file, then rename). If runs were active, prepend drain summary to follow-on prompt. The restart lock guarantees no concurrent writer.
9. **Send SIGTERM** — `kill(pid, SIGTERM)` (only after identity validation in step 3). The daemon's shutdown path cancels streaming Claude runs via `ProcessTracker` and drops the `CancellationToken`. Script task children not tracked by `ProcessTracker` may outlive the daemon as orphans (see Drain section caveats).
10. **Wait for exit** — Poll process existence, timeout after 30 seconds
11. **Start new daemon** — Spawn the freshly built binary as a detached process using its **absolute path** (`{repo_root}/target/debug/threshold daemon start`), not a bare `threshold` that would resolve via PATH. The repo root is determined by `find_repo_root()`, which uses two strategies: (1) walk up from `cwd` looking for `Cargo.toml` with `[workspace]`, (2) if that fails, resolve the running binary's path via `std::env::current_exe()` and walk up from there (the binary is at `{repo_root}/target/debug/threshold`). This ensures restart works even when invoked from outside the repo directory.
12. **Wait for healthy** — Poll `Health` command via Unix socket, timeout after 60 seconds
13. **Report success** — Print new PID, build time, drain summary (e.g., "3 runs drained, 0 aborted"), startup time

### Restart Command (Supervised Mode)

The same safety invariant applies: the build happens in the CLI *before* SIGTERM is sent. The wrapper does not need to rebuild because the CLI already produced a clean binary.

When the supervised marker is detected:

1. Build already succeeded in step 4 above (before mode detection)
2. **Drain active work** — Send `Drain` command via Unix socket. Record `initial_active_work`.
3. **Wait for drain** — Poll `Health` until `active_work == 0` (timeout: `--drain-timeout`, default 120s). Print progress. If timeout expires, proceed — streaming runs cancelled, script children may orphan.
4. Write `restart-pending.json` to `$DATA_DIR/state/` (atomic write) — with `skip_build: true` since the CLI already built successfully
5. Write follow-on hook (if provided, with drain summary prepended)
6. Send SIGTERM to daemon PID
7. Print: "Restart signal sent (build succeeded, N runs drained, M aborted). The supervisor will restart the daemon."
8. Exit — the wrapper starts the new binary without rebuilding

### Stop Command

```
threshold daemon stop [--data-dir <path>] [--drain-timeout <secs>]
```

1. Acquire restart lock (`flock()` on `$DATA_DIR/state/restart.lock`, same as restart command)
2. Read PID file, validate process identity (same `is_threshold_process()` check)
3. **Drain active work** — Send `Drain` command via Unix socket. Record `initial_active_work`.
4. **Wait for drain** — Poll `Health` until `active_work == 0` (timeout: `--drain-timeout`, default 30s). Print progress. If timeout expires, proceed — streaming Claude runs are cancelled via `ProcessTracker`; script task children may outlive the daemon as orphans.
5. If supervised mode: write `$DATA_DIR/state/stop-sentinel` file
6. Send SIGTERM (only after identity validation)
7. Wait for process exit (timeout 30s)
8. Print: "Daemon stopped. (N runs drained, M aborted)" — or just "Daemon stopped." if no runs were active.

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
    /// Prompt to inject into the conversation (drain summary is prepended by the CLI
    /// before writing, so this field contains the full prompt including summary).
    pub prompt: String,
    /// When the hook was created.
    pub created_at: DateTime<Utc>,
    /// Who requested the restart (for audit trail).
    pub requested_by: Option<String>,
    /// Drain statistics at the time of restart, if any work was active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_summary: Option<DrainSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainSummary {
    /// Number of active work items that completed during drain.
    pub finished: u32,
    /// Number of active work items aborted (drain timeout expired).
    pub aborted: u32,
}
```

### File Robustness

All hook/pending file writes use atomic write-temp-then-rename. The temp file uses a unique suffix (PID + timestamp) to avoid collisions if two processes write simultaneously. Note: the rename itself is atomic on POSIX, but two concurrent writers performing read-modify-write could still lose data. In practice this is a non-issue — only one restart can be in flight at a time (there's only one daemon PID to signal). If a future use case requires concurrent hook writers, add `flock()`-based advisory locking.

```rust
fn write_hooks_atomic(path: &Path, hooks: &[RestartHook]) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to temp file with unique suffix (PID + timestamp), then rename for atomicity.
    // Unique suffix prevents concurrent writers from clobbering each other's temp file.
    let suffix = format!("{}.{}", std::process::id(), chrono::Utc::now().timestamp_millis());
    let tmp_path = path.with_extension(format!("tmp.{}", suffix));
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
            "Some restart hooks failed — scheduling background retry"
        );

        // Retry failed hooks with exponential backoff (5s, 10s, 20s — 3 attempts max).
        // If all retries fail, the hooks remain on disk for the next startup.
        let engine = engine.clone();
        let hooks_path = hooks_path.clone();
        tokio::spawn(async move {
            let delays = [5, 10, 20];
            for (attempt, delay_secs) in delays.iter().enumerate() {
                tokio::time::sleep(std::time::Duration::from_secs(*delay_secs)).await;
                let raw = match std::fs::read_to_string(&hooks_path) {
                    Ok(r) => r,
                    Err(_) => return, // File removed (maybe processed by another path)
                };
                let hooks: Vec<RestartHook> = match serde_json::from_str(&raw) {
                    Ok(h) => h,
                    Err(_) => return,
                };
                if hooks.is_empty() { return; }

                let mut still_failed = Vec::new();
                for hook in hooks {
                    if engine.send_to_conversation(&hook.conversation_id, &hook.prompt).await.is_err() {
                        still_failed.push(hook);
                    }
                }

                if still_failed.is_empty() {
                    std::fs::remove_file(&hooks_path).ok();
                    tracing::info!("All restart hooks delivered on retry attempt {}", attempt + 1);
                    return;
                } else {
                    write_hooks_atomic(&hooks_path, &still_failed).ok();
                }
            }
            tracing::warn!("Restart hooks still pending after all retries — will retry on next startup");
        });
    }
    Ok(())
}
```

### One-Shot Scheduled Tasks (Independent Enhancement — Deferrable)

As a standalone improvement in this milestone, the scheduler gains a `one_shot` field for tasks that should auto-delete after first execution. This is **not** used by the follow-on hook system (which processes hooks directly via the conversation engine). It's a general-purpose scheduler enhancement that enables "run once at next opportunity" patterns for future use cases.

**Scope note:** This is fully independent of the daemon management objective. It can be deferred to a later milestone without affecting any other Phase 16 deliverable. It's included here because it's small (~15 lines of code) and was identified during the design of the follow-on system.

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

BINARY="$REPO_ROOT/target/debug/threshold"

# Initial build if binary doesn't exist (first boot, after cargo clean, etc.)
if [ ! -f "$BINARY" ]; then
    echo "[wrapper] Binary not found at $BINARY. Building from source..."
    (cd "$REPO_ROOT" && cargo build -p threshold) || {
        echo "[wrapper] Initial build failed. Cannot start daemon. Exiting."
        exit 1
    }
fi

while true; do
    # Check for stop sentinel — exit loop instead of restarting
    if [ -f "$STATE_DIR/stop-sentinel" ]; then
        rm -f "$STATE_DIR/stop-sentinel"
        echo "[wrapper] Stop sentinel found. Exiting."
        break
    fi

    # Check if a restart was requested via CLI
    # The CLI builds BEFORE sending SIGTERM, so skip_build is normally true.
    # The wrapper retains build capability as a fallback for manual restarts
    # (e.g., someone kills the daemon directly, or the daemon crashes).
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
        else
            echo "[wrapper] Build already completed by CLI. Skipping rebuild."
        fi
    fi

    echo "[wrapper] Starting daemon..."
    EXIT_CODE=0
    "$BINARY" daemon start || EXIT_CODE=$?

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

# Exit 0 = intentional stop (launchd won't restart).
# Exit 1 = unexpected failure (launchd will restart via KeepAlive/SuccessfulExit=false).
echo "[wrapper] Wrapper exiting."
exit 0
```

### Restart Pending File

Written by `threshold daemon restart` in supervised mode. Since the CLI builds *before* signaling, `skip_build` is normally `true`:

```json
{
    "skip_build": true,
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
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <!-- KeepAlive/SuccessfulExit=false: launchd restarts the wrapper only if it
         exits with a non-zero status (unexpected crash). When the wrapper exits
         cleanly via stop sentinel (exit 0), launchd leaves it down. This prevents
         a wrapper crash from permanently killing the service. -->

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
threshold daemon stop [--data-dir <path>]           # Drain + SIGTERM, wait for exit
                      [--drain-timeout <secs>]
threshold daemon restart [--data-dir <path>]        # Drain + stop + optional build + start
                         [--skip-build]
                         [--drain-timeout <secs>]
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
        /// Path to threshold config file
        #[arg(short, long)]
        config: Option<String>,
    },
    /// Stop the running daemon gracefully. Drains active runs before
    /// sending SIGTERM — in-flight conversations finish cleanly.
    Stop {
        /// Data directory (default: $THRESHOLD_DATA_DIR / $THRESHOLD_CONFIG / ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
        /// Max seconds to wait for active runs to finish before forcing
        /// shutdown (default: 30)
        #[arg(long, default_value = "30")]
        drain_timeout: u64,
    },
    /// Rebuild from source, then restart the daemon. Build runs BEFORE
    /// shutdown — if it fails, the running daemon is untouched. Active
    /// runs are drained before SIGTERM. Use --follow-on-conversation
    /// and --follow-on-prompt to resume an agent conversation after restart.
    Restart {
        /// Data directory (default: $THRESHOLD_DATA_DIR / $THRESHOLD_CONFIG / ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
        /// Skip cargo build (restart with existing binary)
        #[arg(long)]
        skip_build: bool,
        /// Max seconds to wait for active runs to finish before forcing
        /// shutdown (default: 120)
        #[arg(long, default_value = "120")]
        drain_timeout: u64,
        /// Conversation ID to receive the follow-on prompt after restart
        #[arg(long, requires = "follow_on_prompt")]
        follow_on_conversation: Option<String>,
        /// Prompt to inject into the conversation after restart (e.g.,
        /// "Restart complete. Verify the fix took effect.")
        #[arg(long, requires = "follow_on_conversation")]
        follow_on_prompt: Option<String>,
    },
    /// Show daemon status: PID, uptime, version, scheduler task counts
    Status {
        /// Data directory (default: $THRESHOLD_DATA_DIR / $THRESHOLD_CONFIG / ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// Install as a macOS launchd service for auto-start on boot
    Install {
        /// Data directory (default: $THRESHOLD_DATA_DIR / $THRESHOLD_CONFIG / ~/.threshold)
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// Remove the launchd service (daemon will no longer auto-start)
    Uninstall,
}
```

All management actions (`stop`, `restart`, `status`, `install`) accept `--data-dir` for non-default data directory locations. The PID file and socket path are derived from the data dir.

**Canonical data-dir resolution order** (used by all management commands and `DaemonClient`):

1. `--data-dir` CLI flag (explicit override)
2. `THRESHOLD_DATA_DIR` env var (set by daemon on startup, inherited by child processes)
3. `THRESHOLD_CONFIG` env var → load config file → read `data_dir` field (fallback for separate shells)
4. `~/.threshold` default

This single chain is used everywhere: `resolve_data_dir()` helper, `DaemonClient` socket path, Phase 16B status, Phase 16C stop/restart. Management subcommands do not expose a `--config` flag — the `THRESHOLD_CONFIG` env var is checked automatically as step 3 in the resolution chain.

**Data dir consistency:** The `daemon start` command resolves data_dir from `ThresholdConfig.data_dir()` (config file field → `~/.threshold`). On startup, the daemon **exports both `THRESHOLD_DATA_DIR` and `THRESHOLD_CONFIG`** as environment variables so child processes (including CLI commands spawned by agents) inherit the correct paths — steps 2 and 3 above.

The env vars only propagate to child processes — they don't help a user running `threshold daemon status` from a separate shell. For that case, the user either: (a) relies on the `~/.threshold` default (step 4), (b) passes `--data-dir` explicitly (step 1), or (c) sets `THRESHOLD_DATA_DIR` or `THRESHOLD_CONFIG` in their shell profile (steps 2/3).

In practice, the common cases are: (a) default `~/.threshold` — everything works with no flags, (b) agent-spawned commands — env vars inherited, (c) manual commands with custom data dir — user passes `--data-dir` or has env vars in their shell profile.

**Help flags:** Clap auto-generates `-h` and `--help` for every subcommand. This is the primary discovery mechanism for agents — they don't need to memorize the CLI interface. An agent can run `threshold --help` to see all top-level commands, `threshold daemon --help` to see all daemon actions, and `threshold daemon restart --help` to see restart-specific flags. The `/// doc comments` on the `DaemonAction` variants and `#[arg(...)]` fields become the help text, so they should be written as clear, concise descriptions suitable for both human and agent consumption.

**Backward compatibility:** To avoid breaking existing `threshold daemon --config <path>` invocations, `Start` is the default subcommand. If no action is given, it behaves as `threshold daemon start`.

---

## Agent Restart Flow

The agent triggers a restart by running shell commands — the same way it uses `threshold schedule`, `threshold gmail`, and other CLI subcommands:

```
Agent (in a Claude CLI conversation):
  1. Makes code changes (edit files, run tests)
  2. Runs: threshold daemon restart \
       --follow-on-conversation <this-conversation-id> \
       --follow-on-prompt "Restart complete. Verify the fix took effect."

  Standalone mode:
    3. The CLI builds first — if build fails, returns error and daemon stays running
    4. On build success: CLI drains active work (other agents' runs finish)
    5. CLI stops daemon, starts new binary, waits for health check
    6. Returns: "Daemon restarted (PID 12345, build: 8.2s, 2 runs drained, startup: 1.3s)"

  Supervised mode:
    3. The CLI builds first — if build fails, returns error and daemon stays running
    4. On build success: CLI drains active work, then writes restart-pending.json and sends SIGTERM
    5. Returns: "Restart signal sent (build succeeded, 2 runs drained). The supervisor will restart."
    6. The wrapper starts the new binary without rebuilding

After restart (both modes):
  - New daemon processes the follow-on hook
  - Agent's conversation receives the follow-on prompt (with drain summary if runs were active)
  - Agent picks up: "2 runs drained, 0 aborted. Restart confirmed. Running tests to verify..."
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
| `crates/core/src/types.rs` | Add `HealthConfig` struct (pid, started_at, version). Add `DaemonState` struct (draining: AtomicBool, active_work: AtomicU32). |
| `crates/scheduler/src/daemon_api.rs` | Add `DaemonCommand::Health`, `DaemonCommand::Drain`, and `DaemonCommand::Undrain`. Change `scheduler` field to `Option<SchedulerHandle>`. Accept `HealthConfig` and `Arc<DaemonState>` in `DaemonApi::new()`. Compute task counts dynamically via `list_tasks()` when scheduler is `Some`. Include `draining` and `active_work` in Health response. `Drain` sets `DaemonState.draining = true`, `Undrain` resets to false. Return `scheduler_disabled` error for schedule commands when `None`. |
| `crates/server/src/main.rs` | **Decouple DaemonApi from scheduler task** — spawn DaemonApi as independent top-level task in `tokio::select!`, not nested inside the scheduler block. Pass `Some(scheduler_handle)` when scheduler is enabled, `None` when disabled. Create `HealthConfig` and `Arc<DaemonState>` at startup, pass to `DaemonApi`. Pass `Arc<DaemonState>` to ConversationEngine, Scheduler, Discord, and Web for drain checks and active_work tracking. Restructure `Commands::Daemon` to use `DaemonAction` subcommand enum. Implement `DaemonAction::Status` — connect to socket, send `Health`, format output (including draining state and active work). **Export `THRESHOLD_DATA_DIR` and `THRESHOLD_CONFIG`** env vars on startup for child process consistency. |
| `crates/server/src/daemon_client.rs` | Accept configurable socket path (canonical resolution chain: `--data-dir` → `THRESHOLD_DATA_DIR` → `THRESHOLD_CONFIG` → `~/.threshold`). Add `send_health_check()` method — connects to socket, sends `Health` command, returns parsed `DaemonResponse` with health JSON payload. Add `send_drain()` method — sends `Drain` command, returns response with initial active_work count. Add `send_undrain()` method — sends `Undrain` to roll back drain on failure. |
| `crates/conversation/src/engine.rs` | Accept `Option<Arc<DaemonState>>` in constructor. Check `is_draining()` at start of both `handle_message()` and `send_to_conversation()` — return `DaemonDraining` error if draining. In-flight conversations (already streaming) continue to completion. Increment/decrement `DaemonState.active_work` via `WorkGuard` in both `handle_message()` and `send_to_conversation()` (these are separate streaming paths — both need independent tracking). |
| `crates/scheduler/src/engine.rs` | Accept `Option<Arc<DaemonState>>` in `Scheduler::new()`. Check `is_draining()` in `run_ready_tasks()` — skip firing if draining. |
| `crates/scheduler/src/execution.rs` | Increment/decrement `DaemonState.active_work` around `exec_script()`, `exec_new_conversation()`, and `exec_script_then_conversation()` — these don't go through ConversationEngine so need their own tracking. (`exec_resume_conversation()` delegates to `engine.send_to_conversation()` which has its own `WorkGuard` — no scheduler-level tracking needed for that path.) |
| `crates/discord/src/bot.rs` | Accept `Option<Arc<DaemonState>>`. Check `is_draining()` before processing incoming messages. If draining, reply: "Threshold is restarting. Your message will not be processed — please retry in a moment." |
| `crates/web/src/routes.rs` (or equivalent) | Accept `Arc<DaemonState>` in app state. Check `is_draining()` on action requests (conversation sends, schedule changes). Return 503: "Daemon is restarting." Read-only pages continue working. |
| `crates/core/src/error.rs` | Add `DaemonDraining` error variant for rejected work during drain. |

**Tests:**
- `daemon_api::health_returns_uptime` — Send `Health` command to daemon API, verify response envelope includes pid, uptime, version, draining, active_work, task counts in `data` field.
- `daemon_api::health_counts_dynamic` — Add and remove tasks, verify health response reflects current counts.
- `daemon_api::health_without_scheduler` — Create `DaemonApi` with `scheduler: None`, send `Health`, verify response succeeds with `scheduler_task_count: null`. Send `ScheduleList`, verify `scheduler_disabled` error.
- `daemon_api::drain_sets_draining_flag` — Send `Drain` command, verify `DaemonState.is_draining()` returns true, verify response includes `active_work`.
- `daemon_api::health_reflects_draining` — Set draining via `Drain` command, send `Health`, verify `draining: true` in response.
- `daemon_api::undrain_restores_normal` — Send `Drain`, verify draining. Send `Undrain`, verify `is_draining()` returns false and Health shows `draining: false`.
- `daemon::restart_failure_undrains` — Start daemon, trigger restart, simulate failure after Drain but before SIGTERM (e.g., hook write error). Verify CLI sends `Undrain` and daemon resumes normal operation (accepts new work). Also verify any control files written before the failure (`restart-hooks.json`, `restart-pending.json`) are cleaned up.
- `conversation::send_rejected_during_drain` — Set `DaemonState.draining = true`, call `send_to_conversation()`, verify `DaemonDraining` error returned.
- `conversation::active_work_tracked` — Start a conversation via `handle_message()`, verify `DaemonState.active_work() > 0` during execution, verify it returns to 0 after completion.
- `scheduler::skip_firing_during_drain` — Set draining, verify `run_ready_tasks()` skips task execution.
- `scheduler::script_task_tracked` — Execute a Script task, verify `DaemonState.active_work() > 0` during execution.
- `discord::message_rejected_during_drain` — Set draining, send a Discord message, verify the bot replies with "restarting" message and does NOT start a conversation.
- `web::action_rejected_during_drain` — Set draining, POST to a conversation send endpoint, verify 503 response.
- `web::readonly_allowed_during_drain` — Set draining, GET status page, verify 200 response.
- `daemon::status_shows_running` — Start daemon, run `threshold daemon status`, verify output includes PID, uptime, version, active work, task counts.
- `daemon::status_shows_not_running` — No daemon running, run status, verify "Not running" output.
- `daemon::status_shows_scheduler_disabled` — Start daemon with scheduler disabled, run status, verify output shows "Scheduler: disabled".
- `daemon::status_shows_draining` — Start daemon, send Drain, run status, verify output shows "Status: Draining".
- `daemon_state::decrement_saturates_at_zero` — Call `decrement_work()` when `active_work == 0`, verify it stays at 0 (saturates) instead of wrapping to `u32::MAX`. In debug builds, verify the debug assertion fires.
- `conversation::active_work_decrements_on_error` — Start `handle_message()` with input that causes an error (e.g., non-existent conversation ID), verify `active_work` returns to 0 after the error. Ensures the decrement runs even on the error path.
- `scheduler::script_task_decrements_on_failure` — Execute a Script task that exits non-zero, verify `active_work` returns to 0. Ensures `decrement_work()` is in a `finally`-equivalent (drop guard or `scopeguard`).
- `scheduler::new_conversation_decrements_on_error` — Execute a NewConversation task that fails (e.g., bad model config), verify `active_work` returns to 0.
- `conversation::active_work_decrements_on_abort` — Start `handle_message()`, abort the run mid-stream via `ProcessTracker`, verify `active_work` returns to 0 after abort completes.
- `daemon_api::health_config_serde_round_trip` — Serialize and deserialize `HealthConfig`.

### Phase 16C — Stop, Restart, and Follow-On Hooks

**Goal:** `threshold daemon stop/restart` commands work in both standalone and supervised modes. Follow-on hooks provide agent continuity through restarts.

**Changes:**

| File | Change |
|------|--------|
| `crates/core/src/types.rs` | Add `RestartHook` struct (conversation_id, prompt, created_at, requested_by). Add optional `drain_summary` field to `RestartHook` (finished, aborted counts). |
| `crates/server/src/main.rs` | Implement `DaemonAction::Stop` (send `Drain`, poll Health until drained or timeout, detect supervised mode, write stop sentinel if needed, send SIGTERM, wait for exit, print drain summary). Implement `DaemonAction::Restart` (build first, send `Drain`, poll Health until drained or timeout, detect mode, write hooks atomically with drain summary, write restart-pending if supervised, stop + start if standalone, poll health). Add `resolve_data_dir()`, `find_repo_root()`, `write_hooks_atomic()`, `detect_supervised()`, `wait_for_process_exit()`, `wait_for_healthy()`, `drain_and_wait()`, `rollback_on_failure()` helpers. `rollback_on_failure()` removes any control files written during the failed attempt and sends `Undrain`. Add `process_restart_hooks()` — on startup, read hooks, process sequentially via `send_to_conversation()`, preserve failed hooks. |
| `crates/scheduler/src/task.rs` | Add `one_shot: bool` field to `ScheduledTask` (with `#[serde(default)]`). |
| `crates/scheduler/src/engine.rs` | After task completion, if `one_shot`, remove the task from `self.tasks` and persist. |

**Tests:**
- `daemon::restart_aborts_on_build_failure` — Start daemon, trigger restart with a simulated build failure (e.g., mock cargo returning non-zero), verify daemon is still running (PID unchanged, health check passes), verify CLI returned the build error.
- `daemon::restart_drains_before_sigterm` — Start daemon with an active run (mock long-running conversation), trigger restart. Verify Drain command sent, active_work polled via Health, SIGTERM sent only after active_work == 0.
- `daemon::restart_drain_timeout_proceeds` — Start daemon with an active run that never finishes, trigger restart with `--drain-timeout 2`. Verify restart proceeds after timeout, drain summary shows aborted count.
- `daemon::restart_drain_summary_in_follow_on` — Trigger restart with active runs and `--follow-on-prompt`. Verify the written hook includes drain summary prepended to the prompt.
- `daemon::stop_drains_before_sigterm` — Start daemon with active runs, run stop. Verify Drain sent and waited for before SIGTERM.
- `daemon::stop_failure_undrains` — Start daemon, trigger stop, simulate failure after Drain but before SIGTERM (e.g., signal send error). Verify CLI sends `Undrain` and daemon resumes normal operation. Also verify `stop-sentinel` (if written) is cleaned up.
- `daemon::stop_skips_drain_on_socket_failure` — Start daemon, remove socket file, trigger stop. Verify stop proceeds (skips drain, sends SIGTERM directly) with warning message.
- `daemon::restart_skips_drain_on_socket_failure` — Start daemon, remove socket file, trigger restart. Verify restart proceeds (skips drain, sends SIGTERM directly) with warning message.
- `daemon::restart_rollback_cleans_hooks` — Trigger restart with `--follow-on-prompt`, simulate failure after hook file is written but before SIGTERM (e.g., inject signal send error). Verify `restart-hooks.json` is removed and `Undrain` is sent.
- `daemon::restart_rollback_cleans_pending` — In supervised mode, trigger restart, simulate failure after `restart-pending.json` is written. Verify the file is removed and `Undrain` is sent.
- `daemon::restart_lock_serializes_concurrent` — Acquire restart lock in test, spawn `threshold daemon restart` in background, verify it blocks (doesn't send SIGTERM while lock is held), release lock, verify restart proceeds.
- `daemon::stop_sends_sigterm` — Start daemon in background, run stop, verify process exits.
- `daemon::stop_writes_sentinel_in_supervised_mode` — Set supervised marker, run stop, verify sentinel file created.
- `daemon::supervised_marker_valid` — Write supervised marker with current process PID and start time, verify `detect_supervised()` returns `true`.
- `daemon::supervised_marker_stale_pid_dead` — Write supervised marker with non-existent PID, verify `detect_supervised()` returns `false` and deletes marker.
- `daemon::supervised_marker_stale_pid_recycled` — Write supervised marker with a live PID but mismatched start time (simulating PID reuse), verify `detect_supervised()` returns `false` and deletes marker.
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
- `wrapper::stop_sentinel_exits_loop` — Integration test: create stop sentinel, verify wrapper exits with code 0.
- `wrapper::restart_pending_triggers_rebuild` — Integration test: create restart-pending.json with `skip_build: false`, verify build runs.
- `wrapper::first_boot_builds_if_binary_missing` — Integration test: remove binary, start wrapper, verify it runs `cargo build` before first daemon start.
- `wrapper::first_boot_exits_on_build_failure` — Integration test: remove binary, make build fail (e.g., invalid source), verify wrapper exits with code 1 (triggering launchd restart).
- Note: Actual launchd loading/unloading is manual-test only (requires user login session).

---

## Backward Compatibility

All new persisted fields use serde defaults:

- `ScheduledTask.one_shot: bool` — `#[serde(default)]`, defaults to `false`. Existing tasks are unaffected.
- `RestartHook` — New file, no backward compat concern.
- `HealthConfig` — Static config struct, runtime-only (not persisted across restarts), no backward compat concern.
- `DaemonState` — Runtime-only struct (`AtomicBool` + `AtomicU32`), no backward compat concern.
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
2. Check status: `threshold daemon status` — verify health info displayed (including active runs)
3. Drain test: start a conversation run, run `threshold daemon status`, verify "Active: 1 runs"
4. Stop daemon: `threshold daemon stop` — verify drain phase (waits for runs), clean shutdown, PID file removed
5. Stop with timeout: start a long-running conversation, `threshold daemon stop --drain-timeout 5` — verify it aborts after 5s and reports aborted count
6. Restart (standalone): `threshold daemon restart` — verify rebuild, drain, restart, health check
7. Restart with follow-on: `threshold daemon restart --follow-on-conversation <id> --follow-on-prompt "test"` — verify drain summary prepended to hook, hook processed after restart, conversation receives message
8. Install launchd: `threshold daemon install` — verify plist created
9. Wrapper test: run `scripts/threshold-wrapper.sh`, trigger restart via `threshold daemon restart`, verify wrapper rebuilds and restarts
10. Wrapper stop: run `threshold daemon stop` under wrapper, verify wrapper exits (stop sentinel)
11. Reboot test: restart machine, verify daemon starts automatically
12. Agent restart: in a conversation, agent runs `threshold daemon restart --follow-on-conversation <id> --follow-on-prompt "Restart complete. Verify the fix took effect."` — verify daemon drains, restarts, and conversation resumes with drain summary
13. Uninstall: `threshold daemon uninstall` — verify plist removed

---

## Files Affected (Summary)

| File | Action | Phase |
|------|--------|-------|
| `crates/server/src/main.rs` | PID file, SIGTERM, DaemonAction enum, status/stop/restart/install/uninstall commands, restart hook processing, health state creation, DaemonState wiring to all subsystems, drain in stop/restart | 16A, 16B, 16C, 16D |
| `crates/core/src/error.rs` | Add `DaemonAlreadyRunning { pid: u32 }` variant, add `DaemonDraining` variant | 16A, 16B |
| `crates/core/src/types.rs` | Add `HealthConfig` struct, add `DaemonState` struct (draining: `AtomicBool`, active_work: `AtomicU32`), add `RestartHook` struct with `DrainSummary` | 16B, 16C |
| `crates/scheduler/src/daemon_api.rs` | Add `Health`, `Drain`, and `Undrain` commands, accept `HealthConfig`/`Arc<DaemonState>`, change `scheduler` to `Option<SchedulerHandle>`, include draining/active_work in Health, handle scheduler-disabled errors | 16B |
| `crates/conversation/src/engine.rs` | Accept `Option<Arc<DaemonState>>`, check `is_draining()` in both `handle_message()` and `send_to_conversation()`, track `active_work` via `WorkGuard` in both paths | 16B |
| `crates/scheduler/src/engine.rs` | Accept `Option<Arc<DaemonState>>`, check `is_draining()` in `run_ready_tasks()`. One-shot task auto-deletion after execution | 16B, 16C |
| `crates/scheduler/src/execution.rs` | Track `active_work` for Script, NewConversation, ScriptThenConversation tasks | 16B |
| `crates/discord/src/bot.rs` | Accept `Option<Arc<DaemonState>>`, reject messages during drain | 16B |
| `crates/web/src/routes.rs` | Accept `Arc<DaemonState>` in app state, return 503 on actions during drain | 16B |
| `crates/scheduler/src/task.rs` | Add `one_shot: bool` field (`#[serde(default)]`) | 16C |
| `crates/server/src/daemon_client.rs` | Configurable socket path (canonical resolution chain), add `send_health_check()`, `send_drain()`, and `send_undrain()` | 16B |
| `scripts/threshold-wrapper.sh` | New: restart loop wrapper script with stop sentinel, supervised marker, rebuild support | 16D |
| `Cargo.toml` (server) | Add `libc` dependency | 16A |

---

## Resolved Design Questions

1. **Who restarts the daemon after the agent triggers a restart?** — In standalone mode, the `threshold daemon restart` CLI command handles the full cycle (stop, build, start, health check). In supervised mode (wrapper/launchd), the CLI writes `restart-pending.json` and sends SIGTERM; the wrapper script handles the rebuild and restart. The CLI detects supervised mode via a `$DATA_DIR/state/supervised` marker file.

2. **How does the agent maintain continuity through a restart?** — Restart follow-on hooks. The agent passes `--follow-on-conversation` and `--follow-on-prompt` to the restart command. The hook is written to disk before the daemon is stopped. On startup, the new daemon reads the hooks and calls `engine.send_to_conversation()` directly — no scheduler dependency required.

3. **Why not use the scheduler for follow-on hooks?** — The scheduler is optional (can be disabled in config). Follow-on hooks must work regardless. Processing hooks directly via `ConversationEngine::send_to_conversation()` is simpler and always available.

4. **Why not a built-in `restart_daemon` tool?** — The `ToolRegistry` is not wired into the conversation engine's streaming pipeline. The engine prepends a tool prompt to the system prompt and streams Claude CLI output. The agent invokes CLI subcommands via shell execution — `threshold daemon restart` follows this established pattern, consistent with `threshold schedule`, `threshold gmail`, etc.

5. **How do `stop` and `restart` interact with the wrapper script?** — `threshold daemon stop` writes a `stop-sentinel` file before sending SIGTERM. The wrapper checks for this sentinel on each loop iteration and exits instead of restarting. `threshold daemon restart` writes `restart-pending.json` (with optional `skip_build`) instead of the stop sentinel, so the wrapper rebuilds and restarts.

6. **What if the rebuild fails?** — The build runs *before* the daemon is stopped, in both modes. If the build fails, the restart is aborted immediately — the running daemon is never touched. The agent sees the compiler error output and can fix the issue and retry. This is a critical safety invariant: we never stop a running daemon without a known-good binary ready to replace it.

7. **What prevents infinite restart loops?** — The wrapper script has no built-in loop limit, but restart loops only happen on crashes (non-zero exit). A 5-second delay is added between crash restarts. The stop sentinel provides a reliable way to break out of the loop. A future enhancement could add a crash counter and exponential backoff.

8. **How does the agent know its own conversation ID?** — The conversation ID is part of the agent's context. The `memory.md` path (`$DATA_DIR/conversations/{id}/memory.md`) is visible to the agent, and the conversation ID can also be inferred from environment or system prompt injection.

9. **What about Windows/Linux?** — The PID file, health check, restart command, follow-on hooks, and wrapper script are platform-agnostic (or trivially portable). Only Phase 16D's launchd integration is macOS-specific. Linux systemd support would use the same patterns: PID file, wrapper script as ExecStart, and `systemctl restart` integration.

10. **Why a wrapper script instead of pure launchd KeepAlive?** — launchd's `KeepAlive` would restart the daemon immediately, but it can't run `cargo build` first. The wrapper checks `restart-pending.json` and conditionally rebuilds before restarting. It also handles the stop sentinel logic. The plist uses `KeepAlive/SuccessfulExit=false` so launchd restarts the *wrapper* if it crashes unexpectedly (non-zero exit), but leaves it down on intentional stop (exit 0 via stop sentinel).

11. **How are health check fields scoped?** — `HealthConfig` stores static fields set at startup: PID, start time, version. Scheduler task counts are computed dynamically per health request by calling `list_tasks()` on the `SchedulerHandle`, so they always reflect the current state. `active_work` and `draining` are read from `DaemonState` (lock-free atomics). Fields requiring other cross-crate queries (Discord connection status) are deferred to a future milestone.

12. **Should daemon management work when the scheduler is disabled?** — Yes. The DaemonApi (Unix socket listener) is decoupled from the scheduler and always starts as an independent top-level task. Health, status, stop, and restart all work regardless of scheduler config. Scheduler-specific commands return a `scheduler_disabled` error when the scheduler is not enabled. See the Health Check section for details.

13. **What is the single source of truth for data dir?** — The canonical resolution chain is: (1) `--data-dir` CLI flag → (2) `THRESHOLD_DATA_DIR` env var → (3) `THRESHOLD_CONFIG` env var (load config, read `data_dir` field) → (4) `~/.threshold` default. This single chain is used by all management commands and `DaemonClient`. The daemon exports both `THRESHOLD_DATA_DIR` and `THRESHOLD_CONFIG` at startup so child processes inherit them (steps 2 and 3). See "Canonical data-dir resolution order" in the CLI Subcommand Changes section for full details.

14. **In supervised mode, does restart return only after health is green?** — No. In supervised mode, the CLI returns immediately after signaling (SIGTERM sent, wrapper will handle the rest). The agent gets continuity via the follow-on hook, not via the CLI's return value. In standalone mode, the CLI blocks until health is green. This is intentional: the supervised wrapper is the authority in supervised mode, and having the CLI also wait would create two processes competing to verify startup. Note: the drain phase *does* block in both modes — the CLI waits for active runs to finish before sending SIGTERM.

15. **What happens to in-flight work during restart/stop?** — The daemon enters a "draining" state before SIGTERM is sent. In drain mode: new work is rejected (scheduler skips firing, Discord replies with "restarting", web returns 503), but in-flight conversations continue running until they complete naturally or the drain timeout expires. The CLI polls Health for `active_work == 0`. If the timeout expires, SIGTERM is sent and the daemon's shutdown path cancels streaming Claude runs via `ProcessTracker` and drops the `CancellationToken`. Script task children not tracked by `ProcessTracker` may outlive the daemon as orphans (see Drain section caveats). The drain summary (N finished, M aborted) is included in CLI output and prepended to follow-on hooks — the "aborted" count is best-effort since some tasks may complete after the daemon exits. This gives safety (most runs complete) with bounded latency (restart won't hang forever). Default drain timeout: 120s for restart (agents often run 30-60s), 30s for stop.

16. **Why not use `ProcessTracker::count()` for active work?** — `ProcessTracker` only tracks streaming Claude CLI processes (for abort support). It misses scheduler Script tasks, NewConversation (non-streaming path), and ScriptThenConversation — all of which represent active work that should complete before shutdown. `DaemonState.active_work` is a unified `AtomicU32` counter incremented at each work entry point: `ConversationEngine::handle_message()` (user-initiated conversations), `ConversationEngine::send_to_conversation()` (scheduler `ResumeConversation`, follow-on hooks), `exec_script()`, `exec_new_conversation()`, and `exec_script_then_conversation()`. Note: `handle_message()` and `send_to_conversation()` are separate streaming paths — both need independent `WorkGuard` tracking. `exec_resume_conversation()` delegates to `send_to_conversation()` which tracks it — no double-counting at the scheduler level. `ProcessTracker` is retained for abort support, not for drain tracking.

17. **Do Discord and Web need drain checks?** — Yes, for correctness. Without drain checks, new Discord messages or web actions could start conversations during the drain window, extending or defeating the drain. The checks are trivial (one `AtomicBool` read) and the error messages are clear: Discord gets a human-readable reply, web gets 503. Read-only web pages (status, conversation list) continue working during drain.

18. **What if the restart/stop fails after Drain?** — The CLI sends `Undrain` to roll back the draining state, restoring the daemon to normal operation. Without this, a failed restart (e.g., hook write error, signal failure) would leave the daemon alive but permanently rejecting new work. `Undrain` resets `DaemonState.draining = false`. If the socket is unreachable, the drain phase is skipped entirely (see socket-down fallback), so there's nothing to roll back.

19. **What if the socket is down during stop/restart?** — If the daemon process is alive (PID check passes) but the Unix socket is unreachable (connection refused, file missing), the CLI skips the drain phase and proceeds directly to SIGTERM with a warning. This handles edge cases like a crashed socket listener. Active work may be interrupted, but the alternative — refusing to stop/restart — is worse. The status command already has this fallback pattern.

20. **Why don't Script tasks register in ProcessTracker for clean abort?** — `ProcessTracker` is designed for streaming Claude CLI processes that support abort signaling. Script tasks are arbitrary shell commands (`tokio::process::Command`) with no standard cancellation protocol — there's no guarantee a script responds cleanly to SIGTERM. Adding script PID tracking and kill-on-shutdown is possible but adds complexity for marginal benefit: (1) the drain phase gives scripts time to complete naturally, (2) scripts are typically short cron-like jobs, (3) orphaned scripts reparented to PID 1 are cleaned up by the OS. If orphaned scripts become a real problem, a future enhancement could add a `ScriptTracker` that sends `SIGTERM` then `SIGKILL` with a grace period, or use process groups (`setsid`) to kill the script and all its descendants.
