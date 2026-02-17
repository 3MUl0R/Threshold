# Milestone 6 — Unified Scheduler

**Crate:** `scheduler`
**Complexity:** Large
**Dependencies:** Milestone 1 (core), Milestone 2 (cli-wrapper), Milestone 3
(conversation), Milestone 4 (discord), Milestone 5 (tool framework)

## What This Milestone Delivers

A unified scheduling system that handles both **heartbeat** (autonomous agent
wake-ups) and **cron jobs** (user-defined recurring tasks). These are the
**same system under the hood** — a heartbeat is simply a pre-configured
scheduled task.

**This is the killer feature** — the AI can continue working on projects
overnight, push commits to branches, check email, run tests, and report
progress in the morning.

### Key Insight

A heartbeat is a scheduled task with:
- `ScheduledAction::ResumeConversation` (always resumes the same thread)
- A dedicated instruction file (heartbeat.md)
- A skip-if-running guard
- Handoff notes for continuity between runs

A cron job is a scheduled task with any `ScheduledAction` variant, created by
the user or by Claude during a conversation.

**One engine runs them all.**

### Use Cases

| Scenario | ScheduledAction | Notes |
|----------|----------------|-------|
| "Continue working on this project overnight" | ResumeConversation | Heartbeat pattern |
| "Check my email every hour" | NewConversation | Fresh conversation + gmail tool |
| "Run my backup script nightly" | Script | No Claude involved |
| "Run tests, tell me if anything fails" | ScriptThenConversation | Script output fed to Claude |
| "Monitor this API endpoint every 4 hours" | ScriptThenConversation | `curl` + analysis |

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                  Scheduler Engine                          │
│                                                           │
│  ┌─────────────┐  ┌──────────────────┐  ┌──────────────┐ │
│  │  Cron Loop   │  │  Command Channel  │  │  Daemon API  │ │
│  │  (60s tick)  │  │  (SchedulerHandle)│  │  (Unix sock) │ │
│  └──────┬──────┘  └────────┬─────────┘  └──────┬───────┘ │
│         │                  │                    │          │
│         ▼                  ▼                    ▼          │
│  ┌────────────────────────────────────────────────────┐   │
│  │                 Task Execution                      │   │
│  │                                                     │   │
│  │  NewConversation                                    │   │
│  │    → spawn claude CLI with prompt                   │   │
│  │                                                     │   │
│  │  ResumeConversation (heartbeat pattern)             │   │
│  │    → skip-if-running guard                          │   │
│  │    → load handoff notes + heartbeat.md              │   │
│  │    → ConversationEngine.send_to_conversation()      │   │
│  │    → save handoff notes from response               │   │
│  │                                                     │   │
│  │  Script                                             │   │
│  │    → ExecTool via ToolRegistry (audit logged)       │   │
│  │                                                     │   │
│  │  ScriptThenConversation                             │   │
│  │    → ExecTool, then feed output to Claude           │   │
│  └────────────────────────────────────────────────────┘   │
│                                                           │
│  Result delivery via trait:                                │
│    dyn ResultSender (defined in core)                      │
│    └── DiscordResultSender (impl in discord crate)        │
│                                                           │
│  ┌────────────────────────────────────────────────────┐   │
│  │  Persistence: ~/.threshold/state/schedules.json     │   │
│  └────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────┘

External interfaces:

  CLI: threshold schedule conversation|script|monitor|list|delete|toggle
       ↕ Unix socket ↕
       Daemon API → SchedulerHandle → Command Channel

  Discord: /schedule, /schedules, /unschedule, /heartbeat
       → SchedulerHandle → Command Channel
```

### Dependency Inversion

The scheduler needs to deliver results to Discord channels, but must not
depend on the discord crate directly (circular dependency). Solution:

1. Define `ResultSender` trait in `crates/core/src/types.rs`
2. Implement it in `crates/discord/` as `DiscordResultSender`
3. Inject `Arc<dyn ResultSender>` into the `Scheduler` at construction time
4. Wire it together in `crates/server/src/main.rs`

```rust
// crates/core/src/types.rs
#[async_trait]
pub trait ResultSender: Send + Sync {
    async fn send_to_channel(&self, channel_id: u64, message: &str) -> Result<()>;
    async fn send_dm(&self, user_id: u64, message: &str) -> Result<()>;
}

// crates/discord/src/outbound.rs
impl ResultSender for DiscordOutbound {
    async fn send_to_channel(&self, channel_id: u64, message: &str) -> Result<()> { ... }
    async fn send_dm(&self, user_id: u64, message: &str) -> Result<()> { ... }
}

