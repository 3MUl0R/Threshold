# Milestone 6 — Heartbeat System

**Crate:** `heartbeat`
**Complexity:** Medium
**Dependencies:** Milestone 1 (core), Milestone 2 (cli-wrapper), Milestone 5 (tools)

## What This Milestone Delivers

Periodic autonomous wake-ups that turn Threshold from "an assistant that
responds to messages" into "an agent that works on its own." The heartbeat
reads instructions, checks its task list, decides what to work on, executes
actions, and leaves notes for its next wake-up.

**This is the killer feature** — the AI can continue working on projects
overnight, push commits to branches, and report progress in the morning.

---

## Phase 6.1 — Heartbeat Runner

### `crates/heartbeat/src/runner.rs`

```rust
pub struct HeartbeatRunner {
    claude: Arc<ClaudeClient>,
    tools: Arc<ToolRegistry>,
    config: HeartbeatConfig,
    data_dir: PathBuf,
    running: Arc<AtomicBool>,
    discord_outbound: Option<Arc<DiscordOutbound>>,
}
```

### The Skip-If-Running Guard

This is critical. If the AI kicks off a multi-hour coding session during one
heartbeat, subsequent heartbeats must NOT fire while it's still working.

```rust
impl HeartbeatRunner {
    pub async fn run(&self, cancel: CancellationToken) {
        let interval_secs = self.config.interval_minutes.unwrap_or(30) * 60;
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.tick().await;
                }
                _ = cancel.cancelled() => {
                    tracing::info!("Heartbeat shutting down.");
                    break;
                }
            }
        }
    }

    async fn tick(&self) {
        // Atomic compare-and-swap: only proceed if not already running
        if self.running.compare_exchange(
            false, true,
            Ordering::SeqCst, Ordering::SeqCst
        ).is_err() {
            tracing::info!("Skipping heartbeat: previous cycle still running.");
            return;
        }

        // Drop guard resets `running` to false when this scope exits
        let _guard = RunningGuard(self.running.clone());

        match self.execute_heartbeat().await {
            Ok(()) => tracing::info!("Heartbeat cycle completed."),
            Err(e) => tracing::error!("Heartbeat cycle failed: {}", e),
        }
    }
}

/// RAII guard that resets the AtomicBool on drop.
struct RunningGuard(Arc<AtomicBool>);
impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
```

---

## Phase 6.2 — Heartbeat Execution Logic

Each heartbeat cycle follows this sequence:

```
1. Read heartbeat.md instructions
2. Read previous handoff notes (if any)
3. Read task store (pending tasks)
4. Build a prompt combining all three
5. Send to Claude CLI (dedicated heartbeat conversation)
6. Parse response for actions and notes
7. Execute any tool actions
8. Save handoff notes for next cycle
9. Update task statuses
10. Notify Discord (if configured)
11. Write audit trail entry
```

### Building the Heartbeat Prompt

```rust
fn build_heartbeat_prompt(
    &self,
    instructions: &str,
    handoff_notes: &Option<String>,
    tasks: &[Task],
) -> String {
    let mut prompt = String::new();

    prompt.push_str("## Heartbeat Instructions\n\n");
    prompt.push_str(instructions);
    prompt.push_str("\n\n");

    if let Some(notes) = handoff_notes {
        prompt.push_str("## Notes From Previous Heartbeat\n\n");
        prompt.push_str(notes);
        prompt.push_str("\n\n");
    }

    if !tasks.is_empty() {
        prompt.push_str("## Current Task List\n\n");
        for task in tasks {
            prompt.push_str(&format!(
                "- [{}] {} (priority: {}){}\n",
                match task.status {
                    TaskStatus::Pending => " ",
                    TaskStatus::InProgress => "~",
                    TaskStatus::Completed => "x",
                    TaskStatus::Blocked { .. } => "!",
                },
                task.description,
                task.priority,
                task.notes.as_ref().map(|n| format!(" — {}", n)).unwrap_or_default(),
            ));
        }
        prompt.push_str("\n");
    }

    prompt.push_str("## Your Job Right Now\n\n");
    prompt.push_str(
        "Review the instructions and task list above. Decide what to work on. \
         Execute any needed actions. When you're done (or need to pause), write \
         handoff notes explaining what you did and what should happen next.\n\n\
         Format your handoff notes in a section starting with `## Handoff Notes`."
    );

    prompt
}
```

### Parsing the Response

The heartbeat response from Claude may contain:
- Regular text (status updates, reasoning)
- Tool calls (executed by the CLI internally)
- A `## Handoff Notes` section (extracted and saved for next cycle)

```rust
fn extract_handoff_notes(response: &str) -> Option<String> {
    // Find "## Handoff Notes" header and extract everything after it
    if let Some(idx) = response.find("## Handoff Notes") {
        let notes = &response[idx + "## Handoff Notes".len()..];
        let notes = notes.trim();
        if !notes.is_empty() {
            return Some(notes.to_string());
        }
    }
    None
}
```

