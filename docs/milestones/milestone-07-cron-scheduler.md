# Milestone 7 — Cron Scheduler

**Crate:** `scheduler`
**Complexity:** Medium
**Dependencies:** Milestone 1 (core), Milestone 2 (cli-wrapper), Milestone 4
(discord for delivery), Milestone 5 (tools)

## What This Milestone Delivers

A cron-based scheduled task system. Users create recurring tasks that execute
autonomously and deliver results to Discord. Examples:

- "Check my email every morning and tell me if there's anything important"
- "Run `cargo test` on project-alpha every night and report failures"
- "Fetch this webpage every hour and tell me if the price drops below $50"

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
pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
    store_path: PathBuf,              // ~/.threshold/state/schedules.json
    claude: Arc<ClaudeClient>,
    tools: Arc<ToolRegistry>,
    discord_outbound: Option<Arc<DiscordOutbound>>,
}
```

### Core Loop

```rust
impl Scheduler {
    /// Main loop. Checks every 60 seconds for tasks due to run.
    pub async fn run(&mut self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.check_and_run().await;
                }
                _ = cancel.cancelled() => {
                    tracing::info!("Scheduler shutting down.");
                    break;
                }
            }
        }
    }

    async fn check_and_run(&mut self) {
        let now = Utc::now();

        // Phase 1: Collect indices of tasks that are due (avoids borrow conflict).
        let due_indices: Vec<usize> = self.tasks.iter().enumerate()
            .filter(|(_, task)| {
                task.enabled && task.next_run.map_or(false, |next| now >= next)
            })
            .map(|(i, _)| i)
            .collect();

        if due_indices.is_empty() { return; }

        // Phase 2: Execute each due task. We clone the task data needed for
        // execution to avoid holding &mut self during the async calls.
        for i in due_indices {
            let task_snapshot = self.tasks[i].clone();
            tracing::info!("Running scheduled task: {}", task_snapshot.name);

            let result = self.execute_task(&task_snapshot).await;
            self.deliver_result(&task_snapshot, &result).await;

            // Phase 3: Update the task in place after execution completes.
            self.tasks[i].last_run = Some(now);
            self.tasks[i].last_result = Some(result);
            self.tasks[i].next_run = compute_next_run(&self.tasks[i].cron_expression);
        }

        self.save().await.ok();
    }
}

fn compute_next_run(cron_expr: &str) -> Option<DateTime<Utc>> {
    let schedule: cron::Schedule = cron_expr.parse().ok()?;
    schedule.upcoming(Utc).next()
}
```

---

## Phase 7.3 — Task Execution

### `crates/scheduler/src/execution.rs`

```rust
impl Scheduler {
    async fn execute_task(&self, task: &ScheduledTask) -> TaskRunResult {
        let start = Instant::now();

        let result = match &task.action {
            ScheduledAction::ShellCommand { command } => {
                self.run_shell_command(command).await
            }
            ScheduledAction::ClaudePrompt { prompt, model } => {
                self.run_claude_prompt(prompt, model.as_deref()).await
            }
            ScheduledAction::WebCheck { url, prompt } => {
                self.run_web_check(url, prompt).await
            }
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

## Phase 7.4 — Discord Commands

### `crates/discord/src/commands/schedule.rs`

```rust
/// Create a recurring scheduled task.
#[poise::command(slash_command)]
pub async fn schedule(
    ctx: Context<'_>,
    #[description = "Task name"] name: String,
    #[description = "Cron expression (e.g., '0 30 7 * * MON-FRI')"] cron: String,
    #[description = "Action: command, prompt, or webcheck"] action_type: String,
    #[description = "Value (command string, prompt text, or URL)"] value: String,
    #[description = "Extra (prompt for webcheck)"] extra: Option<String>,
) -> Result<(), ThresholdError> {
    // Parse action type and build ScheduledAction
    // Default delivery: current Discord channel
    // Validate cron expression
    // Add to scheduler
    // Respond with confirmation
}

/// List all scheduled tasks.
#[poise::command(slash_command)]
pub async fn schedules(ctx: Context<'_>) -> Result<(), ThresholdError> {
    // List tasks with name, cron, next run, last result
}

/// Remove a scheduled task.
#[poise::command(slash_command)]
pub async fn unschedule(
    ctx: Context<'_>,
    #[description = "Task name or ID"] name_or_id: String,
) -> Result<(), ThresholdError> {
    // Find by name or UUID, remove, confirm
}

/// Enable or disable a scheduled task.
#[poise::command(slash_command)]
pub async fn schedule_toggle(
    ctx: Context<'_>,
    #[description = "Task name or ID"] name_or_id: String,
    #[description = "Enable or disable"] enabled: bool,
) -> Result<(), ThresholdError> { /* ... */ }
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
  lib.rs            — re-exports Scheduler
  task.rs           — ScheduledTask, ScheduledAction, DeliveryTarget types
  engine.rs         — Scheduler main loop
  execution.rs      — task execution logic
  store.rs          — persistence (load/save schedules.json)
```

---

## Verification Checklist

- [ ] Unit test: cron expression parsing and next-run computation
- [ ] Unit test: task CRUD operations (add, remove, toggle)
- [ ] Unit test: ShellCommand action executes correctly
- [ ] Unit test: ClaudePrompt action sends to Claude and returns response
- [ ] Unit test: WebCheck action fetches URL then analyzes with Claude
- [ ] Unit test: disabled tasks are skipped
- [ ] Integration test: schedule a task, advance time, verify it fires
- [ ] Integration test: result delivery to Discord channel
- [ ] Integration test: result delivery as DM
- [ ] Integration test: task state persists across restarts
- [ ] Integration test: scheduler respects cancellation token
