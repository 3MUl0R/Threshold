# Milestone 13: File-Based Secret Store Backend

## Context

The current `SecretStore` uses the macOS Keychain via the `keyring` crate. Every time the threshold daemon binary is recompiled, macOS treats it as a new application and prompts for keychain authorization. This makes development painful (constant prompts) and headless operation impossible (no GUI to click "Allow").

**Solution:** Add a file-based secret store backend (`<data_dir>/secrets.toml`, chmod 600) as the new default. Keep keychain as opt-in via `secret_backend = "keychain"` in config. The `get/set/delete/resolve` method signatures are unchanged — all consumers continue to work. Constructor API changes: `new()` now returns `Result`, `Default` trait impl removed.

## Design

**Enum dispatch inside `SecretStore`** (not a trait). The struct gains an internal `SecretBackendInner` enum with `File(FileStore)` and `Keychain` variants. This is the same pattern used by `CliProvider`, `ConversationMode`, and `ToolProfile` in the codebase. Zero changes needed to consumers.

**File format:** TOML with a `[secrets]` section, 0600 permissions, `BTreeMap` for deterministic key ordering.
```toml
[secrets]
discord-bot-token = "xyzabc..."
google-api-key = "AIza..."
```

TOML handles special characters in keys and values (quotes, `@`, `.`) natively via its string escaping — no manual escaping needed. Keys like `gmail-oauth-refresh-token-alice@gmail.com` work correctly.

