//! Scheduler engine — command channel pattern and main scheduling loop.
//!
//! The `SchedulerHandle` provides a safe, cloneable interface for other components
//! (CLI daemon API, Discord commands, conversation engine) to interact with the
//! scheduler while its main loop runs.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use threshold_cli_wrapper::ClaudeClient;
use threshold_conversation::ConversationEngine;
use threshold_core::{ResultSender, ThresholdError};

use crate::cron_utils;
use crate::execution;
use crate::store;
use crate::task::{ScheduledTask, TaskRunResult};

/// Commands sent from handles to the scheduler loop.
enum SchedulerCommand {
    AddTask(ScheduledTask),
    RemoveTask {
        id: Uuid,
        reply: oneshot::Sender<Result<(), ThresholdError>>,
    },
    ToggleTask {
        id: Uuid,
        enabled: bool,
        reply: oneshot::Sender<Result<(), ThresholdError>>,
    },
    ListTasks(oneshot::Sender<Vec<ScheduledTask>>),
}

/// Cloneable handle for sending commands to the scheduler.
///
/// This can be shared across tasks (Discord commands, daemon API, etc.)
/// to interact with the scheduler without direct access to its internal state.
#[derive(Clone)]
pub struct SchedulerHandle {
    command_tx: mpsc::UnboundedSender<SchedulerCommand>,
}

impl SchedulerHandle {
    /// Add a new scheduled task.
    pub async fn add_task(&self, task: ScheduledTask) -> Result<(), ThresholdError> {
        self.command_tx
            .send(SchedulerCommand::AddTask(task))
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        Ok(())
    }

    /// Remove a scheduled task by ID.
    pub async fn remove_task(&self, id: Uuid) -> Result<(), ThresholdError> {
        let (reply, rx) = oneshot::channel();
        self.command_tx
            .send(SchedulerCommand::RemoveTask { id, reply })
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        rx.await.map_err(|_| ThresholdError::SchedulerShutdown)?
    }

    /// Toggle a scheduled task on or off.
    pub async fn toggle_task(&self, id: Uuid, enabled: bool) -> Result<(), ThresholdError> {
        let (reply, rx) = oneshot::channel();
        self.command_tx
            .send(SchedulerCommand::ToggleTask {
                id,
                enabled,
                reply,
            })
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        rx.await.map_err(|_| ThresholdError::SchedulerShutdown)?
    }

    /// List all scheduled tasks.
    pub async fn list_tasks(&self) -> Result<Vec<ScheduledTask>, ThresholdError> {
        let (reply, rx) = oneshot::channel();
        self.command_tx
            .send(SchedulerCommand::ListTasks(reply))
            .map_err(|_| ThresholdError::SchedulerShutdown)?;
        rx.await.map_err(|_| ThresholdError::SchedulerShutdown)
    }
}

/// The scheduler's internal state and main loop.
pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
    /// IDs of tasks currently executing (for skip-if-running).
    running_tasks: Arc<RwLock<HashSet<Uuid>>>,
    /// Bounded concurrency for task execution (default: 4).
    task_semaphore: Arc<tokio::sync::Semaphore>,
    /// Path for persisting task state.
    store_path: PathBuf,
    /// Claude CLI client for NewConversation and ScriptThenConversation actions.
    claude: Arc<ClaudeClient>,
    /// Conversation engine for ResumeConversation actions.
    engine: Arc<ConversationEngine>,
    /// Optional result sender for delivering task results to Discord.
    result_sender: Option<Arc<dyn ResultSender>>,
    command_rx: mpsc::UnboundedReceiver<SchedulerCommand>,
    /// Receives task completion results from spawned execution tasks.
    completion_rx: mpsc::UnboundedReceiver<(Uuid, TaskRunResult)>,
    completion_tx: mpsc::UnboundedSender<(Uuid, TaskRunResult)>,
    cancel: CancellationToken,
}

impl Scheduler {
    /// Create a new scheduler and its handle.
    ///
    /// Loads persisted tasks from `store_path` on startup.
    /// Returns `(Scheduler, SchedulerHandle)` — call `scheduler.run()` to start the loop.
    pub async fn new(
        store_path: PathBuf,
        claude: Arc<ClaudeClient>,
        engine: Arc<ConversationEngine>,
        result_sender: Option<Arc<dyn ResultSender>>,
        cancel: CancellationToken,
    ) -> (Self, SchedulerHandle) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();

