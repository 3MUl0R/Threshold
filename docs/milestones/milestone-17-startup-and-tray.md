# Milestone 17 — Cross-Platform Startup & System Tray

## Goal

Give Threshold a polished, zero-friction presence on both macOS and Windows:

1. **Auto-start on boot** — daemon launches silently in the background when the machine starts, on both platforms.
2. **System tray icon** — a small status indicator in the menu bar (macOS) or notification area (Windows) that reflects daemon health and exposes common actions without opening a terminal.

macOS auto-start is already functional via `threshold daemon install` (launchd + wrapper script from Milestone 16). This milestone fills the Windows gap and adds the tray layer on both platforms.

## Reference

EchoType (https://github.com/3MUl0R/EchoType) is a prior implementation in this family of projects. Key patterns worth reusing:
- `scripts/autostart.ts` — cross-platform autostart via launchd, registry Run key, and systemd
- `src-tauri/src/tray.rs` — dynamic tray icon with state-driven RGBA icon generation and hide-on-close window behavior
- `#![windows_subsystem = "windows"]` to suppress console flash on Windows

## Deliverables

- `scripts/threshold-wrapper.ps1` — Windows PowerShell equivalent of `threshold-wrapper.sh`
- `threshold daemon install` / `uninstall` on Windows — Task Scheduler (XML import, no admin, ONLOGON trigger)
- `crates/tray/` — new crate with tray icon, event loop, and daemon socket connection
- `threshold tray` CLI subcommand — launches/runs the tray process
- `threshold daemon install` updates to also register the tray for auto-start on both platforms
- Icon assets in `assets/tray/` (PNG, at minimum 32×32 and 16×16 for Windows)

## Architecture

### Two Binaries, Clear Responsibilities

| Binary | Role | Platform |
|--------|------|----------|
| `threshold` | Daemon, CLI — headless, all platforms | macOS, Windows, Linux |
| `threshold-tray` | System tray process — GUI subsystem, user session | macOS, Windows |

The tray is a **separate binary** built from `crates/tray/`. This separation is required because:
- The tray needs `#![windows_subsystem = "windows"]` (suppresses console) — the daemon must NOT have this flag (it writes to launchd/wrapper logs)
- The tray runs an event loop on the main thread; the daemon runs a tokio reactor
- They have different lifetimes: the tray should stay up even if the daemon is stopped, so the user can restart it from the tray menu

### Communication

The tray polls the daemon via the existing Unix socket (`~/.threshold/threshold.sock`) using `DaemonCommand::Health`. It does NOT embed any daemon logic.

Unix sockets are supported on Windows 10 1803+ (April 2018) via the `tokio::net::UnixListener` / `UnixStream` path. This is the same socket the daemon already uses — no new protocol needed.

Poll interval: 5 seconds when daemon is running, 30 seconds when stopped (reduces noise in stopped state).

### Tray States

| State | Icon Color | Tooltip |
|-------|-----------|---------|
| Running | Green | "Threshold — Running" |
| Draining | Yellow | "Threshold — Draining (restarting)" |
| Stopped | Gray | "Threshold — Stopped" |
| Unknown / Error | Red | "Threshold — Unreachable" |

### Tray Menu

```
Open Dashboard          (opens http://127.0.0.1:<port> — port read from config at tray startup)
─────────────────
Restart                 (calls `threshold daemon restart`)
Stop                    (calls `threshold daemon stop`)
Start                   (calls `threshold daemon start [--data-dir] [--config]`, only shown when stopped)
─────────────────
Launch at Login  [✓]   (checkbox — toggles autostart registration)
─────────────────
Quit Tray               (exits tray process only, daemon keeps running)
```

"Open Dashboard" is grayed out when the daemon is stopped (web server is also down).

### Autostart Registration

The "Launch at Login" checkbox toggles autostart for **the tray binary** (`threshold-tray`), not the daemon binary directly. The tray binary is what the user interacts with. On startup, the tray will:
1. Start the daemon if it is not already running (via `threshold daemon start [--data-dir <dir>] [--config <path>]` — same paths the tray was launched with — as a detached subprocess)
2. Show the tray icon

This means installing only the tray is sufficient for most users. The launchd wrapper (Milestone 16) remains the correct choice for server/headless deployments where no tray is desired.

**macOS autostart**: `LaunchAgent` plist in `~/Library/LaunchAgents/com.threshold.tray.plist`. Use `auto-launch = "0.6"` crate for cross-platform toggle.

**Windows autostart**: `HKEY_CURRENT_USER\SOFTWARE\Microsoft\Windows\CurrentVersion\Run` registry key. No admin required. `auto-launch = "0.6"` handles this platform too.

## Windows Auto-Start

`threshold daemon install` on Windows registers both the daemon (via Task Scheduler) and the tray (via Registry Run key). For users who want the daemon only without the tray — e.g., running Threshold as a server tool — they can skip `daemon install` and instead register the wrapper script manually with Task Scheduler, or run the daemon directly at startup via another mechanism.

### PowerShell Wrapper (`scripts/threshold-wrapper.ps1`)

Direct port of `scripts/threshold-wrapper.sh`. Additionally, the supervised-mode detection in `crates/server/src/main.rs` (`detect_supervised()`) currently only recognizes shell wrapper process names (`bash`, `sh`, `zsh`). It must be updated to also recognize `pwsh` and `powershell` so the stop-sentinel and restart-pending logic works correctly on Windows.

```diff
// main.rs — detect_supervised() process name check
// Strip .exe suffix before matching (Windows process names include it)
- ends with "bash", "sh", or "zsh"
+ ends with "bash", "sh", "zsh", "pwsh", or "powershell"
// Implementation: comm.trim_end_matches(".exe").ends_with(...)
```

```
- Read THRESHOLD_DATA_DIR env var (set in Task Scheduler XML at install time)
- Read THRESHOLD_CONFIG env var (also set in Task Scheduler XML at install time)
- Write supervised marker JSON to $THRESHOLD_DATA_DIR\state\supervised
- Remove marker on exit (try/finally)
- Initial build if binary missing
- Loop:
    - Check state\stop-sentinel → break loop
    - Check state\restart-pending.json → optionally build, delete file
    - Run "threshold.exe daemon start" (THRESHOLD_DATA_DIR + THRESHOLD_CONFIG inherited from env)
    - On exit: check stop-sentinel again; sleep 5s if crash (non-zero exit)
- Exit 0 on intentional stop
```

### Windows `threshold daemon install`

Uses Task Scheduler via XML import (more reliable than `schtasks /create` flag parsing — avoids `/ru` password prompts, `%USERNAME%` shell expansion hazards, and `/tr` length limits):

```rust
// Generate XML task definition and import via:
//   schtasks /create /tn "Threshold Daemon" /xml <tempfile> /f
//
// Key XML elements:
//   <LogonTrigger>           — runs at current user login
//   <UserId>DOMAIN\alice</UserId>              — resolved in Rust via std::env::var("USERDOMAIN"/"USERNAME"), not a literal %var%
//   <RunLevel>LeastPrivilege</RunLevel>        — no elevation, no prompt
//   <Command>powershell.exe</Command>
//   <Arguments>-WindowStyle Hidden -File "C:\path\to\threshold-wrapper.ps1"</Arguments>
//   <WorkingDirectory>C:\path\to\repo</WorkingDirectory>
```

`threshold daemon uninstall` removes it:
```
schtasks /delete /tn "Threshold Daemon" /f
```

**Why XML import over `schtasks /create` flags:**
- `/ru` with no `/rp` can trigger UAC credential prompts on some Windows configurations
- `%USERNAME%` is not safe to pass as a literal argument from Rust — use `std::env::var("USERNAME")` and embed the resolved value in the XML
- `/tr` argument has a 261-character path length limit; XML avoids this
- XML allows setting `WorkingDirectory`, environment variables, and run level cleanly

**Why Task Scheduler over Registry Run key for the daemon:**
- Registry Run key launches processes without a working directory or custom environment
- Task Scheduler allows setting `WorkingDirectory` (needed for `cargo build` in the wrapper) and explicit environment (PATH with cargo bin)
- More reliable for long-running processes with restart semantics

**Why Registry Run key for the tray:**
- The tray binary has `windows_subsystem = "windows"` — it's already silent
- Simple: one registry value, no XML, no `schtasks` parsing
- `auto-launch` crate handles it without platform-specific code

## Windows Platform Compatibility

The existing Milestone 16 daemon control code is Unix-only. Before the tray's Stop/Restart menu items can work on Windows, the following must be addressed in Phase 17A:

| Feature | Current (Unix) | Windows Required |
|---------|---------------|-----------------|
| Send SIGTERM | `libc::kill(pid, SIGTERM)` | `GenerateConsoleCtrlEvent` or `TerminateProcess` via `windows-sys` |
| Check process alive | `libc::kill(pid, 0)` | `OpenProcess` + `GetExitCodeProcess` via `windows-sys` |
| Identify process by name | `ps -p <pid> -o comm=` | `tasklist /fi "PID eq <pid>"` or WMI |
| Supervised marker check start time | `ps -o lstart=` | `GetProcessTimes` via `windows-sys` |
| Unix socket | `tokio::net::UnixListener` | Available on Windows 10 1803+ via tokio (no change needed) |

These are gated with `#[cfg(target_os = "windows")]` / `#[cfg(unix)]` blocks. The scope for this milestone is to add the Windows-specific paths so `stop`/`restart`/`status` work on Windows.

**`winreg`** crate for registry keys (tray autostart) and **`windows-sys`** for process management are the recommended additions:

```toml
# crates/server/Cargo.toml
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.59", features = [
    "Win32_System_Threading",
    "Win32_Foundation",
] }
```

### Config/Data-Dir Passthrough

The tray binary must be launched with `--data-dir` and `--config` when the user has non-default paths. The `auto-launch` registration and the tray's internal `threshold daemon start` invocations must embed these flags. The `threshold daemon install` command reads the effective data dir and config path and bakes them into both the Task Scheduler XML (daemon) and the `auto-launch` registration (tray).

### Double-Start Prevention

If both the Task Scheduler task and the tray's startup logic attempt to start the daemon simultaneously at login, the second attempt will fail gracefully — `check_existing_daemon()` (Milestone 16) returns `DaemonAlreadyRunning` and the launcher exits cleanly. No additional guard is needed, but the tray should treat `DaemonAlreadyRunning` from its startup launch as a success (not an error).

### Dashboard URL

The "Open Dashboard" menu action uses the web server port from config (`[web] port`, default 3000), resolved from the config file path the tray was launched with. It is not hardcoded.

## Phase Plan

### Phase 17A — Windows Daemon Startup

**Goal:** `threshold daemon install` / `uninstall` works on Windows, daemon starts at login.

**Files:**
- `scripts/threshold-wrapper.ps1` — PowerShell restart loop
- `crates/server/src/main.rs` — `run_daemon_install()` / `run_daemon_uninstall()` with `#[cfg(target_os = "windows")]` and `#[cfg(target_os = "macos")]` branches
- `crates/core/src/config.rs` or platform helpers — any needed path utilities

**Changes:**
- `run_daemon_install()` dispatches on `cfg!(target_os)`:
  - `macos`: existing launchd plist path (no change)
  - `windows`: writes Task Scheduler task via `schtasks /create ...`
  - other: prints "Not supported on this platform" and exits 1
- `run_daemon_uninstall()` mirrors this dispatch
- PowerShell wrapper uses `$PSScriptRoot` to find the repo root (equivalent of bash's `dirname $0`)

**Verification:**
- Manual test on Windows: `threshold daemon install`, log out and log back in, daemon auto-starts
- `threshold daemon status` shows running after login
- `threshold daemon uninstall` removes the task

---

### Phase 17B — Tray Crate Foundation

**Goal:** `crates/tray/` builds on both macOS and Windows with a functional tray icon.

**New crate:** `crates/tray/`

**Dependencies:**
```toml
[dependencies]
tray-icon = "0.21"
tao = "0.34"           # event loop; no window needed
image = { version = "0.25", features = ["png"] }
auto-launch = "0.6"
tokio = { version = "1", features = ["rt", "net", "time", "process"] }
serde_json = "1"
anyhow = "1"
tracing = "0.1"
threshold-core = { path = "../core" }
threshold-scheduler = { path = "../scheduler" }  # for DaemonCommand/DaemonResponse

[target.'cfg(windows)'.build-dependencies]
winres = "0.1"          # embed app icon into .exe
```

**`crates/tray/src/main.rs`:**
```
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    // Parse args: --data-dir, --config
    // Init tracing (file-only, no stderr on Windows release)
    // Start tokio runtime for daemon polling (separate thread)
    // Run tray event loop on main thread
}
```

**Icon strategy:**
- Include PNG icon files at compile time: `include_bytes!("../../../assets/tray/icon-green.png")`, etc.
- Load via `image::load_from_memory()` → `tray_icon::Icon::from_rgba()`
- Four icons: green (running), yellow (draining), gray (stopped), red (error)
- On macOS: use `with_icon_as_template(false)` to preserve color; OR use template icons for Dark Mode compatibility
- Minimum icon sizes: 32×32 PNG (shown at 16×16 on Windows)

**Daemon polling (tokio task on background thread):**
```
loop {
    let health = try_health_check(&socket_path).await;
    state_tx.send(health);
    sleep(poll_interval).await;   // 5s if running, 30s if stopped
}
```

`try_health_check()` connects to the Unix socket, sends `DaemonCommand::Health`, reads `DaemonResponse`. Returns `TrayState` (Running/Draining/Stopped/Error).

**Event loop (main thread):**
```
StartCause::Init → build TrayIcon, create menu items, spawn daemon poller
UserEvent::StateChanged(s) → set_icon(), set_tooltip()
UserEvent::MenuEvent(id) → match id {
    open_dashboard  → open_browser(&format!("http://127.0.0.1:{}", config.web.port))  // port from config
    restart         → spawn("threshold daemon restart")
    stop            → spawn("threshold daemon stop")
    start           → spawn("threshold daemon start [--data-dir <dir>] [--config <path>]")
    launch_at_login → toggle_autostart()
    quit            → drop tray, exit event loop
}
```

`open_browser()` uses `open` crate or platform-specific command (`open` on macOS, `start` on Windows, `xdg-open` on Linux).

**Files:**
```
crates/tray/
├── Cargo.toml
├── build.rs                # winres icon embedding on Windows
└── src/
    ├── main.rs             # entry point, event loop
    ├── tray.rs             # TrayIcon builder, icon loading, state mapping
    ├── menu.rs             # Menu construction, item IDs
    ├── poller.rs           # tokio daemon health polling task
    └── autostart.rs        # auto-launch toggle wrapper
```

**Binary target in workspace:**
The `crates/tray/` crate is a binary (`src/main.rs`), producing `threshold-tray`. It is named separately from the `threshold` binary in `crates/server/`.

**Assets:**
```
assets/
└── tray/
    ├── icon-green.png    (32×32, RGBA)
    ├── icon-yellow.png
    ├── icon-gray.png
    ├── icon-red.png
    └── icon.ico          (Windows multi-resolution .ico for taskbar)
```

---

### Phase 17C — CLI Integration & Install Improvements

**Goal:** `threshold tray` subcommand, `threshold daemon install` installs tray autostart.

**`threshold tray`** subcommand in `crates/server/src/main.rs`:
- Finds `threshold-tray` binary relative to current executable (`std::env::current_exe()?.parent()?.join("threshold-tray")`)
- Executes it (replaces current process with `exec` on Unix, `spawn` + wait on Windows)
- Accepts `--data-dir` and `--config` pass-through flags

**`threshold daemon install` updates:**
- After registering the daemon, also register the tray via `auto-launch`:
  ```
  threshold-tray binary → registered in:
    macOS: ~/Library/LaunchAgents/com.threshold.tray.plist (RunAtLoad=true, separate plist)
    Windows: HKCU\...\Run\ThresholdTray → "C:\...\threshold-tray.exe --data-dir <dir> --config <path>"
  ```
  The full `--data-dir` and `--config` paths are embedded at install time (resolved from effective config at the time `threshold daemon install` is run).
- Print clear output: "Daemon: registered via Task Scheduler / launchd" + "Tray: registered via Run key / LaunchAgent"
- `threshold daemon uninstall` removes both

**Tray auto-start behavior on launch:**
1. Tray starts (user logged in)
2. Tray checks if daemon is running (health check via socket)
3. If stopped: spawn `threshold daemon start [--data-dir <dir>] [--config <path>]` (same paths tray was launched with) as a detached subprocess, wait up to 10s for it to come up
4. Show tray icon with current state

---

### Phase 17D — Polish & Testing

**Goal:** Smooth experience on both platforms, no rough edges.

**macOS polish:**
- Test Dark Mode + Light Mode — ensure icon is visible in both
- Test after System Sleep → Wake (daemon may need restart, tray should recover)
- Test `threshold daemon restart` from terminal while tray is showing — tray should reflect stopped → starting → running state transitions
- Menu bar icon should not flicker during state changes (`set_icon_with_as_template` on macOS)

**Windows polish:**
- Verify no console window flashes on login (requires `windows_subsystem = "windows"` in release build)
- Verify tray icon appears in notification area (not hidden in overflow flyout by default — Windows may hide new icons)
- Verify `Open Dashboard` opens the browser correctly
- Test daemon restart from tray menu

**Error cases:**
- Daemon binary not found at expected path → tray shows red icon, menu shows "daemon not installed" message
- Socket exists but daemon unresponsive → red icon + tooltip "Threshold — Unreachable (socket timeout)"
- `threshold daemon start` fails (build error) → tray shows error notification if OS notification API available

**Testing:**
- Unit tests in `crates/tray/src/` for state mapping and icon selection logic
- Integration test: launch tray with `--data-dir` pointing to a temp dir (no daemon), verify it shows stopped state
- Manual test matrix: macOS 13+, Windows 10 (1803+), Windows 11

---

## Crate Dependency Graph (Updated)

```
core ─────────────────────────────┐
  └─ cli-wrapper                  │
       └─ conversation            │
            └─ scheduler ─────────┤
                 └─ server ───────┤
                                  │
tray ────────────────────────────►┘
  (depends on: core, scheduler for socket protocol types)
```

## Files Affected

| File | Change |
|------|--------|
| `crates/tray/` | New crate (binary: `threshold-tray`) |
| `assets/tray/*.png` | New icon assets |
| `assets/tray/icon.ico` | New Windows ICO file |
| `scripts/threshold-wrapper.ps1` | New Windows wrapper script |
| `crates/server/src/main.rs` | Add `Tray` command, update `Install`/`Uninstall` for Windows + tray |
| `Cargo.toml` (workspace) | `crates/tray` auto-discovered |

## Key Design Decisions

**Q: Why a separate binary instead of integrating the tray into the daemon?**
A: Windows requires `#![windows_subsystem = "windows"]` to suppress the console, but the daemon must write to stdout/stderr for launchd/wrapper logs. Separate binaries, separate subsystem flags. Also keeps the daemon fully headless for server deployments.

**Q: Why Task Scheduler for Windows daemon, but Registry Run key for tray?**
A: The daemon wrapper (PS1 script) needs a working directory, environment variables (PATH with cargo), and the ability to restart on failure — Task Scheduler exposes all of this cleanly. The tray binary has `windows_subsystem = "windows"` so it's already silent; the simple Registry Run key is sufficient and requires no `schtasks` parsing.

**Q: Why `tray-icon` + `tao` instead of Tauri?**
A: Tauri adds ~40MB of dependencies and a JS/HTML UI layer we don't need. `tray-icon` and `tao` are the exact crates Tauri wraps internally, available standalone. The tray UI is simple enough that pure-Rust menus (via `muda`) are sufficient.

**Q: Why `auto-launch` crate for tray registration instead of platform-specific code?**
A: `auto-launch 0.6` handles macOS LaunchAgent and Windows Registry Run key in one API. Saves writing and maintaining platform-specific code for a non-critical path.

**Q: What happens to the existing macOS `threshold daemon install` (launchd)?**
A: It stays unchanged. It remains the correct mechanism for headless/server macOS deployments. The tray adds its own separate LaunchAgent entry (`com.threshold.tray`) so they are independent.

**Q: Does the tray need to be running for the daemon to work?**
A: No. The daemon runs independently. The tray is a convenience process. Users can run Threshold entirely without the tray (e.g., all terminal, all `threshold daemon` CLI commands).

**Q: Does the tray start the daemon if it is not running?**
A: Yes, on initial tray startup only. If the user explicitly stops the daemon via `threshold daemon stop`, the tray will show "Stopped" but will NOT automatically restart it (respects the user's intent). The user can restart from the tray menu.

## Backward Compatibility

- `threshold daemon install` on macOS: the existing launchd plist behavior is unchanged; this milestone adds a second LaunchAgent plist for the tray (`com.threshold.tray`). The daemon plist (`com.threshold.daemon`) is not modified.
- `threshold daemon start` / `stop` / `status` / `restart`: unchanged
- The tray is entirely additive — users who don't run `threshold daemon install` or `threshold tray` see no change

## Verification Steps

1. `cargo build --workspace` — clean build on macOS
2. `cargo build --workspace --target x86_64-pc-windows-gnu` (or cross-compile) — clean build on Windows target
3. `threshold daemon install` on macOS → launchd + tray plist both written
4. `threshold daemon install` on Windows → Task Scheduler task + registry key both written
5. Log out, log back in on each platform → daemon and tray both auto-start silently
6. Tray icon state changes correctly as daemon is stopped and started
7. "Open Dashboard" opens browser
8. "Launch at Login" checkbox reflects actual registration state
