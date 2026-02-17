//! Unified Scheduler — cron jobs and heartbeat for Threshold.
//!
//! Provides a scheduling engine that handles both user-defined cron jobs and
//! the heartbeat (autonomous agent wake-up) pattern. One engine runs them all.

pub mod cron_utils;
pub mod daemon_api;
pub mod engine;
pub mod execution;
pub mod heartbeat;
pub mod store;
pub mod task;
pub mod work_items;

pub use engine::{Scheduler, SchedulerHandle};
