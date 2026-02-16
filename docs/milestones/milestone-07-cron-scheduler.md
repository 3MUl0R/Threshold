# Milestone 7 — Cron Scheduler

**Crate:** `scheduler`
**Complexity:** Medium
**Dependencies:** Milestone 1 (core), Milestone 2 (cli-wrapper), Milestone 4
(discord for delivery), Milestone 5 (tools)

## What This Milestone Delivers

A cron-based scheduled task system with **conversation awareness**. Tasks can be:

1. **Conversation-attached**: Created by agents via natural language, maintaining
   conversation context and memory across executions
2. **Standalone**: Created via slash commands, executing independently

### Example Use Cases

**Conversation-Attached Tasks:**
- User: "Check my email every hour and let me know if there's anything important"
- Agent creates task within the conversation, responses flow naturally through the portal

**Standalone Tasks:**
- `/schedule name:"Test Runner" cron:"0 0 3 * * *" action:command value:"cargo test"`
- Results delivered to a specific Discord channel

### Key Features

- Tasks maintain conversation context when attached to a conversation
- Agents can create/manage tasks via tool calls (ScheduleTask tool)
- Execution routes through ConversationEngine for conversation-attached tasks
- Responses flow through portal system as ConversationEvent::AssistantMessage
- Both modes supported: conversation-attached and standalone

---

## Architecture: Two Task Modes

### Conversation-Attached Tasks

**Creation:** Agent uses `schedule_task` tool in conversation
**Execution:** Runs through `ConversationEngine.send_to_conversation()`
**Context:** Maintains conversation history and memory
**Delivery:** Engine broadcasts `ConversationEvent::AssistantMessage` → portal listeners receive it
**Use Case:** "Check my email every hour and let me know"

```
User → Agent → schedule_task tool → Scheduler
  ↓                                      ↓
Portal ← ConversationEvent ← ConversationEngine ← Cron fires
```

**Portal Routing Policy:**

When a conversation-attached task executes:

1. Scheduler calls `engine.send_to_conversation(conversation_id, prompt)`
2. Engine broadcasts `ConversationEvent::AssistantMessage` with the `conversation_id`
3. **All portals listening to that conversation receive the event** (broadcast semantics)
4. The `portal_id` field in `ScheduledTask` is **metadata only** — it records which portal the task was created from, useful for auditing and debugging
5. If a conversation has multiple active portals (e.g., Discord + web interface), **all receive the response**

This follows the same routing as user-initiated messages: responses go to all portals attached to the conversation, not just the originating portal. This ensures consistent behavior and allows tasks to be visible across all interfaces.

### Standalone Tasks

**Creation:** User uses `/schedule` slash command
**Execution:** Fresh Claude conversation (no context)
**Context:** None (stateless execution)
**Delivery:** Direct message to Discord channel/DM via `DiscordOutbound`
**Use Case:** "Run cargo test every night at 3am"

```
User → /schedule → Scheduler
                      ↓
           Cron fires → Fresh Claude call → DiscordOutbound → Channel
```

### Key Differences

| Aspect | Conversation-Attached | Standalone |
|--------|----------------------|------------|
| Context | Maintains conversation history | Fresh conversation each time |
| Memory | Agent remembers previous runs | No memory between runs |
| Delivery | Through portal (natural flow) | Direct Discord message |
| Created by | Agent tool call | User slash command |
| conversation_id | Set to active conversation | None |
| Use case | Ongoing monitoring/assistance | One-off automated tasks |

---

## Pre-Phase: Core Dependencies

**⚠️ BLOCKING DEPENDENCIES - Must be completed before Phase 7.1**

### Error Variant Addition

**File**: `crates/core/src/error.rs`

Add new error variants for scheduler operations:

```rust
#[error("Scheduler is shutting down")]
SchedulerShutdown,

#[error("I/O error at {path}: {message}")]
IoError { path: String, message: String },

#[error("Serialization error: {message}")]
SerializationError { message: String },

#[error("Not found: {message}")]
NotFound { message: String },

#[error("Invalid input: {message}")]
InvalidInput { message: String },
```

### Enhance ConversationEngine for Scheduler Integration

**File**: `crates/conversation/src/engine.rs`

**⚠️ CRITICAL BLOCKING ISSUE:**

Currently, `send_to_conversation()` does NOT broadcast `AssistantMessage` events, which means portal listeners won't receive responses from scheduled tasks. Without this fix, conversation-attached tasks will be completely non-functional. This enhancement is **mandatory** before Milestone 7 implementation can begin.