// crates/server/src/main.rs — wiring
let outbound = Arc::new(DiscordOutbound::new(http));
let (scheduler, handle) = Scheduler::new(
    store_path,
    claude,
    tools,
    engine,
    Some(outbound as Arc<dyn ResultSender>),  // injected
    cancel,
).await?;
```

This keeps the dependency graph acyclic:
- `core` defines the trait (no dependencies)
- `scheduler` depends on `core` (uses the trait)
- `discord` depends on `core` (implements the trait)
- `server` depends on all three (wires them together)

---

## Core Types

### ScheduledAction (defined in `crates/core/src/types.rs`)

See Milestone 5 for the canonical definition. The four variants are:
`NewConversation`, `ResumeConversation`, `Script`, `ScriptThenConversation`.

### ScheduledTask (`crates/scheduler/src/task.rs`)

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use threshold_core::{ConversationId, PortalId, ScheduledAction};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: Uuid,
    pub name: String,
    pub cron_expression: String,
    pub action: ScheduledAction,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_result: Option<TaskRunResult>,
    pub next_run: Option<DateTime<Utc>>,

    /// What kind of task this is — used for identity, not behavior.
    #[serde(default)]
    pub kind: TaskKind,

    // Delivery target for standalone tasks
    pub delivery: DeliveryTarget,

    // Conversation context (for agent-created tasks)
    #[serde(default)]
    pub conversation_id: Option<ConversationId>,
    #[serde(default)]
    pub portal_id: Option<PortalId>,
    #[serde(default)]
    pub created_by_agent: bool,

    // Heartbeat-specific
    #[serde(default)]
    pub skip_if_running: bool,
    #[serde(default)]
    pub handoff_notes_path: Option<PathBuf>,
}

/// Explicit task identity — distinguishes heartbeats from user-created cron jobs.
/// This is preferable to inferring heartbeat status from `skip_if_running`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum TaskKind {
    /// User-created cron job (the default).
    #[default]
    Cron,
    /// The heartbeat task — created from config, has special semantics.
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeliveryTarget {
    /// Send results to a Discord channel.
    DiscordChannel { channel_id: u64 },
    /// Send results as a DM to a user.
    DiscordDm { user_id: u64 },
    /// Log only (conversation-attached tasks deliver via portal).
    AuditLogOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunResult {
    pub timestamp: DateTime<Utc>,
    pub success: bool,
    pub summary: String,
    pub duration_ms: u64,
}
```

---

## Phase 6.1 — Cron Parsing and Foundation

### Dependencies

```toml
[dependencies]
cron = "0.13"
chrono = "0.4"
uuid = { version = "1", features = ["v4"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
threshold-core = { path = "../core" }
threshold-cli-wrapper = { path = "../cli-wrapper" }
threshold-tools = { path = "../tools" }
threshold-conversation = { path = "../conversation" }
```

### Cron Format and Timezone

The `cron` crate uses **7-field** cron expressions (with seconds and year):

```
sec  min  hour  day  month  weekday  year
 0    0    3    *     *       *       *     → 3:00 AM every day
 0   */30  *    *     *       *       *     → every 30 minutes
```

**All times are UTC.** The scheduler computes next-run times using
`Utc::now()` and `schedule.upcoming(Utc)`. Users specify times in UTC.
Local timezone support is a future enhancement.

For convenience, the CLI also accepts the common **6-field** format
(without year). If 6 fields are provided, `*` is appended for year.
The CLI `--help` output documents this.

### Cron Utilities

```rust
use chrono::{DateTime, Utc};

/// Compute the next run time for a cron expression.
/// Re-exported from crate root for external access.
pub fn compute_next_run(cron_expr: &str) -> Option<DateTime<Utc>> {
    let normalized = normalize_cron(cron_expr);
    let schedule: cron::Schedule = normalized.parse().ok()?;
    schedule.upcoming(Utc).next()
}

/// Validate a cron expression without computing next run.
pub fn validate_cron(cron_expr: &str) -> Result<(), String> {
    let normalized = normalize_cron(cron_expr);
    normalized.parse::<cron::Schedule>()
        .map(|_| ())
        .map_err(|e| format!("Invalid cron expression: {}", e))
}

/// Normalize 6-field cron expressions to 7-field by appending year.
fn normalize_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() == 6 {
        format!("{} *", expr)
    } else {
        expr.to_string()
    }
}
```

---

## Phase 6.2 — Scheduler Engine

### Command Channel Pattern

The `SchedulerHandle` allows safe interaction with the scheduler from other
components (CLI daemon API, Discord commands, conversation engine) while the
scheduler's main loop runs.

```rust
/// Handle for interacting with the scheduler. Clone-able, Send + Sync.
#[derive(Clone)]
pub struct SchedulerHandle {
    command_tx: tokio::sync::mpsc::UnboundedSender<SchedulerCommand>,
}

enum SchedulerCommand {
    AddTask(ScheduledTask),
    RemoveTask(Uuid),
    ToggleTask { id: Uuid, enabled: bool },
    ListTasks(tokio::sync::oneshot::Sender<Vec<ScheduledTask>>),
}

impl SchedulerHandle {
    pub async fn add_task(&self, task: ScheduledTask) -> Result<()> {
        self.command_tx.send(SchedulerCommand::AddTask(task))
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        Ok(())
    }

    pub async fn remove_task(&self, id: Uuid) -> Result<()> {
        self.command_tx.send(SchedulerCommand::RemoveTask(id))
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        Ok(())
    }

    pub async fn toggle_task(&self, id: Uuid, enabled: bool) -> Result<()> {
        self.command_tx.send(SchedulerCommand::ToggleTask { id, enabled })
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        Ok(())
    }

    pub async fn list_tasks(&self) -> Result<Vec<ScheduledTask>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.command_tx.send(SchedulerCommand::ListTasks(tx))
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        rx.await.map_err(|_| ThresholdError::SchedulerShutdown)
    }
}
```

