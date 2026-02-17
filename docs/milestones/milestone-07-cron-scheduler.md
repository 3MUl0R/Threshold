# Milestone 7 — Cron Scheduler (MERGED)

> **This milestone has been merged into [Milestone 6 — Unified Scheduler](milestone-06-unified-scheduler.md).**
>
> The key architectural insight: heartbeats and cron jobs are the same system
> under the hood. All scheduling functionality — cron parsing, task execution,
> the command channel pattern, persistence, Discord commands, and the daemon
> API — lives in the unified scheduler milestone.
>
> The `ScheduledAction` enum (source of truth) is defined in Milestone 5 and
> lives in `crates/core/src/types.rs`.
>
> See `milestone-06-unified-scheduler.md` for the unified design.