### Option 1: Enhance send_to_conversation (Preferred)

Modify `send_to_conversation` to broadcast `AssistantMessage` events after sending:

```rust
pub async fn send_to_conversation(
    &self,
    conversation_id: &ConversationId,
    content: &str,
) -> Result<String> {
    // ... existing lookup and send logic ...

    // NEW: Broadcast AssistantMessage event so portal listeners receive it
    match self.event_tx.send(ConversationEvent::AssistantMessage {
        conversation_id: *conversation_id,
        content: response.text.clone(),
        artifacts: Vec::new(),
        usage: response.usage,
        timestamp: Utc::now(),
    }) {
        Ok(receiver_count) => {
            tracing::debug!(receiver_count, "scheduled message broadcast");
        }
        Err(_) => {
            tracing::warn!("no receivers for scheduled message");
        }
    }

    Ok(response.text)
}
```

### Verification

- [ ] Update `send_to_conversation` to broadcast events
- [ ] Test that heartbeat/cron messages reach portal listeners
- [ ] Verify conversation context is maintained

---

## Phase 7.1 — Cron Expression Parsing

Use the `cron` crate for standard cron expression support.

### `crates/scheduler/Cargo.toml` key deps

```toml
[dependencies]
cron = "0.13"
chrono = "0.4"
threshold-core = { path = "../core" }
threshold-cli-wrapper = { path = "../cli-wrapper" }
threshold-tools = { path = "../tools" }
```