### Scheduler (Internal State)

```rust
pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
    running_tasks: Arc<RwLock<HashSet<Uuid>>>,  // For skip-if-running
    task_semaphore: Arc<tokio::sync::Semaphore>,  // Bounded concurrency (default: 4)
    store_path: PathBuf,
    claude: Arc<ClaudeClient>,
    tools: Arc<ToolRegistry>,
    engine: Arc<ConversationEngine>,
    result_sender: Option<Arc<dyn ResultSender>>,  // Trait from core — no discord dependency
    command_rx: tokio::sync::mpsc::UnboundedReceiver<SchedulerCommand>,
    cancel: CancellationToken,
}

impl Scheduler {
    pub async fn new(
        store_path: PathBuf,
        claude: Arc<ClaudeClient>,
        tools: Arc<ToolRegistry>,
        engine: Arc<ConversationEngine>,
        result_sender: Option<Arc<dyn ResultSender>>,
        cancel: CancellationToken,
    ) -> Result<(Self, SchedulerHandle)> {
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();

        let tasks = Self::load_tasks(&store_path).await.unwrap_or_else(|e| {
            tracing::warn!("Failed to load scheduler state: {}, starting fresh", e);
            Vec::new()
        });

        tracing::info!("Loaded {} scheduled tasks from disk", tasks.len());

        let scheduler = Self {
            tasks,
            running_tasks: Arc::new(RwLock::new(HashSet::new())),
            task_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            store_path,
            claude,
            tools,
            engine,
            result_sender,
            command_rx,
            cancel: cancel.clone(),
        };

        let handle = SchedulerHandle { command_tx };
        Ok((scheduler, handle))
    }
}
```

### Core Loop

```rust
impl Scheduler {
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.check_and_run().await;
                }
                Some(cmd) = self.command_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                _ = self.cancel.cancelled() => {
                    tracing::info!("Scheduler shutting down.");
                    break;
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: SchedulerCommand) {
        match cmd {
            SchedulerCommand::AddTask(task) => {
                tracing::info!("Adding scheduled task: {}", task.name);
                self.tasks.push(task);
                self.save().await.ok();
            }
            SchedulerCommand::RemoveTask(id) => {
                if let Some(pos) = self.tasks.iter().position(|t| t.id == id) {
                    let task = self.tasks.remove(pos);
                    tracing::info!("Removed scheduled task: {}", task.name);
                    self.save().await.ok();
                }
            }
            SchedulerCommand::ToggleTask { id, enabled } => {
                if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
                    task.enabled = enabled;
                    tracing::info!("Task '{}' {}", task.name,
                        if enabled { "enabled" } else { "disabled" });
                    self.save().await.ok();
                }
            }
            SchedulerCommand::ListTasks(reply_tx) => {
                let _ = reply_tx.send(self.tasks.clone());
            }
        }
    }

    /// Check for due tasks and spawn them with bounded concurrency.
    /// Uses a semaphore to prevent head-of-line blocking — tasks run
    /// concurrently up to the limit, so a long-running heartbeat
    /// doesn't block cron jobs from firing.
    async fn check_and_run(&mut self) {
        let now = Utc::now();

        let due_task_ids: Vec<Uuid> = self.tasks.iter()
            .filter(|task| task.enabled && task.next_run.map_or(false, |next| now >= next))
            .map(|task| task.id)
            .collect();

        if due_task_ids.is_empty() { return; }

        for task_id in due_task_ids {
            let task_snapshot = match self.tasks.iter().find(|t| t.id == task_id).cloned() {
                Some(task) => task,
                None => continue,
            };

            // Update next_run immediately (don't wait for execution)
            if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                task.next_run = compute_next_run(&task.cron_expression);
            }

            // Spawn task execution with bounded concurrency
            let semaphore = self.task_semaphore.clone();
            let executor = self.make_task_executor();
            tokio::spawn(async move {
                let _permit = semaphore.acquire().await;
                tracing::info!("Running scheduled task: {}", task_snapshot.name);
                let result = executor.execute_task(&task_snapshot).await;
                executor.deliver_result(&task_snapshot, &result).await;
                executor.record_result(task_snapshot.id, result).await;
            });
        }

        self.save().await.ok();
    }
}
```

---

## Phase 6.3 — Task Execution

All four `ScheduledAction` variants are handled here.

