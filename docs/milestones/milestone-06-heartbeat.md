# Milestone 6 — Heartbeat System (MERGED)

> **This milestone has been merged into [Milestone 6 — Unified Scheduler](milestone-06-unified-scheduler.md).**
>
> The key architectural insight: heartbeats and cron jobs are the same system
> under the hood. A heartbeat is simply a pre-configured scheduled task with
> `ScheduledAction::ResumeConversation`, a skip-if-running guard, and handoff
> notes for continuity.
>
> See `milestone-06-unified-scheduler.md` for the unified design.