### `crates/scheduler/src/task.rs`

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use threshold_core::{ConversationId, PortalId};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: Uuid,
    pub name: String,
    pub cron_expression: String,        // "0 30 7 * * MON-FRI"
    pub action: ScheduledAction,
    pub delivery: DeliveryTarget,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_result: Option<TaskRunResult>,
    pub next_run: Option<DateTime<Utc>>,

    // Conversation awareness (NEW)
    // These fields use #[serde(default)] for safe migration from old persisted tasks
    #[serde(default)]
    pub conversation_id: Option<ConversationId>,  // If set, task runs within this conversation
    #[serde(default)]
    pub portal_id: Option<PortalId>,              // Metadata: which portal created the task (not used for routing)
    #[serde(default)]
    pub created_by_agent: bool,                   // True if created via agent tool call
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScheduledAction {
    /// Run a shell command and return its output.
    ShellCommand { command: String },

    /// Send a prompt to Claude and return the response.
    ClaudePrompt {
        prompt: String,
        model: Option<String>,
    },

    /// Fetch a URL, then ask Claude to analyze the content.
    WebCheck {
        url: String,
        prompt: String,          // "Tell me if the price dropped below $50"
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeliveryTarget {
    /// Send results to a Discord channel.
    DiscordChannel { channel_id: u64 },

    /// Send results as a DM to a user.
    DiscordDm { user_id: u64 },

    /// Log only (no notification).
    AuditLogOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunResult {
    pub timestamp: DateTime<Utc>,
    pub success: bool,
    pub summary: String,         // Brief result summary
    pub duration_ms: u64,
}
```

### Cron Expression Examples

| Expression | Meaning |
|------------|---------|
| `0 30 7 * * MON-FRI` | 7:30 AM, weekdays |
| `0 0 9 * * *` | 9:00 AM, every day |
| `0 */30 * * * *` | Every 30 minutes |
| `0 0 */4 * * *` | Every 4 hours |
| `0 0 22 * * SUN` | 10:00 PM, Sundays |

---

## Phase 7.2 — Scheduler Engine

### `crates/scheduler/src/engine.rs`

```rust
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use chrono::{DateTime, Utc};
use serde_json::json;
use tokio::time;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use threshold_core::{Result, ThresholdError};
use threshold_cli_wrapper::ClaudeClient;
use threshold_tools::ToolRegistry;
use threshold_conversation::ConversationEngine;
use crate::task::{ScheduledTask, ScheduledAction, DeliveryTarget, TaskRunResult};
use crate::discord_outbound::DiscordOutbound;
```

#### SchedulerHandle (Command Channel Pattern)

To allow safe task creation while the scheduler is running, we use a command channel:

```rust
/// Handle for interacting with the scheduler from other components (e.g., tools).
/// Internally uses a command channel to communicate with the scheduler loop.
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

#### Scheduler (Internal State)

```rust
pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
    store_path: PathBuf,              // ~/.threshold/state/schedules.json
    claude: Arc<ClaudeClient>,
    tools: Arc<ToolRegistry>,
    discord_outbound: Option<Arc<DiscordOutbound>>,
    engine: Arc<ConversationEngine>,
    command_rx: tokio::sync::mpsc::UnboundedReceiver<SchedulerCommand>,
    cancel: CancellationToken,        // For graceful shutdown and task cancellation
}

impl Scheduler {
    /// Create a new scheduler and return both the scheduler and a handle for it.
    /// Loads existing tasks from disk if they exist.
    pub async fn new(
        store_path: PathBuf,
        claude: Arc<ClaudeClient>,
        tools: Arc<ToolRegistry>,
        discord_outbound: Option<Arc<DiscordOutbound>>,
        engine: Arc<ConversationEngine>,
        cancel: CancellationToken,
    ) -> Result<(Self, SchedulerHandle)> {
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();

        // Load existing tasks from disk
        let tasks = Self::load_tasks(&store_path).await.unwrap_or_else(|e| {
            tracing::warn!("Failed to load scheduler state: {}, starting fresh", e);
            Vec::new()
        });

        tracing::info!("Loaded {} scheduled tasks from disk", tasks.len());

        let scheduler = Self {
            tasks,
            store_path,
            claude,
            tools,
            discord_outbound,
            engine,
            command_rx,
            cancel: cancel.clone(),
        };

        let handle = SchedulerHandle { command_tx };

        Ok((scheduler, handle))
    }

    /// Build a ToolContext for standalone task execution (no conversation context).
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

### Core Loop

```rust
impl Scheduler {
    /// Main loop. Checks every 60 seconds for tasks due to run, and processes commands.
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

    /// Handle commands sent via SchedulerHandle.
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
                    tracing::info!(
                        "Task '{}' {}",
                        task.name,
                        if enabled { "enabled" } else { "disabled" }
                    );
                    self.save().await.ok();
                }
            }
            SchedulerCommand::ListTasks(reply_tx) => {
                let _ = reply_tx.send(self.tasks.clone());
            }
        }
    }

    async fn check_and_run(&mut self) {
        let now = Utc::now();

        // Phase 1: Collect IDs of tasks that are due (prevents index invalidation).
        let due_task_ids: Vec<Uuid> = self.tasks.iter()
            .filter(|task| {
                task.enabled && task.next_run.map_or(false, |next| now >= next)
            })
            .map(|task| task.id)
            .collect();

        if due_task_ids.is_empty() { return; }

        // Phase 2: Execute each due task by ID (safe against concurrent modifications).
        for task_id in due_task_ids {
            // Find and clone the task
            let task_snapshot = match self.tasks.iter().find(|t| t.id == task_id).cloned() {
                Some(task) => task,
                None => {
                    tracing::warn!("Task {} disappeared before execution", task_id);
                    continue;
                }
            };

            tracing::info!("Running scheduled task: {}", task_snapshot.name);

            let result = self.execute_task(&task_snapshot).await;
            self.deliver_result(&task_snapshot, &result).await;

            // Phase 3: Update the task by ID after execution completes.
            if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                task.last_run = Some(now);
                task.last_result = Some(result);
                task.next_run = compute_next_run(&task.cron_expression);
            }
        }

        self.save().await
            .map_err(|e| tracing::warn!("Failed to save scheduler state: {}", e))
            .ok();
    }
}

/// Compute the next run time for a cron expression.
/// Public utility function used by both Scheduler and ScheduleTaskTool.
/// Re-exported from crate root for external access.
pub fn compute_next_run(cron_expr: &str) -> Option<DateTime<Utc>> {
    let schedule: cron::Schedule = cron_expr.parse().ok()?;
    schedule.upcoming(Utc).next()
}
```

---

## Phase 7.3 — Task Execution

### `crates/scheduler/src/execution.rs`

```rust
use std::time::Instant;
use chrono::Utc;
use serde_json::json;
use threshold_core::Result;
use threshold_tools::ToolContext;
use crate::task::{ScheduledTask, ScheduledAction, TaskRunResult};

/// Truncate a string to a maximum length, adding "... [truncated]" if needed.
/// Ensures UTF-8 character boundaries are respected to avoid panics.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        // Find valid UTF-8 boundary
        let mut boundary = max_len;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}... [truncated]", &s[..boundary])
    }
}