```rust
impl Scheduler {
    async fn execute_task(&self, task: &ScheduledTask) -> TaskRunResult {
        let start = Instant::now();

        // Skip-if-running guard (primarily for heartbeat tasks)
        if task.skip_if_running {
            let running = self.running_tasks.read().await;
            if running.contains(&task.id) {
                tracing::info!("Skipping task '{}': previous run still active", task.name);
                return TaskRunResult {
                    timestamp: Utc::now(),
                    success: true,
                    summary: "Skipped: previous run still active".to_string(),
                    duration_ms: 0,
                };
            }
            drop(running);
            self.running_tasks.write().await.insert(task.id);
        }

        let result = match &task.action {
            ScheduledAction::NewConversation { prompt, model } => {
                self.exec_new_conversation(prompt, model.as_deref()).await
            }
            ScheduledAction::ResumeConversation { conversation_id, prompt } => {
                self.exec_resume_conversation(task, conversation_id, prompt).await
            }
            ScheduledAction::Script { command, working_dir } => {
                self.exec_script(command, working_dir.as_deref()).await
            }
            ScheduledAction::ScriptThenConversation { command, prompt_template, model } => {
                self.exec_script_then_conversation(command, prompt_template, model.as_deref()).await
            }
        };

        // Clear running flag
        if task.skip_if_running {
            self.running_tasks.write().await.remove(&task.id);
        }

        let duration = start.elapsed();
        match result {
            Ok(output) => TaskRunResult {
                timestamp: Utc::now(),
                success: true,
                summary: truncate(&output, 2000),
                duration_ms: duration.as_millis() as u64,
            },
            Err(e) => TaskRunResult {
                timestamp: Utc::now(),
                success: false,
                summary: format!("Error: {}", e),
                duration_ms: duration.as_millis() as u64,
            },
        }
    }
}
```

### Action Handlers

```rust
impl Scheduler {
    /// Launch a fresh Claude conversation with the given prompt.
    async fn exec_new_conversation(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> Result<String> {
        let response = self.claude.send_message(
            Uuid::new_v4(),
            prompt,
            None,
            model,
        ).await?;
        Ok(response.text)
    }

    /// Resume an existing conversation. Used by heartbeat tasks.
    /// Loads handoff notes and heartbeat instructions, sends combined prompt.
    async fn exec_resume_conversation(
        &self,
        task: &ScheduledTask,
        conversation_id: &ConversationId,
        prompt: &str,
    ) -> Result<String> {
        // Build prompt with handoff notes if available
        let full_prompt = if let Some(notes_path) = &task.handoff_notes_path {
            let notes = self.load_handoff_notes(notes_path).await;
            build_heartbeat_prompt(prompt, &notes)
        } else {
            prompt.to_string()
        };

        // Send through conversation engine (broadcasts to portal listeners)
        let response = self.engine
            .send_to_conversation(conversation_id, &full_prompt)
            .await?;

        // Extract and save handoff notes from response
        if let Some(notes_path) = &task.handoff_notes_path {
            if let Some(notes) = extract_handoff_notes(&response) {
                self.save_handoff_notes(notes_path, &notes).await.ok();
            }
        }

        Ok(response)
    }

    /// Run a script directly via ExecTool (no Claude involved).
    async fn exec_script(
        &self,
        command: &str,
        working_dir: Option<&str>,
    ) -> Result<String> {
        let mut params = serde_json::json!({"command": command});
        if let Some(dir) = working_dir {
            params["working_dir"] = serde_json::Value::String(dir.to_string());
        }

        let ctx = self.build_tool_context();
        let result = self.tools.execute("exec", params, &ctx).await?;
        Ok(result.content)
    }

    /// Run a script, then feed output to Claude for analysis.
    async fn exec_script_then_conversation(
        &self,
        command: &str,
        prompt_template: &str,
        model: Option<&str>,
    ) -> Result<String> {
        // Step 1: Run the script
        let script_output = self.exec_script(command, None).await?;

        // Step 2: Build prompt with script output
        let prompt = prompt_template.replace("{output}", &script_output);

        // Step 3: Send to Claude
        self.exec_new_conversation(&prompt, model).await
    }

    fn build_tool_context(&self) -> ToolContext {
        ToolContext {
            conversation_id: None,
            portal_id: None,
            agent_id: "scheduler".to_string(),
            working_dir: std::env::current_dir().unwrap_or_default(),
            profile: ToolProfile::Full,
            permission_mode: ToolPermissionMode::FullAuto,
            cancellation: self.cancel.clone(),
        }
    }
}
```

### Result Delivery