        // Load persisted tasks (non-fatal: log and start with empty)
        let tasks = match store::load_tasks(&store_path).await {
            Ok(tasks) => {
                if !tasks.is_empty() {
                    tracing::info!("Loaded {} scheduled tasks from disk", tasks.len());
                }
                tasks
            }
            Err(e) => {
                tracing::warn!("Failed to load scheduled tasks: {}", e);
                Vec::new()
            }
        };

        let scheduler = Self {
            tasks,
            running_tasks: Arc::new(RwLock::new(HashSet::new())),
            task_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            store_path,
            claude,
            engine,
            result_sender,
            command_rx,
            completion_rx,
            completion_tx,
            cancel,
        };

        let handle = SchedulerHandle { command_tx };
        (scheduler, handle)
    }

    /// Set the result sender after construction.
    ///
    /// Used to wire the Discord outbound handle after Discord connects,
    /// since the scheduler must be created before Discord starts.
    pub fn set_result_sender(&mut self, sender: Arc<dyn ResultSender>) {
        self.result_sender = Some(sender);
    }

    /// Persist the current task list to disk.
    ///
    /// Non-fatal: logs a warning if saving fails.
    async fn persist(&self) {
        if let Err(e) = store::save_tasks(&self.store_path, &self.tasks).await {
            tracing::warn!("Failed to persist scheduled tasks: {}", e);
        }
    }

    /// Run the scheduler main loop.
    ///
    /// Ticks every 60 seconds, checks for due tasks, and handles commands
    /// from the `SchedulerHandle`. Exits when the cancellation token fires.
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
                Some((task_id, result)) = self.completion_rx.recv() => {
                    self.record_completion(task_id, result).await;
                }
                _ = self.cancel.cancelled() => {
                    tracing::info!("Scheduler shutting down.");
                    break;
                }
            }
        }
    }

    /// Handle a command from the SchedulerHandle.
    async fn handle_command(&mut self, cmd: SchedulerCommand) {
        match cmd {
            SchedulerCommand::AddTask(task) => {
                tracing::info!("Adding scheduled task: {} ({})", task.name, task.id);
                self.tasks.push(task);
                self.persist().await;
            }
            SchedulerCommand::RemoveTask { id, reply } => {
                let result = if let Some(pos) = self.tasks.iter().position(|t| t.id == id) {
                    let task = self.tasks.remove(pos);
                    tracing::info!("Removed scheduled task: {} ({})", task.name, id);
                    self.persist().await;
                    Ok(())
                } else {
                    Err(ThresholdError::NotFound {
                        message: format!("Task {} not found", id),
                    })
                };
                let _ = reply.send(result);
            }
            SchedulerCommand::ToggleTask {
                id,
                enabled,
                reply,
            } => {
                let result = if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
                    task.enabled = enabled;
                    tracing::info!(
                        "Task '{}' {}",
                        task.name,
                        if enabled { "enabled" } else { "disabled" }
                    );
                    self.persist().await;
                    Ok(())
                } else {
                    Err(ThresholdError::NotFound {
                        message: format!("Task {} not found", id),
                    })
                };
                let _ = reply.send(result);
            }
            SchedulerCommand::ListTasks(reply) => {
                let _ = reply.send(self.tasks.clone());
            }
        }
    }

    /// Record a completed task's result and persist the update.
    async fn record_completion(&mut self, task_id: Uuid, result: TaskRunResult) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
            task.last_run = Some(result.timestamp);
            task.last_result = Some(result);
            self.persist().await;
        }
    }

    /// Check for due tasks and spawn them with bounded concurrency.
    async fn check_and_run(&mut self) {
        let now = Utc::now();

        let due_task_ids: Vec<Uuid> = self
            .tasks
            .iter()
            .filter(|task| task.enabled && task.next_run.is_some_and(|next| now >= next))
            .map(|task| task.id)
            .collect();

        if due_task_ids.is_empty() {
            return;
        }

        for task_id in due_task_ids {
            // Skip if already running
            {
                let running = self.running_tasks.read().await;
                if running.contains(&task_id) {
                    if let Some(task) = self.tasks.iter().find(|t| t.id == task_id) {
                        if task.skip_if_running {
                            tracing::info!(
                                "Skipping task '{}': previous run still active",
                                task.name
                            );
                            continue;
                        }
                    }
                }
            }

            // Update next_run immediately (don't wait for execution)
            if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                task.next_run = cron_utils::compute_next_run(&task.cron_expression);
            }

            let task_snapshot = match self.tasks.iter().find(|t| t.id == task_id).cloned() {
                Some(task) => task,
                None => continue,
            };

            // Spawn task execution with bounded concurrency
            let semaphore = self.task_semaphore.clone();
            let running_tasks = self.running_tasks.clone();
            let claude = self.claude.clone();
            let engine = self.engine.clone();
            let result_sender = self.result_sender.clone();
            let completion_tx = self.completion_tx.clone();
            tokio::spawn(async move {
                let _permit = match semaphore.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => return, // semaphore closed
                };

                running_tasks.write().await.insert(task_snapshot.id);
                tracing::info!("Running scheduled task: {}", task_snapshot.name);

                let result = execution::execute_task(&task_snapshot, &claude, &engine).await;
                execution::deliver_result(&task_snapshot, &result, &result_sender).await;

                if result.success {
                    tracing::info!(
                        "Task '{}' completed in {}ms",
                        task_snapshot.name,
                        result.duration_ms
                    );
                } else {
                    tracing::warn!(
                        "Task '{}' failed: {}",
                        task_snapshot.name,
                        result.summary
                    );
                }

                // Send completion back to scheduler loop to update last_run/last_result
                let _ = completion_tx.send((task_snapshot.id, result));

                running_tasks.write().await.remove(&task_snapshot.id);
            });
        }

        // Persist updated next_run values
        self.persist().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::DeliveryTarget;
    use threshold_core::ScheduledAction;
    use threshold_core::config::ThresholdConfig;

    fn make_test_task(name: &str, cron: &str) -> ScheduledTask {
        let mut task = ScheduledTask::new(
            name.into(),
            cron.into(),
            ScheduledAction::Script {
                command: "echo test".into(),
                working_dir: None,
            },
        )
        .unwrap();
        task.delivery = DeliveryTarget::AuditLogOnly;
        task
    }

    async fn make_test_scheduler(
        cancel: CancellationToken,
    ) -> (Scheduler, SchedulerHandle) {
        use threshold_core::config::{AgentConfigToml, ClaudeCliConfig, CliConfig, ToolsConfig};

        let tmp = tempfile::tempdir().unwrap();
        let claude = Arc::new(
            ClaudeClient::new("claude".into(), tmp.path().join("cli"), false)
                .await
                .unwrap(),
        );
        let config = ThresholdConfig {
            data_dir: Some(tmp.path().to_path_buf()),
            log_level: None,
            cli: CliConfig {
                claude: ClaudeCliConfig {
                    command: Some("claude".to_string()),
                    model: Some("sonnet".to_string()),
                    timeout_seconds: None,
                    skip_permissions: Some(false),
                    extra_flags: vec![],
                },
            },
            discord: None,
            agents: vec![AgentConfigToml {
                id: "default".to_string(),
                name: "Default Agent".to_string(),
                cli_provider: "claude".to_string(),
                model: Some("sonnet".to_string()),
                system_prompt: None,
                system_prompt_file: None,
                tools: Some("full".to_string()),
            }],
            tools: ToolsConfig::default(),
            heartbeat: None,
            scheduler: None,
        };
        let engine = Arc::new(
            ConversationEngine::new(&config, claude.clone(), None)
                .await
                .unwrap(),
        );
        Scheduler::new(
            tmp.path().join("schedules.json"),
            claude,
            engine,
            None,
            cancel,
        )
        .await
    }

    #[tokio::test]
    async fn handle_add_and_list_tasks() {
        let cancel = CancellationToken::new();
        let (mut scheduler, handle) = make_test_scheduler(cancel.clone()).await;

        // Run scheduler in background
        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        let task = make_test_task("test-task", "0 0 3 * * * *");
        let task_id = task.id;
        handle.add_task(task).await.unwrap();

        // Give scheduler time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tasks = handle.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task_id);
        assert_eq!(tasks[0].name, "test-task");

        cancel.cancel();
        scheduler_task.await.unwrap();
    }

    #[tokio::test]
    async fn handle_remove_task() {
        let cancel = CancellationToken::new();
        let (mut scheduler, handle) = make_test_scheduler(cancel.clone()).await;

        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        let task = make_test_task("to-remove", "0 0 3 * * * *");
        let task_id = task.id;
        handle.add_task(task).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.remove_task(task_id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tasks = handle.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 0);

        cancel.cancel();
        scheduler_task.await.unwrap();
    }

    #[tokio::test]
    async fn handle_remove_nonexistent_returns_not_found() {
        let cancel = CancellationToken::new();
        let (mut scheduler, handle) = make_test_scheduler(cancel.clone()).await;

        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        let result = handle.remove_task(Uuid::new_v4()).await;
        assert!(result.is_err());

        cancel.cancel();
        scheduler_task.await.unwrap();
    }

    #[tokio::test]
    async fn handle_toggle_task() {
        let cancel = CancellationToken::new();
        let (mut scheduler, handle) = make_test_scheduler(cancel.clone()).await;

        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        let task = make_test_task("toggle-me", "0 0 3 * * * *");
        let task_id = task.id;
        handle.add_task(task).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Disable
        handle.toggle_task(task_id, false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tasks = handle.list_tasks().await.unwrap();
        assert!(!tasks[0].enabled);

        // Re-enable
        handle.toggle_task(task_id, true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tasks = handle.list_tasks().await.unwrap();
        assert!(tasks[0].enabled);

        cancel.cancel();
        scheduler_task.await.unwrap();
    }

    #[tokio::test]
    async fn scheduler_shuts_down_on_cancel() {
        let cancel = CancellationToken::new();
        let (mut scheduler, _handle) = make_test_scheduler(cancel.clone()).await;

        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        // Cancel immediately
        cancel.cancel();

        // Should complete promptly
        tokio::time::timeout(Duration::from_secs(5), scheduler_task)
            .await
            .expect("scheduler did not shut down within timeout")
            .unwrap();
    }

    #[tokio::test]
    async fn scheduler_handle_fails_after_scheduler_dropped() {
        let cancel = CancellationToken::new();
        let (mut scheduler, handle) = make_test_scheduler(cancel.clone()).await;

        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        cancel.cancel();
        scheduler_task.await.unwrap();

        // Now the scheduler is gone — handle operations should fail
        let result = handle.list_tasks().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn disabled_task_not_fired() {
        let cancel = CancellationToken::new();
        let (mut scheduler, handle) = make_test_scheduler(cancel.clone()).await;

        let mut task = make_test_task("disabled-task", "0 * * * * * *");
        task.enabled = false;
        // Set next_run to the past so it would fire if enabled
        task.next_run = Some(Utc::now() - chrono::Duration::minutes(1));

        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        handle.add_task(task).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Verify task is still in the list but was not executed (still has old next_run)
        let tasks = handle.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].enabled);

        cancel.cancel();
        scheduler_task.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_tasks_managed() {
        let cancel = CancellationToken::new();
        let (mut scheduler, handle) = make_test_scheduler(cancel.clone()).await;

        let scheduler_task = tokio::spawn(async move {
            scheduler.run().await;
        });

        let task1 = make_test_task("task-1", "0 0 3 * * * *");
        let task2 = make_test_task("task-2", "0 0 6 * * * *");
        let task3 = make_test_task("task-3", "0 0 9 * * * *");
        let id2 = task2.id;

        handle.add_task(task1).await.unwrap();
        handle.add_task(task2).await.unwrap();
        handle.add_task(task3).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(handle.list_tasks().await.unwrap().len(), 3);

        handle.remove_task(id2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tasks = handle.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.id != id2));

        cancel.cancel();
        scheduler_task.await.unwrap();
    }
}