impl Scheduler {
    async fn execute_task(&self, task: &ScheduledTask) -> TaskRunResult {
        let start = Instant::now();

        // Check if this is a conversation-attached task
        let result = if let Some(conv_id) = task.conversation_id {
            // Execute within conversation context - response flows through portal
            self.execute_conversation_task(task, conv_id).await
        } else {
            // Execute standalone - return result directly
            self.execute_standalone_task(task).await
        };

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

    /// Execute a conversation-attached task through the conversation engine.
    /// The response will flow through the portal system as ConversationEvent::AssistantMessage.
    /// Returns the agent's response text for inclusion in TaskRunResult.
    async fn execute_conversation_task(
        &self,
        task: &ScheduledTask,
        conv_id: ConversationId,
    ) -> Result<String> {
        let prompt = match &task.action {
            ScheduledAction::ClaudePrompt { prompt, .. } => prompt.clone(),
            ScheduledAction::ShellCommand { command } => {
                // Run command, then ask agent to summarize if needed
                let ctx = self.build_tool_context();
                let result = self.tools.execute("exec", json!({"command": command}), &ctx).await?;
                format!("Command output:\n{}\n\nSummarize if there's anything important.", result.content)
            }
            ScheduledAction::WebCheck { url, prompt } => {
                let ctx = self.build_tool_context();
                let fetch = self.tools.execute(
                    "web_fetch",
                    json!({"url": url, "extract_text": true}),
                    &ctx,
                ).await?;
                format!("Content from {}:\n{}\n\nTask: {}", url, fetch.content, prompt)
            }
        };

        // Send through conversation engine - response broadcasts to portal listeners
        // and is also returned for TaskRunResult summary
        let response = self.engine.send_to_conversation(&conv_id, &prompt).await?;
        Ok(response)
    }

    /// Execute a standalone task (no conversation context).
    async fn execute_standalone_task(&self, task: &ScheduledTask) -> Result<String> {
        match &task.action {
            ScheduledAction::ShellCommand { command } => {
                self.run_shell_command(command).await
            }
            ScheduledAction::ClaudePrompt { prompt, model } => {
                self.run_claude_prompt(prompt, model.as_deref()).await
            }
            ScheduledAction::WebCheck { url, prompt } => {
                self.run_web_check(url, prompt).await
            }
        }
    }

    async fn run_shell_command(&self, command: &str) -> Result<String> {
        let ctx = self.build_tool_context();
        let result = self.tools.execute("exec", json!({"command": command}), &ctx).await?;
        Ok(result.content)
    }

    async fn run_claude_prompt(&self, prompt: &str, model: Option<&str>) -> Result<String> {
        // Each cron run gets a fresh conversation (no session reuse)
        let response = self.claude.send_message(
            Uuid::new_v4(),
            prompt,
            None,
            model,
        ).await?;
        Ok(response.text)
    }

    async fn run_web_check(&self, url: &str, prompt: &str) -> Result<String> {
        // Step 1: Fetch the URL
        let ctx = self.build_tool_context();
        let fetch = self.tools.execute(
            "web_fetch",
            json!({"url": url, "extract_text": true}),
            &ctx,
        ).await?;

        // Step 2: Ask Claude to analyze
        let analysis_prompt = format!(
            "I fetched the following content from {}:\n\n{}\n\nTask: {}",
            url, fetch.content, prompt
        );
        let response = self.claude.send_message(
            Uuid::new_v4(),
            &analysis_prompt,
            None,
            Some("haiku"),  // Use efficient model for analysis
        ).await?;

        Ok(response.text)
    }
}
```

### Result Delivery

```rust
impl Scheduler {
    async fn deliver_result(&self, task: &ScheduledTask, result: &TaskRunResult) {
        // For conversation-attached tasks, responses already flow through
        // the portal system as ConversationEvent::AssistantMessage, so we
        // only need to deliver standalone task results here.
        if task.conversation_id.is_some() {
            tracing::debug!(
                "Task '{}' is conversation-attached, response delivered via portal",
                task.name
            );
            return;
        }

        // Standalone task delivery
        let message = format!(
            "**Scheduled Task: {}**\n{}\n*Duration: {}ms*",
            task.name,
            result.summary,
            result.duration_ms,
        );

        match &task.delivery {
            DeliveryTarget::DiscordChannel { channel_id } => {
                if let Some(outbound) = &self.discord_outbound {
                    outbound.send_to_channel(*channel_id, &message).await.ok();
                }
            }
            DeliveryTarget::DiscordDm { user_id } => {
                if let Some(outbound) = &self.discord_outbound {
                    outbound.send_dm(*user_id, &message).await.ok();
                }
            }
            DeliveryTarget::AuditLogOnly => {
                // Already logged in audit trail by the execution pipeline
            }
        }
    }
}
```

---

## Phase 7.4 — Agent Tool for Task Creation

### `crates/tools/src/builtin/schedule_task.rs`

Agents can create conversation-attached scheduled tasks via this tool. Uses `SchedulerHandle` to avoid blocking the scheduler's main loop.

```rust
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use threshold_core::{Result, ThresholdError};
use threshold_tools::{Tool, ToolContext, ToolResult};
use threshold_scheduler::{SchedulerHandle, ScheduledTask, ScheduledAction, DeliveryTarget, compute_next_run};

pub struct ScheduleTaskTool {
    scheduler: SchedulerHandle,
}

#[async_trait]
impl Tool for ScheduleTaskTool {
    fn name(&self) -> &str { "schedule_task" }