```rust
impl Scheduler {
    async fn deliver_result(&self, task: &ScheduledTask, result: &TaskRunResult) {
        // Conversation-attached tasks deliver responses via the portal system
        // (ConversationEngine broadcasts AssistantMessage events)
        if task.conversation_id.is_some() {
            return;
        }

        // Standalone tasks: deliver to configured target
        let message = format!(
            "**Scheduled Task: {}**\n{}\n*Duration: {}ms*",
            task.name, result.summary, result.duration_ms,
        );

        match &task.delivery {
            DeliveryTarget::DiscordChannel { channel_id } => {
                if let Some(sender) = &self.result_sender {
                    sender.send_to_channel(*channel_id, &message).await.ok();
                }
            }
            DeliveryTarget::DiscordDm { user_id } => {
                if let Some(sender) = &self.result_sender {
                    sender.send_dm(*user_id, &message).await.ok();
                }
            }
            DeliveryTarget::AuditLogOnly => {}
        }
    }
}
```

---

## Phase 6.4 — Heartbeat Features

The heartbeat is the most important use case for the scheduler. These features
make it work well for autonomous agent sessions.

### Handoff Notes

File: `~/.threshold/state/heartbeat-notes.md`

Handoff notes give the AI continuity between heartbeat cycles. At the start
of each cycle, notes from the previous run are loaded into the prompt. At the
end, the AI writes new notes that are extracted and saved.

```rust
impl Scheduler {
    async fn load_handoff_notes(&self, path: &Path) -> Option<String> {
        tokio::fs::read_to_string(path).await.ok()
    }

    async fn save_handoff_notes(&self, path: &Path, notes: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, notes).await?;
        Ok(())
    }
}

/// Extract handoff notes from Claude's response.
/// Looks for a "## Handoff Notes" section.
fn extract_handoff_notes(response: &str) -> Option<String> {
    if let Some(idx) = response.find("## Handoff Notes") {
        let notes = &response[idx + "## Handoff Notes".len()..];
        let notes = notes.trim();
        if !notes.is_empty() {
            return Some(notes.to_string());
        }
    }
    None
}

/// Build the heartbeat prompt combining instructions and handoff notes.
fn build_heartbeat_prompt(instructions: &str, handoff_notes: &Option<String>) -> String {
    let mut prompt = String::new();

    prompt.push_str("## Heartbeat Instructions\n\n");
    prompt.push_str(instructions);
    prompt.push_str("\n\n");

    if let Some(notes) = handoff_notes {
        prompt.push_str("## Notes From Previous Heartbeat\n\n");
        prompt.push_str(notes);
        prompt.push_str("\n\n");
    }

    prompt.push_str(
        "## Your Job Right Now\n\n\
         Review the instructions above. Decide what to work on. Execute any \
         needed actions. When you're done (or need to pause), write handoff \
         notes explaining what you did and what should happen next.\n\n\
         Format your handoff notes in a section starting with `## Handoff Notes`."
    );

    prompt
}
```

### Task Store (for heartbeat work items)

A simple file-backed task list that the heartbeat reads and updates.

```rust
// crates/scheduler/src/task_store.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: Uuid,
    pub description: String,
    pub status: WorkItemStatus,
    pub priority: u32,
    pub project: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkItemStatus {
    Pending,
    InProgress,
    Completed,
    Blocked { reason: String },
}

pub struct TaskStore {
    path: PathBuf,  // ~/.threshold/state/tasks.json
    items: Vec<WorkItem>,
}

impl TaskStore {
    pub async fn load(path: &Path) -> Result<Self>;
    pub async fn save(&self) -> Result<()>;
    pub fn add(&mut self, description: &str, priority: u32) -> &WorkItem;
    pub fn update_status(&mut self, id: &Uuid, status: WorkItemStatus) -> Result<()>;
    pub fn list_pending(&self) -> Vec<&WorkItem>;
    pub fn list_all(&self) -> Vec<&WorkItem>;
}
```

Work items can be created by:
- The user via Discord commands (`/task add "..."`)
- The heartbeat itself (Claude creates subtasks)
- The CLI (`threshold task add "..."`)

### Example heartbeat.md

```markdown
# Heartbeat Instructions

You are the autonomous agent for the Threshold project. When you wake up,
review your handoff notes from your previous session.

## Standing Orders

1. Check if there are any pending tasks. Prioritize by priority number.
2. For coding tasks: always work in a feature branch, run tests, write
   clear commit messages.
3. If you're blocked, note it in your handoff notes.
4. If you finish all tasks, review the project for improvements or TODOs.

## Safety Rules

- Never force-push to any branch
- Never delete branches you didn't create
- Never modify production configuration

## Reporting

Write a brief summary of what you accomplished in your handoff notes.
```

---

## Phase 6.5 — Persistence

```rust
// crates/scheduler/src/store.rs
impl Scheduler {
    async fn load_tasks(store_path: &PathBuf) -> Result<Vec<ScheduledTask>> {
        if !store_path.exists() {
            return Ok(Vec::new());
        }

        let data = tokio::fs::read_to_string(store_path).await?;
        let tasks: Vec<ScheduledTask> = serde_json::from_str(&data)?;
        Ok(tasks)
    }