---

## Phase 6.3 — Task Store

A simple file-backed task list.

### `crates/heartbeat/src/tasks.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub description: String,
    pub status: TaskStatus,
    pub priority: u32,           // Lower = higher priority
    pub project: Option<String>, // Associated project/conversation
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Blocked { reason: String },
}

pub struct TaskStore {
    path: PathBuf,               // ~/.threshold/state/tasks.json
    tasks: Vec<Task>,
}

impl TaskStore {
    pub async fn load(path: &Path) -> Result<Self>;
    pub async fn save(&self) -> Result<()>;

    pub fn add(&mut self, description: &str, priority: u32) -> &Task;
    pub fn update_status(&mut self, id: &Uuid, status: TaskStatus) -> Result<()>;
    pub fn update_notes(&mut self, id: &Uuid, notes: &str) -> Result<()>;
    pub fn list_pending(&self) -> Vec<&Task>;
    pub fn list_in_progress(&self) -> Vec<&Task>;
    pub fn list_all(&self) -> Vec<&Task>;
    pub fn remove(&mut self, id: &Uuid) -> Result<()>;
}
```

### Usage

Tasks can be created by:
- **The user** via Discord commands (`/task add "Refactor auth module"`)
- **The heartbeat** itself (Claude decides to create subtasks)
- **The cron scheduler** (scheduled task creation)

---

## Phase 6.4 — Handoff Notes

### File: `~/.threshold/state/heartbeat-notes.md`

This is a simple markdown file that gives the AI continuity between heartbeat
cycles. At the start of each cycle, the notes are read and included in the
prompt. At the end, the AI writes new notes.

Example handoff notes:

```markdown
## What I Did
- Cloned the repo for project-alpha
- Set up the development environment (Rust toolchain, dependencies)
- Started implementing the authentication module
- Got stuck on the OAuth callback handler — need to research the redirect flow

## What To Do Next
- Research OAuth redirect flow for Discord
- Implement the callback handler
- Run the test suite after implementation

## Blockers
- None currently, but the Google API key isn't configured yet (needed for
  milestone 9 features)
```

---

## Phase 6.5 — Discord Integration

The heartbeat reports results to a configured Discord channel.

```rust
impl HeartbeatRunner {
    async fn notify_discord(&self, summary: &str) -> Result<()> {
        if let (Some(outbound), Some(channel_id)) =
            (&self.discord_outbound, self.config.notification_channel_id)
        {
            let message = format!("**Heartbeat Report**\n\n{}", summary);
            outbound.send_to_channel(channel_id, &message).await?;
        }
        Ok(())
    }
}
```

### Heartbeat Discord Commands

```
/heartbeat status     — Is it running? When was the last cycle?
/heartbeat trigger    — Fire a heartbeat right now (manual trigger)
/heartbeat pause      — Pause the heartbeat
/heartbeat resume     — Resume the heartbeat
/tasks                — List all tasks
/task add <desc>      — Add a task
/task done <id>       — Mark a task complete
```

---

## Example `heartbeat.md`

This file contains standing instructions for the heartbeat:

```markdown
# Heartbeat Instructions

You are the autonomous agent for the Threshold project. When you wake up,
review your task list and handoff notes from your previous session.

## Standing Orders

1. Check if there are any pending tasks. Prioritize by priority number.
2. For coding tasks:
   - Always work in a feature branch (never commit to main)
   - Run tests before committing
   - Write clear commit messages
3. If you're blocked on something, note it in your handoff notes.
4. If you finish all tasks, review the project for improvements or TODOs.

## Safety Rules

- Never force-push to any branch
- Never delete branches you didn't create
- Never modify production configuration
- If unsure about a destructive action, skip it and note it for human review

## Reporting

At the end of your session, write a brief summary of what you accomplished
in your handoff notes.
```

---

## Crate Module Structure

```
crates/heartbeat/src/
  lib.rs            — re-exports HeartbeatRunner
  runner.rs         — HeartbeatRunner with skip-if-running guard
  execution.rs      — heartbeat execution logic (prompt building, parsing)
  tasks.rs          — TaskStore
  handoff.rs        — handoff notes read/write
```

---

## Verification Checklist

- [ ] Unit test: skip-if-running guard (concurrent tick attempts)
- [ ] Unit test: RunningGuard resets on drop (even on panic)
- [ ] Unit test: TaskStore CRUD operations
- [ ] Unit test: handoff notes read/write
- [ ] Unit test: heartbeat prompt construction includes all sections
- [ ] Unit test: handoff notes extraction from response
- [ ] Integration test: full heartbeat cycle with Claude CLI
- [ ] Integration test: handoff notes survive across cycles
- [ ] Integration test: task store persists across restarts
- [ ] Integration test: Discord notification delivery
- [ ] Integration test: manual trigger fires immediately
- [ ] Integration test: heartbeat respects cancellation token for clean shutdown