    fn description(&self) -> &str {
        "Create a recurring scheduled task that will execute within this conversation. \
         Supports prompts, shell commands, and web checks. Use this when the user asks \
         you to do something periodically (e.g., 'check my email every hour', 'run tests \
         every night', 'watch this webpage for price changes')."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Short descriptive name for the task"
                },
                "cron_expression": {
                    "type": "string",
                    "description": "Cron expression (e.g., '0 0 9 * * *' for 9am daily)"
                },
                "action_type": {
                    "type": "string",
                    "enum": ["prompt", "command", "webcheck"],
                    "description": "Type of action: 'prompt' (ask Claude), 'command' (run shell command), or 'webcheck' (fetch URL and analyze)"
                },
                "action_value": {
                    "type": "string",
                    "description": "The prompt text, shell command, or URL depending on action_type"
                },
                "action_extra": {
                    "type": "string",
                    "description": "For 'webcheck' only: the analysis prompt (e.g., 'tell me if the price is below $50')"
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Whether the task starts enabled (default: true)"
                }
            },
            "required": ["name", "cron_expression", "action_type", "action_value"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let name = params["name"].as_str()
            .ok_or_else(|| ThresholdError::ToolError {
                tool: "schedule_task".into(),
                message: "Missing 'name' parameter".into(),
            })?;
        let cron = params["cron_expression"].as_str()
            .ok_or_else(|| ThresholdError::ToolError {
                tool: "schedule_task".into(),
                message: "Missing 'cron_expression' parameter".into(),
            })?;
        let action_type = params["action_type"].as_str()
            .ok_or_else(|| ThresholdError::ToolError {
                tool: "schedule_task".into(),
                message: "Missing 'action_type' parameter".into(),
            })?;
        let action_value = params["action_value"].as_str()
            .ok_or_else(|| ThresholdError::ToolError {
                tool: "schedule_task".into(),
                message: "Missing 'action_value' parameter".into(),
            })?;
        let action_extra = params["action_extra"].as_str();
        let enabled = params["enabled"].as_bool().unwrap_or(true);

        // Validate cron expression
        let _schedule: cron::Schedule = cron.parse()
            .map_err(|e| ThresholdError::ToolError {
                tool: "schedule_task".into(),
                message: format!("Invalid cron expression: {}", e),
            })?;

        // IMPORTANT: Require conversation context - tool only works within conversations
        let conversation_id = ctx.conversation_id
            .ok_or_else(|| ThresholdError::ToolError {
                tool: "schedule_task".into(),
                message: "This tool can only be used within a conversation context".into(),
            })?;

        // Build the action based on type
        let action = match action_type {
            "prompt" => ScheduledAction::ClaudePrompt {
                prompt: action_value.to_string(),
                model: None,
            },
            "command" => ScheduledAction::ShellCommand {
                command: action_value.to_string(),
            },
            "webcheck" => {
                let prompt = action_extra.ok_or_else(|| ThresholdError::ToolError {
                    tool: "schedule_task".into(),
                    message: "action_extra (analysis prompt) required for webcheck action".into(),
                })?;
                ScheduledAction::WebCheck {
                    url: action_value.to_string(),
                    prompt: prompt.to_string(),
                }
            },
            _ => return Err(ThresholdError::ToolError {
                tool: "schedule_task".into(),
                message: format!("Invalid action_type '{}'. Must be 'prompt', 'command', or 'webcheck'", action_type),
            }),
        };

        // Create conversation-attached task
        let task = ScheduledTask {
            id: Uuid::new_v4(),
            name: name.to_string(),
            cron_expression: cron.to_string(),
            action,
            delivery: DeliveryTarget::AuditLogOnly,  // Response flows through conversation
            enabled,
            created_at: Utc::now(),
            last_run: None,
            last_result: None,
            next_run: compute_next_run(cron),
            conversation_id: Some(conversation_id),  // Attach to current conversation
            portal_id: ctx.portal_id,                 // Metadata: which portal created the task
            created_by_agent: true,
        };

        // Add to scheduler via handle (non-blocking)
        self.scheduler.add_task(task.clone()).await?;

        Ok(ToolResult {
            content: format!(
                "Created scheduled task '{}' ({}) with cron expression '{}'. \
                 Next run: {}",
                task.name,
                action_type,
                task.cron_expression,
                task.next_run.map_or("unknown".to_string(), |t| t.to_rfc3339())
            ),
            artifacts: vec![],
            success: true,
        })
    }
}
```

### Usage Examples

**Example 1: Prompt-based task**

User: "Can you check my email every hour and let me know if there's anything important?"

Agent uses the tool:
```json
{
  "tool": "schedule_task",
  "params": {
    "name": "Email Check",
    "cron_expression": "0 0 * * * *",
    "action_type": "prompt",
    "action_value": "Check the user's email inbox and report if there are any important or urgent messages.",
    "enabled": true
  }
}
```

**Example 2: Shell command task**

User: "Run the test suite every night at 3am and let me know if anything fails"

Agent uses the tool:
```json
{
  "tool": "schedule_task",
  "params": {
    "name": "Nightly Tests",
    "cron_expression": "0 0 3 * * *",
    "action_type": "command",
    "action_value": "cd /projects/myapp && cargo test",
    "enabled": true
  }
}
```

**Example 3: Web check task**

User: "Watch this product page and let me know if the price drops below $50"

Agent uses the tool:
```json
{
  "tool": "schedule_task",
  "params": {
    "name": "Price Watch",
    "cron_expression": "0 0 */4 * * *",
    "action_type": "webcheck",
    "action_value": "https://example.com/product/12345",
    "action_extra": "Tell me if the price has dropped below $50",
    "enabled": true
  }
}
```

**Result for all examples:**
- Task created and attached to the current conversation
- At scheduled time, the task executes within the conversation context
- Response flows through the portal to the Discord channel
- Conversation memory is maintained across executions

---

## Phase 7.6 — Persistence (Save/Load)

### `crates/scheduler/src/store.rs`

```rust
use std::path::PathBuf;
use serde_json;
use threshold_core::Result;
use crate::task::ScheduledTask;