    async fn save(&self) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let data = serde_json::to_string_pretty(&self.tasks)?;
        tokio::fs::write(&self.store_path, data).await?;
        Ok(())
    }
}
```

### Migration

The `#[serde(default)]` attributes on newer fields ensure backward
compatibility when loading tasks from older versions:
- `conversation_id: None` → standalone task
- `skip_if_running: false` → no guard
- `handoff_notes_path: None` → no handoff notes

---

## Phase 6.6 — Daemon API and CLI Subcommands

### IPC Protocol Specification

The daemon communicates with CLI clients via a Unix domain socket at
`~/.threshold/threshold.sock`. The protocol is newline-delimited JSON
(NDJSON) with a simple request-response pattern.

**Socket path:** `~/.threshold/threshold.sock` (Unix 0600 permissions)

**Framing:** Each message is a single JSON object terminated by `\n`.
No length prefix — the newline delimiter is sufficient because JSON
objects don't contain bare newlines.

```
CLIENT → DAEMON: {"version":1,"command":"ScheduleList"}\n
DAEMON → CLIENT: {"version":1,"status":"ok","data":[...]}\n
```

**Protocol version:** All messages include a `version` field (integer).
The daemon rejects requests with unsupported versions with a clear error.
Starting at version 1. Bump version only for breaking changes.

**Error schema:**
```json
{"version": 1, "status": "error", "code": "not_found", "message": "Task 'abc' not found"}
```

Error codes: `not_found`, `invalid_input`, `internal`, `version_mismatch`,
`scheduler_shutdown`.

**Stale socket handling:** On daemon startup, if `threshold.sock` already
exists:
1. Attempt to connect to it
2. If connection succeeds → another daemon is running → exit with error
3. If connection fails → stale socket → delete it and bind fresh

**Client timeout:** CLI clients set a 5-second timeout on socket connect
and a 30-second timeout on response read.

### Daemon API

The threshold daemon exposes the JSON-over-Unix-socket API described
above. The `threshold schedule` CLI commands communicate with the running
scheduler through this API.

```rust
// crates/scheduler/src/daemon_api.rs
use tokio::net::UnixListener;

pub struct DaemonApi {
    scheduler: SchedulerHandle,
    socket_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub version: u32,
    pub command: DaemonCommand,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonCommand {
    ScheduleCreate(ScheduledTask),
    ScheduleList,
    ScheduleDelete { id: String },
    ScheduleToggle { id: String, enabled: bool },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub version: u32,
    pub status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ResponseStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
}

impl DaemonApi {
    pub async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)?;

        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => {
                    let handle = self.scheduler.clone();
                    tokio::spawn(async move {
                        Self::handle_connection(stream, handle).await.ok();
                    });
                }
                _ = cancel.cancelled() => break,
            }
        }

        // Clean up socket file
        tokio::fs::remove_file(&self.socket_path).await.ok();
        Ok(())
    }

    async fn handle_connection(
        stream: UnixStream,
        scheduler: SchedulerHandle,
    ) -> Result<()> {
        // Read command JSON, dispatch to SchedulerHandle, write response JSON
        todo!()
    }
}
```

### CLI Subcommands