**Config:** Top-level `secret_backend: Option<String>` field (like `log_level`). Defaults to `"file"` when absent. All 7 struct literal sites must add `secret_backend: None` (serde's `#[serde(default)]` only affects deserialization, not compile-time struct literals).

**Memory model — no persistent cache:** Every `get()` re-reads the file from disk. Every `set()`/`delete()` acquires a lock, re-reads the file, applies the change, writes back. This ensures cross-process consistency when daemon + CLI tools access the same file simultaneously. The file is small (a few KB at most), so the I/O cost is negligible.

**File locking:** Use a separate `secrets.toml.lock` lockfile via `fs2::lock_exclusive()` for all write operations and `lock_shared()` for reads. This avoids the inode problem with advisory locks (locking the data file itself would break when rename swaps inodes).

**Permission hardening (Unix):**
- Create new files with `OpenOptions` + `.mode(0o600)` via `std::os::unix::fs::OpenOptionsExt` (never world-readable)
- On load: if existing file permissions are not 0600, auto-chmod to 0600 and log a warning
- Reject symlink paths at construction (prevent symlink attacks)
- All permission code gated with `#[cfg(unix)]` — on non-Unix platforms, permissions are skipped (relies on OS user-directory ACLs, matching how `~/.docker/config.json` works on Windows)

**Constructor API changes:**
- `new()` → now returns `crate::Result<Self>` (was infallible). File backend at default path, fails on I/O error.
- `with_file_backend(path)` → returns `crate::Result<Self>` (new)
- `with_backend(backend, data_dir)` → returns `crate::Result<Self>` (new)
- `with_keychain_backend(service_name)` → infallible, returns `Self` (no I/O)
- `with_service_name(name)` → infallible, returns `Self` (preserved, always keychain)
- `Default` trait impl **removed** (since `new()` can now fail)
- Call sites using `SecretStore::new()` must add `?` or `.unwrap()`:
  - Production: `main.rs:93`
  - Test/E2E: `crates/web/src/lib.rs:144`, `crates/web/tests/e2e_server.rs:77`
  - Internal tests in `secrets.rs`:
    - Line 192-194 (`new_creates_with_default_service_name`) → **rewrite**: `SecretStore::new().unwrap()` + assert `backend_name() == "file"` (the `service_name` field no longer exists)
    - Line 198-200 (`with_service_name_creates_with_custom_name`) → **rewrite**: assert `backend_name() == "keychain"` (the `service_name` field is now inside `SecretBackendInner::Keychain`)
    - Line 204-206 (`default_creates_with_default_service_name`) → **remove** (since `Default` trait is removed)
  - Doctest at `secrets.rs:132` → add `?` or update example

**No automatic migration** from keychain — that would require the keychain access we're trying to avoid. Instead, log an info message on first startup pointing users to re-enter via web UI or switch backend.

**Security note:** The plaintext file is appropriate for this use case: the secrets file lives in `<data_dir>/` (owner-only directory) with 0600 permissions, matching how `~/.ssh/`, `~/.aws/credentials`, and `~/.docker/config.json` handle secrets. The keychain backend remains available for users who prefer hardware-backed storage.

## Files to Modify

| File | Change |
|------|--------|
| `crates/core/Cargo.toml` | Add `fs2 = "0.4"` dependency |
| `crates/core/src/secrets.rs` | Add `SecretBackend` enum, `FileStore` struct, `SecretBackendInner` dispatch, file backend tests |
| `crates/core/src/config.rs` | Add `secret_backend: Option<String>`, validation, `secret_backend()` helper, tests |
| `crates/server/src/main.rs` | Use `SecretStore::with_backend(config.secret_backend(), Some(data_dir))?`, add migration log hint |
| `crates/server/src/gmail.rs` | Create `SecretStore` from config and pass to gmail handler |
| `crates/server/src/imagegen.rs` | Create `SecretStore` from config and pass to imagegen handler |
| `crates/gmail/src/cli.rs` | Accept `Arc<SecretStore>` parameter instead of creating own |
| `crates/imagegen/src/cli.rs` | Accept `Arc<SecretStore>` parameter instead of creating own |
| `crates/web/src/lib.rs` | Test: use `with_file_backend(data_dir.join("secrets.toml"))`, add `secret_backend: None` to struct literal |
| `crates/web/tests/e2e_server.rs` | Same: use `with_file_backend(data_dir.join("secrets.toml"))`, add `secret_backend: None` |
| `crates/conversation/src/engine.rs` | Add `secret_backend: None` to struct literals (lines 885, 1149) |
| `crates/scheduler/src/engine.rs` | Add `secret_backend: None` to struct literal (line 514) |
| `crates/discord/src/portals.rs` | Add `secret_backend: None` to struct literal (line 43) |
| `crates/tools/src/prompt.rs` | Add `secret_backend: None` to struct literal (line 76) |
| `config.example.toml` | Add `secret_backend` docs, update "keychain" references |
| `crates/web/src/routes/config.rs` | Update "Keychain access timed out" error messages |
| `crates/web/templates/config/credentials.html` | Update "system keychain" copy (line 8) |
| `crates/web/tests/e2e_playwright.sh` | Update "Keychain note present" assertion (line 184) to match new copy |
| `crates/imagegen/src/client.rs` | Update error messages "configure in keychain" (line 18) and "Keychain error" (line 84) |
| `crates/gmail/src/lib.rs` | Update doc comment "stored in OS keychain" (line 14) → "stored in the secret store" |
| `crates/gmail/src/auth.rs` | Update doc comments referencing "keychain" (lines 10, 12, 36, 40, 45, 71, 77) |
| `crates/core/src/config.rs` | Update comment "bot_token resolved from keychain" (line 44) |
| `readme.md` | Update "macOS Keychain integration" references (lines 25, 41, 75, 99) |
| `SETUP.md` | Update "macOS Keychain" references (line 21) |
| `scripts/setup-discord-token.sh` | Rewrite to write to `<data_dir>/secrets.toml` (file backend, `--data-dir` flag); remove macOS `security` command path |

## Implementation Phases

### Phase 1: Core — FileStore + SecretBackend enum in `secrets.rs`

**File:** `crates/core/Cargo.toml`

Add `fs2 = "0.4"` to `[dependencies]`.

**File:** `crates/core/src/secrets.rs`

Add:
- `SecretBackend` enum (`File`, `Keychain`) with `Default` impl → `File`, `#[derive(Debug, Clone, Copy, PartialEq, Eq)]`
- `FileStore` struct: `path: PathBuf`, `lock_path: PathBuf`
  - `FileStore::new(path) -> Result` — rejects symlinks, auto-chmod to 0600 if wrong (`#[cfg(unix)]`), creates parent dirs
  - `FileStore::read_file() -> BTreeMap` — acquire shared lock on `.lock` file, read+parse TOML, release lock
  - `FileStore::get` — calls `read_file()`, returns cloned value (no in-memory cache)
  - `FileStore::set/delete` — acquire exclusive lock on `.lock` file, re-read file, apply change, write back:
    1. Open `.lock` file, `fs2::lock_exclusive()`
    2. Read current file contents into `BTreeMap`
    3. Apply change to map
    4. Serialize to TOML
    5. Write to `.tmp` file with `OpenOptions::mode(0o600)` on Unix (never world-readable)
    6. Atomic rename `.tmp` → target
    7. Release lock
    On write failure: return error, file unchanged.
  - Rejects symlink paths at construction
- `SecretBackendInner` enum (`File(FileStore)`, `Keychain(String)`) — private
  - `Keychain(String)` holds the service name
- Refactor `SecretStore` to dispatch through `SecretBackendInner`:
  - `new() -> crate::Result<Self>` → file backend at default path (`~/.threshold/secrets.toml`), **fails explicitly** on error. **Breaking change** from current infallible `new()`.
  - `with_file_backend(path: PathBuf) -> crate::Result<Self>` → new constructor
  - `with_keychain_backend(service_name: impl Into<String>) -> Self` → new constructor (infallible, no I/O at construction)
  - `with_backend(backend: SecretBackend, data_dir: Option<PathBuf>) -> crate::Result<Self>` → dispatches: file uses `data_dir/secrets.toml` (data_dir required for `File`, ignored for `Keychain`); keychain uses default service name and never touches filesystem
  - `with_service_name(name: impl Into<String>) -> Self` → preserved, always keychain, infallible (for existing test isolation)
  - `backend_name() -> &'static str` → new introspection method
  - `get/set/delete/resolve` → dispatch to inner backend (signatures unchanged)
  - `is_keychain_available()` → returns `false` for file backend
  - **Remove** `impl Default for SecretStore` (since `new()` can now fail)

Existing keychain logic stays in the `Keychain` match arms — no behavioral changes to keychain path.

**Tests** (all run without keychain, no `#[ignore]`):
- `file_backend::set_and_get_roundtrip`
- `file_backend::get_nonexistent_returns_none`
- `file_backend::delete_removes_secret`
- `file_backend::delete_nonexistent_succeeds`
- `file_backend::overwrite_updates_value`
- `file_backend::persists_across_instances` (new store reads old file)
- `file_backend::resolve_with_env_fallback`
- `file_backend::backend_name_is_file`
- `file_backend::is_keychain_available_returns_false`
- `file_backend::file_has_600_permissions` (unix only)
- `file_backend::empty_file_loads_ok`
- `file_backend::toml_format_is_correct` (verify `BTreeMap` deterministic ordering)
- `file_backend::new_default_uses_file_backend` (uses `with_file_backend(tempdir.join("secrets.toml"))` and verifies `backend_name() == "file"` — does NOT call `new()` to avoid touching real `~/.threshold/`)
- `file_backend::special_chars_in_values` (values with `@`, `.`, quotes, newlines)
- `file_backend::special_chars_in_keys` (keys like `gmail-oauth-refresh-token-alice@gmail.com`)
- `file_backend::flush_failure_leaves_file_unchanged` (read-only dir → set fails, old value preserved)
- `file_backend::rejects_symlink_path`
- `file_backend::concurrent_instances_dont_corrupt` (two FileStore instances on same file, interleaved writes from separate threads)
- `file_backend::auto_chmod_fixes_permissions` (unix only: create file with 0644, open FileStore, verify 0600)

Existing keychain tests remain with `#[ignore]`.

### Phase 2: Config — `secret_backend` field in `config.rs`

**File:** `crates/core/src/config.rs`

- Add `#[serde(default)] pub secret_backend: Option<String>` to `ThresholdConfig`
  - `#[serde(default)]` ensures TOML deserialization works when field is absent
  - **Struct literals** in 7 other crates must be updated to include `secret_backend: None` (Rust struct literals are not affected by serde attributes)
- Add validation in `validate()`: must be `"file"` or `"keychain"` if present
- Add `pub fn secret_backend(&self) -> SecretBackend` helper (defaults to `File`)

**Struct literal updates** (add `secret_backend: None`):
- `crates/conversation/src/engine.rs:885` (test_config function)
- `crates/conversation/src/engine.rs:1149` (make_engine_with_dir)
- `crates/scheduler/src/engine.rs:514` (test setup)
- `crates/discord/src/portals.rs:43` (test_config function)
- `crates/tools/src/prompt.rs:76` (minimal_config function)
- `crates/web/src/lib.rs:85` (test helper)
- `crates/web/tests/e2e_server.rs:19` (E2E test server)

**Tests:**
- `secret_backend_defaults_to_file`
- `secret_backend_file_accepted`
- `secret_backend_keychain_accepted`
- `validate_rejects_invalid_secret_backend`

### Phase 3: Wiring — daemon, CLI handlers, test helpers, docs

**File:** `crates/server/src/main.rs` (line 93)

Change:
```rust
let secrets = Arc::new(SecretStore::new());
```
to:
```rust
let data_dir = config.data_dir()?;
let secrets = Arc::new(SecretStore::with_backend(config.secret_backend(), Some(data_dir.clone()))?);
tracing::info!("Secret store backend: {}", secrets.backend_name());
```

Add migration hint log:
```rust
if secrets.backend_name() == "file" {
    let secrets_path = data_dir.join("secrets.toml");
    if !secrets_path.exists() {
        tracing::info!(
            "No secrets.toml found. Set credentials via the web UI at /config/credentials \
             or switch to keychain backend with secret_backend = \"keychain\" in config."
        );
    }
}
```

**File:** `crates/server/src/gmail.rs`

Create `SecretStore` from config backend and pass to gmail:
```rust
let backend = config.secret_backend();
let data_dir = if backend == SecretBackend::File { Some(config.data_dir()?) } else { None };
let secrets = Arc::new(SecretStore::with_backend(backend, data_dir)?);
threshold_gmail::handle_gmail_command(args, gmail_config, audit_path.as_deref(), secrets).await
```

**File:** `crates/server/src/imagegen.rs`

Same pattern — conditionally resolve `data_dir` only for file backend, then create `SecretStore` and pass through.

**File:** `crates/gmail/src/cli.rs`

Change `handle_gmail_command` signature to accept `secret_store: Arc<SecretStore>` as parameter instead of creating `Arc::new(SecretStore::new())` internally. Remove the internal construction at line 110.

**File:** `crates/imagegen/src/cli.rs`

Same — accept `Arc<SecretStore>` parameter in `handle_imagegen_command`. Remove internal construction at line 58.

**File:** `crates/web/src/lib.rs` (line 144) and `crates/web/tests/e2e_server.rs` (line 77)

Change `SecretStore::new()` → `SecretStore::with_file_backend(data_dir.join("secrets.toml")).unwrap()`.

**File:** `config.example.toml`

Add after `log_level`:
```toml
# Secret storage backend: "file" (default) or "keychain"
# - file: stores secrets in <data_dir>/secrets.toml (chmod 600)
# - keychain: uses OS keychain (macOS Keychain, Windows Credential Manager)
# secret_backend = "file"
```

Update line 43 from:
```toml
# Note: Discord bot token is stored in keychain or DISCORD_BOT_TOKEN env var
```
to:
```toml
# Note: Discord bot token is stored in the secret store or DISCORD_BOT_TOKEN env var
```

**File:** `crates/web/src/routes/config.rs`

Update error message strings:
- `"Keychain access timed out"` → `"Secret store access timed out"` (lines 226, 267)

**File:** `crates/web/templates/config/credentials.html` (line 8)

Change:
```html
<p>Secrets are stored in the system keychain. Values are never displayed.</p>
```
to:
```html
<p>Secrets are stored in the secret store. Values are never displayed.</p>
```

**File:** `crates/web/tests/e2e_playwright.sh` (line 184)

Change assertion from:
```bash
pw_test "Keychain note present" "keychain"
```
to:
```bash
pw_test "Secret store note present" "secret store"
```
Also update skip messages on lines 187-190 from "keychain" to "secret store".

**File:** `crates/imagegen/src/client.rs` (line 18)

Change error message (line 18) from:
```rust
#[error("API key not found: configure 'google-api-key' in keychain or set GOOGLE_API_KEY")]
```
to:
```rust
#[error("API key not found: configure 'google-api-key' in secret store or set GOOGLE_API_KEY")]
```

Also update line 84: `"Keychain error: {}"` → `"Secret store error: {}"`.

**File:** `crates/gmail/src/lib.rs` (line 14)

Change `"OAuth 2.0 with per-inbox tokens stored in OS keychain"` → `"OAuth 2.0 with per-inbox tokens stored in the secret store"`.

**File:** `crates/gmail/src/auth.rs`

Update doc comments: replace "keychain" with "secret store" in module-level docs (lines 10, 12) and function-level docs (lines 36, 40, 45, 71, 77).

Update user-facing error messages at lines 212 and 225:
- Line 212: `"Store it with: threshold-core keychain or set GMAIL_OAUTH_CLIENT_ID env var"` → `"Set it via the web UI at /config/credentials or set GMAIL_OAUTH_CLIENT_ID env var"`
- Line 225: `"Store it with: threshold-core keychain or set GMAIL_OAUTH_CLIENT_SECRET env var"` → `"Set it via the web UI at /config/credentials or set GMAIL_OAUTH_CLIENT_SECRET env var"`

Rename `AuthError::KeychainError` display text: `#[error("Keychain error: {0}")]` → `#[error("Secret store error: {0}")]`. Keep the Rust variant name `KeychainError` to minimize diff (it's an internal name, but the `#[error]` string is user-facing via `thiserror::Error::fmt`).

**File:** `crates/core/src/config.rs` (line 44)

Update comment from `// bot_token resolved from keychain, NEVER stored here` to `// bot_token resolved from secret store, NEVER stored here`.

**File:** `readme.md`

Update all keychain references:
- Line 25: `"Credential manager (keychain-backed)"` → `"Credential manager (file-backed by default, keychain optional)"`
- Line 41: `"Secrets: macOS Keychain integration (env var fallback)"` → `"Secrets: File-based store (default) or OS keychain, with env var fallback"`
- Line 75: `"# Store bot token in keychain"` → `"# Store bot token in secret store"`
- Line 99: `"stored in the macOS Keychain"` → `"stored in the secret store (file-based by default, or OS keychain)"`

**File:** `SETUP.md` (line 21)

Change `"This will read the token from .env and store it in your macOS Keychain."` to `"This will read the token from .env and store it in the secret store."`

**File:** `scripts/setup-discord-token.sh`

Rewrite the script to write `discord-bot-token` into `secrets.toml` (file backend) by default, instead of macOS Keychain. The script should:
1. Read token from `.env` file (unchanged)
2. Accept optional `--data-dir <path>` argument (defaults to `~/.threshold`), matching the daemon's `data_dir` config
3. Write to `<data_dir>/secrets.toml` using TOML format: create `<data_dir>/` if needed, read existing `secrets.toml`, upsert the `discord-bot-token` key under `[secrets]`, write back with chmod 600
4. Keep the env var option as alternative (unchanged)
5. Remove the macOS `security` command path entirely (the whole point of this change)
6. Update all comments and output from "keychain" to "secret store"

## Verification

After all phases:
```bash
cargo test --workspace                   # catches all struct literal breakage, all tests
cargo build --workspace                  # redundant but explicit compilation check
```

Then Codex review of all changes.

## Codex Review Findings — Round 1 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | High | Silent fallback to keychain on file init failure | `new()` returns `Result`, fails explicitly |
| 2 | High | Mutex + HashMap not safe across processes | No in-memory cache; re-read file on every operation with lockfile |
| 3 | High | Gmail/ImageGen CLI ignore secret_backend config | Server wrappers create `SecretStore` from config and pass to CLI handlers |
| 4 | High | Adding field to ThresholdConfig breaks struct literals | All 7 struct literal sites updated with `secret_backend: None` |
| 5 | High | Security regression with plaintext file as default | 0600 permissions from creation, auto-chmod existing files, symlink rejection |
| 6 | Medium | Permission hardening incomplete (TOCTOU) | `OpenOptions::mode(0o600)` at creation |
| 7 | Medium | Memory/disk divergence on flush failure | No in-memory cache — always read from disk |
| 8 | Medium | Default path ignores data_dir config | `with_backend()` takes `data_dir` parameter |
| 9 | Medium | TOML format risks (special chars, ordering) | `BTreeMap` for ordering; TOML handles special chars natively |
| 10 | Medium | Missing tests for critical paths | Added: special chars, flush failure, symlink, concurrent, auto-chmod |
| 11 | Low | User-facing copy still says "keychain" | Updated error messages, README, config.example.toml |

## Codex Review Findings — Round 2 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | High | Cross-process stale reads from in-memory cache | Eliminated cache entirely — every operation re-reads from disk |
| 2 | High | `#[serde(default)]` doesn't fix struct literal breakage | Explicitly listed all 7 sites that need `secret_backend: None` |
| 3 | Medium | File lock + rename inode issue | Separate `.lock` lockfile instead of locking data file |
| 4 | Medium | Constructor error contract inconsistent | All constructors documented as returning `Result`, wiring examples use `?` |
| 5 | Medium | Existing insecure file permissions not fixed | Auto-chmod to 0600 on load (not just warn) |
| 6 | Medium | Missing key-character roundtrip tests | Added `special_chars_in_keys` test (gmail-style `@` `.` keys) |
| 7 | Medium | Verification commands miss struct literal breakage | Changed to `cargo test --workspace` (compiles all test code) |
| 8 | Low | Template copy still says "keychain" | Added `credentials.html` line 8 to update list |

## Codex Review Findings — Round 3 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | High | Unix-specific permissions not portable | All permission code gated with `#[cfg(unix)]`; non-Unix skips (relies on OS ACLs) |
| 2 | Medium | Constructor contract inconsistent with "API unchanged" claim | Clarified: `get/set/delete/resolve` unchanged; `new()` returns `Result`, `Default` removed; `with_keychain_backend`/`with_service_name` stay infallible |
| 3 | Medium | Keychain wording incomplete, will break E2E assertion | Added all remaining files: imagegen error, gmail auth docs, config comment, README, SETUP.md, setup script, e2e_playwright.sh assertion |
| 4 | Low | No server-level wiring tests | Thin wrappers covered by `cargo test --workspace` compilation + core unit tests |

## Codex Review Findings — Round 4 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Medium | Keychain wording still missing at `gmail/auth.rs:212,225` and `imagegen/client.rs:84` | Added these specific lines to the update list |
| 2 | Medium | `with_file_backend` path inconsistency (table says `data_dir`, Phase 3 says `data_dir.join("secrets.toml")`) | Fixed table to match Phase 3: `with_file_backend(data_dir.join("secrets.toml"))` |
| 3 | Low | `new_default_uses_file_backend` test would touch real `~/.threshold/` | Changed test to use `with_file_backend(tempdir)` instead of `new()` |
| 4 | Low | Plan references `README.md` but file is `readme.md` | Fixed all references to lowercase `readme.md` |

## Codex Review Findings — Round 5 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | High | Setup script still writes to keychain, not `secrets.toml` | Rewrote script plan: writes to `secrets.toml` instead of using macOS `security` command |
| 2 | Medium | `SecretStore::new()`/`Default` breakage undercounted (doctest, unit tests) | Added internal test sites (line 193, 205, doctest 132) to the update list |
| 3 | Low | Gmail error message replacement text leaves stale `threshold-core keychain` path | Specified exact replacement: "Set it via the web UI at /config/credentials or set ENV_VAR" |

## Codex Review Findings — Round 6 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Medium | Setup script table entry contradicts Phase 3 detail (keep vs remove keychain) | Fixed table: "remove macOS `security` command path" |
| 2 | Medium | `AuthError::KeychainError` display text is user-facing via thiserror | Rename `#[error]` string to "Secret store error: {0}" |
| 3 | Low | `with_file_backend(tempdir)` inconsistent with file-path contract | Fixed test to use `with_file_backend(tempdir.join("secrets.toml"))` |

## Codex Review Findings — Round 7 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Medium | Setup script hardcodes `~/.threshold/` but runtime uses `data_dir` | Script accepts `--data-dir <path>` argument, defaults to `~/.threshold` |
| 2 | Low | Keychain mode unnecessarily coupled to `data_dir` resolution | `with_backend()` takes `Option<PathBuf>` — `data_dir` only used for file backend, ignored for keychain |

## Codex Review Findings — Round 8 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Medium | Gmail/ImageGen wrappers unconditionally call `config.data_dir()?` even for keychain mode | Conditionally resolve `data_dir` only when `backend == File` |
| 2 | Low | `crates/gmail/src/lib.rs:14` has stale "stored in OS keychain" | Added to files-to-modify table and Phase 3 detail |
| 3 | Low | Inconsistent path `web/e2e_server.rs:77` vs `crates/web/tests/e2e_server.rs` | Fixed to full path |

## Codex Review Findings — Round 9 (all resolved)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | Medium | `SecretBackend` enum needs `PartialEq, Eq` for `==` comparison in wiring snippets | Added `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` to enum definition |
| 2 | Medium | Tests at `secrets.rs:193,199` assert `store.service_name` which won't exist after refactor | Rewrote test plan: assert `backend_name()` instead; remove `default()` test |

## Key Reuse

- Atomic write pattern: `crates/web/src/routes/config.rs:80-92` and `crates/scheduler/src/store.rs:60-73`
- Config validation pattern: `crates/core/src/config.rs:169-241` (existing `log_level` and `permission_mode` checks)
- `resolve_path()`: `crates/core/src/paths.rs:11-23` (tilde expansion)
- `dirs::home_dir()`: already a dependency in `crates/core/Cargo.toml`
- `toml` crate: already in `crates/core/Cargo.toml`
- `tempfile` crate: already in dev-dependencies for tests
- New dependency: `fs2 = "0.4"` for file locking

## Current SecretStore Consumers

1. **Daemon startup** (`crates/server/src/main.rs:93`): changes to `with_backend(backend, Some(data_dir))` + `?`
2. **Web credentials page** (`crates/web/src/routes/config.rs`): `.get()`, `.set()`, `.delete()` — **no API changes**
3. **Web AppState** (`crates/web/src/state.rs:18`): holds `Arc<SecretStore>` — **no changes**
4. **Web test setup** (`crates/web/src/lib.rs:144`): changes to `with_file_backend()`
5. **Gmail auth** (`crates/gmail/src/auth.rs`): `.resolve()`, `.get()`, `.set()` — **no API changes**
6. **Gmail client** (`crates/gmail/src/client.rs`): passes SecretStore — **no changes**
7. **Gmail CLI** (`crates/gmail/src/cli.rs:110`): changes to accept `Arc<SecretStore>` parameter
8. **ImageGen client** (`crates/imagegen/src/client.rs`): `.resolve()` — **no API changes**
9. **ImageGen CLI** (`crates/imagegen/src/cli.rs:58`): changes to accept `Arc<SecretStore>` parameter