impl Scheduler {
    /// Load tasks from disk. Returns empty vector if file doesn't exist.
    async fn load_tasks(store_path: &PathBuf) -> Result<Vec<ScheduledTask>> {
        if !store_path.exists() {
            return Ok(Vec::new());
        }

        let data = tokio::fs::read_to_string(store_path).await
            .map_err(|e| ThresholdError::IoError {
                path: store_path.display().to_string(),
                message: format!("Failed to read scheduler state: {}", e),
            })?;

        let tasks: Vec<ScheduledTask> = serde_json::from_str(&data)
            .map_err(|e| ThresholdError::SerializationError {
                message: format!("Failed to parse scheduler state: {}", e),
            })?;

        Ok(tasks)
    }

    /// Save tasks to disk. Creates parent directories if needed.
    async fn save(&self) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.store_path.parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| ThresholdError::IoError {
                    path: parent.display().to_string(),
                    message: format!("Failed to create state directory: {}", e),
                })?;
        }

        let data = serde_json::to_string_pretty(&self.tasks)
            .map_err(|e| ThresholdError::SerializationError {
                message: format!("Failed to serialize scheduler state: {}", e),
            })?;

        tokio::fs::write(&self.store_path, data).await
            .map_err(|e| ThresholdError::IoError {
                path: self.store_path.display().to_string(),
                message: format!("Failed to write scheduler state: {}", e),
            })?;

        Ok(())
    }
}
```

### Migration Strategy

When loading existing tasks from disk, the `#[serde(default)]` attributes on new fields ensure backward compatibility:

- `conversation_id: None` → Treated as standalone task
- `portal_id: None` → No portal metadata
- `created_by_agent: false` → Created manually (via Discord or CLI)

This allows seamless upgrades without manual migration steps.

---

## Phase 7.7 — Discord Commands

### Prerequisites

#### Update BotData Structure

**File**: `crates/discord/src/bot.rs`

Add `SchedulerHandle` to the bot data so commands can access the scheduler:

```rust
pub struct BotData {
    pub engine: Arc<ConversationEngine>,
    pub config: DiscordConfig,
    pub outbound: Arc<DiscordOutbound>,
    pub scheduler: SchedulerHandle,  // ✅ Add this field
}
```

#### Update build_and_start Signature