The `threshold schedule` CLI commands (defined in Milestone 5's CLI skeleton)
connect to the daemon API to manage tasks.

```rust
// crates/server/src/schedule.rs — uses action-specific subcommands from Phase 5.4
pub async fn handle_schedule_command(cmd: ScheduleCommands) -> Result<()> {
    let client = DaemonClient::new();

    match cmd {
        ScheduleCommands::Conversation { name, cron, prompt, model } => {
            let task = build_task(name, cron, ScheduledAction::NewConversation { prompt, model });
            let response = client.send_command(&DaemonCommand::ScheduleCreate(task)).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }

        ScheduleCommands::Script { name, cron, command, working_dir } => {
            let task = build_task(name, cron, ScheduledAction::Script { command, working_dir });
            let response = client.send_command(&DaemonCommand::ScheduleCreate(task)).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }

        ScheduleCommands::Monitor { name, cron, command, prompt_template, model } => {
            let task = build_task(name, cron, ScheduledAction::ScriptThenConversation {
                command, prompt_template, model,
            });
            let response = client.send_command(&DaemonCommand::ScheduleCreate(task)).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }

        ScheduleCommands::List { format } => {
            let response = client.send_command(&DaemonCommand::ScheduleList).await?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&response)?),
                OutputFormat::Table => print_task_table(&response),
            }
        }

        ScheduleCommands::Delete { id } => {
            let response = client.send_command(&DaemonCommand::ScheduleDelete { id }).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }

        ScheduleCommands::Toggle { id, enabled } => {
            let response = client.send_command(
                &DaemonCommand::ScheduleToggle { id, enabled }
            ).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
    }

    Ok(())
}

fn build_task(name: String, cron: String, action: ScheduledAction) -> ScheduledTask {
    ScheduledTask {
        id: Uuid::new_v4(),
        name,
        cron_expression: cron.clone(),
        action,
        enabled: true,
        created_at: Utc::now(),
        last_run: None,
        last_result: None,
        next_run: compute_next_run(&cron),
        kind: TaskKind::Cron,
        delivery: DeliveryTarget::AuditLogOnly,
        conversation_id: None,
        portal_id: None,
        created_by_agent: false,
        skip_if_running: false,
        handoff_notes_path: None,
    }
}
```

---

## Phase 6.7 — Discord Commands

### Prerequisites

Add `SchedulerHandle` to Discord bot data:

```rust
// crates/discord/src/bot.rs
pub struct BotData {
    pub engine: Arc<ConversationEngine>,
    pub config: DiscordConfig,
    pub outbound: Arc<DiscordOutbound>,
    pub scheduler: SchedulerHandle,
}
```

### Commands

```rust
// crates/discord/src/commands/schedule.rs

/// Create a recurring scheduled task.
///
/// The `action_type` uses Discord slash command choices to present a clean
/// dropdown. This maps to the same ScheduledAction variants used by the CLI.
#[poise::command(slash_command)]
pub async fn schedule(
    ctx: Context<'_>,
    #[description = "Task name"] name: String,
    #[description = "Cron expression (e.g. '0 0 3 * * *' for 3 AM daily)"] cron: String,
    #[description = "Action type"]
    #[rename = "action"]
    action_type: ScheduleActionChoice,
    #[description = "Prompt text or shell command"] value: String,
    #[description = "Model override (optional)"] model: Option<String>,
) -> Result<(), Error> {
    let scheduler = &ctx.data().scheduler;
    let channel_id = ctx.channel_id().get();

    let action = match action_type {
        ScheduleActionChoice::Conversation => ScheduledAction::NewConversation {
            prompt: value,
            model,
        },
        ScheduleActionChoice::Script => ScheduledAction::Script {
            command: value,
            working_dir: None,
        },
        ScheduleActionChoice::Monitor => ScheduledAction::ScriptThenConversation {
            command: value,
            prompt_template: "Script output:\n{output}\n\nAnalyze and report any issues."
                .to_string(),
            model,
        },
    };

    let task = ScheduledTask {
        id: Uuid::new_v4(),
        name: name.clone(),
        cron_expression: cron.clone(),
        action,
        enabled: true,
        created_at: Utc::now(),
        last_run: None,
        last_result: None,
        next_run: compute_next_run(&cron),
        kind: TaskKind::Cron,
        delivery: DeliveryTarget::DiscordChannel { channel_id },
        conversation_id: None,
        portal_id: None,
        created_by_agent: false,
        skip_if_running: false,
        handoff_notes_path: None,
    };

    scheduler.add_task(task.clone()).await?;

    ctx.say(format!(
        "Created scheduled task '{}' ({})\nNext run: {}",
        name, cron,
        task.next_run.map_or("unknown".to_string(), |t| t.to_rfc3339())
    )).await?;

    Ok(())
}

/// Discord slash command choices for schedule action types.
#[derive(Debug, Clone, poise::ChoiceParameter)]
pub enum ScheduleActionChoice {
    /// Launch a new Claude conversation
    Conversation,
    /// Run a shell command
    Script,
    /// Run a command, then analyze output with Claude
    Monitor,
}

/// List all scheduled tasks.
#[poise::command(slash_command)]
pub async fn schedules(ctx: Context<'_>) -> Result<(), Error> {
    let tasks = ctx.data().scheduler.list_tasks().await?;
    if tasks.is_empty() {
        ctx.say("No scheduled tasks.").await?;
        return Ok(());
    }

    let mut response = String::from("**Scheduled Tasks:**\n\n");
    for task in tasks {
        response.push_str(&format!(
            "- **{}** | `{}` | {} | Next: {}\n",
            task.name,
            task.cron_expression,
            if task.enabled { "enabled" } else { "disabled" },
            task.next_run.map_or("unknown".to_string(), |t| t.to_rfc3339()),
        ));
    }
    ctx.say(response).await?;
    Ok(())
}

/// Heartbeat controls.
#[poise::command(slash_command)]
pub async fn heartbeat(
    ctx: Context<'_>,
    #[description = "Action: status, trigger, pause, resume"] action: String,
) -> Result<(), Error> {
    // Heartbeat is just a scheduled task with skip_if_running = true
    // and a ResumeConversation action. These commands find and manage it.
    let scheduler = &ctx.data().scheduler;
    let tasks = scheduler.list_tasks().await?;
    let heartbeat = tasks.iter().find(|t| t.kind == TaskKind::Heartbeat);

    match action.as_str() {
        "status" => {
            if let Some(hb) = heartbeat {
                ctx.say(format!(
                    "**Heartbeat:** {}\nEnabled: {}\nLast run: {}\nNext run: {}",
                    hb.name,
                    hb.enabled,
                    hb.last_run.map_or("never".to_string(), |t| t.to_rfc3339()),
                    hb.next_run.map_or("unknown".to_string(), |t| t.to_rfc3339()),
                )).await?;
            } else {
                ctx.say("No heartbeat configured.").await?;
            }
        }
        "pause" => {
            if let Some(hb) = heartbeat {
                scheduler.toggle_task(hb.id, false).await?;
                ctx.say("Heartbeat paused.").await?;
            }
        }
        "resume" => {
            if let Some(hb) = heartbeat {
                scheduler.toggle_task(hb.id, true).await?;
                ctx.say("Heartbeat resumed.").await?;
            }
        }
        _ => ctx.say("Usage: /heartbeat status|pause|resume").await?,
    }
    Ok(())
}
```

---

## Pre-Phase: Blocking Dependencies

### ConversationEngine Enhancement

`send_to_conversation()` must broadcast `AssistantMessage` events so portal
listeners receive responses from scheduled tasks:

```rust
// In ConversationEngine::send_to_conversation
let _ = self.event_tx.send(ConversationEvent::AssistantMessage {
    conversation_id: *conversation_id,
    content: response.text.clone(),
    artifacts: Vec::new(),
    usage: response.usage,
    timestamp: Utc::now(),
});
```

Without this, conversation-attached tasks (heartbeats, agent-created cron
jobs) won't deliver responses to Discord.

### ScheduledAction in Core

The `ScheduledAction` enum must be added to `crates/core/src/types.rs` before
this milestone begins. See Milestone 5 for the definition.

---

## Crate Module Structure

```
crates/scheduler/src/
  lib.rs            — re-exports: Scheduler, SchedulerHandle, ScheduledTask, etc.
  task.rs           — ScheduledTask, DeliveryTarget, TaskRunResult
  engine.rs         — Scheduler, SchedulerHandle, main loop
  execution.rs      — task execution logic (all ScheduledAction variants)
  heartbeat.rs      — handoff notes, heartbeat prompt building, skip-if-running
  task_store.rs     — WorkItem task store for heartbeat work items
  store.rs          — persistence (load/save schedules.json)
  daemon_api.rs     — Unix socket API for CLI communication
  cron_utils.rs     — compute_next_run, validate_cron
```

---

## Configuration

```toml
# In threshold.toml
[scheduler]
enabled = true
store_path = "~/.threshold/state/schedules.json"

[heartbeat]
enabled = true
interval_minutes = 30
instruction_file = "heartbeat.md"
handoff_notes_path = "~/.threshold/state/heartbeat-notes.md"
conversation_id = "..."     # Set after first run, or auto-created
skip_if_running = true
```

The heartbeat config is syntactic sugar — it creates a `ScheduledTask` with
`ScheduledAction::ResumeConversation` on startup.

---

## Verification Checklist

### Cron Parsing (Phase 6.1)
- [ ] Unit test: valid cron expressions parse correctly
- [ ] Unit test: invalid cron expressions return error
- [ ] Unit test: compute_next_run returns future time
- [ ] Unit test: ScheduledTask serialization round-trip

### Scheduler Engine (Phase 6.2)
- [ ] Unit test: SchedulerHandle add/remove/toggle/list via command channel
- [ ] Unit test: SchedulerHandle operations fail after scheduler shutdown
- [ ] Unit test: check_and_run finds due tasks and executes them
- [ ] Unit test: disabled tasks are skipped
- [ ] Integration test: scheduler respects cancellation token

### Task Execution (Phase 6.3)
- [ ] Unit test: NewConversation spawns Claude with prompt
- [ ] Unit test: ResumeConversation sends through ConversationEngine
- [ ] Unit test: Script runs command via ExecTool
- [ ] Unit test: ScriptThenConversation chains script output to Claude
- [ ] Unit test: skip-if-running guard prevents concurrent execution
- [ ] Integration test: full task lifecycle (create → fire → deliver)

### Heartbeat Features (Phase 6.4)
- [ ] Unit test: handoff notes load/save
- [ ] Unit test: extract_handoff_notes parses response correctly
- [ ] Unit test: build_heartbeat_prompt includes all sections
- [ ] Unit test: handoff notes survive across cycles
- [ ] Unit test: WorkItem CRUD operations
- [ ] Integration test: full heartbeat cycle with handoff notes

### Persistence (Phase 6.5)
- [ ] Unit test: save/load round-trip
- [ ] Unit test: missing file returns empty list
- [ ] Unit test: backward-compatible deserialization (missing new fields)

### Daemon API (Phase 6.6)
- [ ] Unit test: DaemonCommand serialization
- [ ] Integration test: CLI create → daemon receives → scheduler adds
- [ ] Integration test: CLI list → returns current tasks
- [ ] Integration test: socket cleanup on shutdown

### Discord Commands (Phase 6.7)
- [ ] Integration test: /schedule creates task
- [ ] Integration test: /schedules lists tasks
- [ ] Integration test: /heartbeat status shows heartbeat info
- [ ] Integration test: /heartbeat pause/resume toggles task