```rust
pub async fn build_and_start(
    engine: Arc<ConversationEngine>,
    config: DiscordConfig,
    scheduler: SchedulerHandle,  // ✅ Add this parameter
    token: &str,
    cancel: CancellationToken,
) -> Result<Arc<DiscordOutbound>> {
    // ... implementation
    let bot_data = BotData {
        engine,
        config,
        outbound: outbound.clone(),
        scheduler,  // ✅ Include in bot data
    };
    // ...
}
```

### `crates/discord/src/commands/schedule.rs`

```rust
/// Create a recurring scheduled task (standalone mode).
#[poise::command(slash_command)]
pub async fn schedule(
    ctx: Context<'_>,
    #[description = "Task name"] name: String,
    #[description = "Cron expression (e.g., '0 30 7 * * MON-FRI')"] cron: String,
    #[description = "Action: command, prompt, or webcheck"] action_type: String,
    #[description = "Value (command string, prompt text, or URL)"] value: String,
    #[description = "Extra (prompt for webcheck)"] extra: Option<String>,
) -> Result<(), ThresholdError> {
    // Get scheduler handle from context
    let scheduler = &ctx.data().scheduler;

    // Get current channel ID for delivery
    let channel_id = ctx.channel_id().get();

    // Parse action type and build ScheduledAction
    let action = match action_type.as_str() {
        "command" => ScheduledAction::ShellCommand { command: value },
        "prompt" => ScheduledAction::ClaudePrompt { prompt: value, model: None },
        "webcheck" => {
            let prompt = extra.ok_or_else(|| ThresholdError::InvalidInput {
                message: "extra parameter required for webcheck action".into(),
            })?;
            ScheduledAction::WebCheck { url: value, prompt }
        },
        _ => return Err(ThresholdError::InvalidInput {
            message: format!("Invalid action_type '{}'. Must be 'command', 'prompt', or 'webcheck'", action_type),
        }),
    };

    // Validate cron expression
    let _schedule: cron::Schedule = cron.parse()
        .map_err(|e| ThresholdError::InvalidInput {
            message: format!("Invalid cron expression: {}", e),
        })?;

    // Create standalone task
    let task = ScheduledTask {
        id: Uuid::new_v4(),
        name: name.clone(),
        cron_expression: cron.clone(),
        action,
        delivery: DeliveryTarget::DiscordChannel { channel_id },
        enabled: true,
        created_at: Utc::now(),
        last_run: None,
        last_result: None,
        next_run: compute_next_run(&cron),
        conversation_id: None,  // Standalone task
        portal_id: None,
        created_by_agent: false,
    };

    // Add to scheduler
    scheduler.add_task(task.clone()).await?;

    // Respond with confirmation
    ctx.say(format!(
        "✅ Created scheduled task '{}' with cron expression '{}'\nNext run: {}",
        name,
        cron,
        task.next_run.map_or("unknown".to_string(), |t| t.to_rfc3339())
    )).await?;

    Ok(())
}

/// List all scheduled tasks.
#[poise::command(slash_command)]
pub async fn schedules(ctx: Context<'_>) -> Result<(), ThresholdError> {
    let scheduler = &ctx.data().scheduler;
    let tasks = scheduler.list_tasks().await?;

    if tasks.is_empty() {
        ctx.say("No scheduled tasks configured.").await?;
        return Ok(());
    }

    let mut response = String::from("**Scheduled Tasks:**\n\n");
    for task in tasks {
        response.push_str(&format!(
            "• **{}** ({})\n  Cron: `{}`\n  Next run: {}\n  Enabled: {}\n\n",
            task.name,
            if task.created_by_agent { "agent" } else { "manual" },
            task.cron_expression,
            task.next_run.map_or("unknown".to_string(), |t| t.to_rfc3339()),
            if task.enabled { "✅" } else { "❌" }
        ));
    }

    ctx.say(response).await?;
    Ok(())
}

/// Remove a scheduled task.
#[poise::command(slash_command)]
pub async fn unschedule(
    ctx: Context<'_>,
    #[description = "Task name or ID"] name_or_id: String,
) -> Result<(), ThresholdError> {
    let scheduler = &ctx.data().scheduler;

    // Try parsing as UUID first, then fall back to name match
    let task_id = if let Ok(uuid) = Uuid::parse_str(&name_or_id) {
        uuid
    } else {
        // Find by name
        let tasks = scheduler.list_tasks().await?;
        tasks.iter()
            .find(|t| t.name.eq_ignore_ascii_case(&name_or_id))
            .map(|t| t.id)
            .ok_or_else(|| ThresholdError::NotFound {
                message: format!("No task found with name '{}'", name_or_id),
            })?
    };

    scheduler.remove_task(task_id).await?;
    ctx.say(format!("✅ Removed scheduled task '{}'", name_or_id)).await?;
    Ok(())
}

/// Enable or disable a scheduled task.
#[poise::command(slash_command)]
pub async fn schedule_toggle(
    ctx: Context<'_>,
    #[description = "Task name or ID"] name_or_id: String,
    #[description = "Enable or disable"] enabled: bool,
) -> Result<(), ThresholdError> {
    let scheduler = &ctx.data().scheduler;

    // Try parsing as UUID first, then fall back to name match
    let task_id = if let Ok(uuid) = Uuid::parse_str(&name_or_id) {
        uuid
    } else {
        let tasks = scheduler.list_tasks().await?;
        tasks.iter()
            .find(|t| t.name.eq_ignore_ascii_case(&name_or_id))
            .map(|t| t.id)
            .ok_or_else(|| ThresholdError::NotFound {
                message: format!("No task found with name '{}'", name_or_id),
            })?
    };

    scheduler.toggle_task(task_id, enabled).await?;
    ctx.say(format!(
        "✅ Task '{}' {}",
        name_or_id,
        if enabled { "enabled" } else { "disabled" }
    )).await?;
    Ok(())
}
```

### Usage Examples

```
/schedule name:"Morning Email" cron:"0 30 7 * * *" action_type:prompt value:"Check my email inbox and give me a summary of anything important."
/schedule name:"Test Runner" cron:"0 0 3 * * *" action_type:command value:"cd /projects/myapp && cargo test"
/schedule name:"Price Watch" cron:"0 0 */4 * * *" action_type:webcheck value:"https://example.com/product" extra:"Tell me if the price is below $50"
/schedules
/unschedule name:"Price Watch"
```

---

## Crate Module Structure

```
crates/scheduler/src/
  lib.rs            — re-exports and public API
  task.rs           — ScheduledTask, ScheduledAction, DeliveryTarget types
  engine.rs         — Scheduler, SchedulerHandle, main loop, compute_next_run
  execution.rs      — task execution logic
  store.rs          — persistence (load/save schedules.json)
```

### `crates/scheduler/src/lib.rs`

```rust
pub use task::{ScheduledTask, ScheduledAction, DeliveryTarget, TaskRunResult};
pub use engine::{Scheduler, SchedulerHandle, compute_next_run};
```

This allows external crates (like `threshold-tools`) to import with:
```rust
use threshold_scheduler::{SchedulerHandle, compute_next_run};
```

---

## Verification Checklist

### Core Functionality
- [ ] Unit test: cron expression parsing and next-run computation
- [ ] Unit test: task CRUD operations (add, remove, toggle)
- [ ] Unit test: ShellCommand action executes correctly
- [ ] Unit test: ClaudePrompt action sends to Claude and returns response
- [ ] Unit test: WebCheck action fetches URL then analyzes with Claude
- [ ] Unit test: disabled tasks are skipped
- [ ] Integration test: schedule a task, advance time, verify it fires
- [ ] Integration test: task state persists across restarts
- [ ] Integration test: scheduler respects cancellation token

### Command Channel Pattern
- [ ] Unit test: SchedulerHandle.add_task() sends command while scheduler is running
- [ ] Unit test: SchedulerHandle.list_tasks() returns correct snapshot via oneshot channel
- [ ] Unit test: SchedulerHandle operations fail gracefully after scheduler shutdown
- [ ] Integration test: concurrent command channel operations don't interfere with task execution

### Conversation-Attached Tasks (NEW)
- [ ] Unit test: ScheduleTaskTool creates task with conversation_id and portal_id
- [ ] Unit test: ScheduleTaskTool validates cron expression
- [ ] Unit test: conversation-attached task executes through ConversationEngine
- [ ] Unit test: conversation-attached task maintains conversation context
- [ ] Integration test: agent creates task via natural language, task fires, response flows to portal
- [ ] Integration test: conversation-attached task with shell command action
- [ ] Integration test: conversation-attached task with web check action
- [ ] Integration test: multiple tasks attached to different conversations don't interfere

### Standalone Tasks
- [ ] Integration test: standalone task result delivery to Discord channel
- [ ] Integration test: standalone task result delivery as DM
- [ ] Integration test: standalone task creates fresh Claude conversation

### Agent Tool Integration
- [ ] Integration test: agent uses schedule_task tool in conversation
- [ ] Integration test: tool returns descriptive result with next run time
- [ ] Integration test: created task persists and executes correctly
