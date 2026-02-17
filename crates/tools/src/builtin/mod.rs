//! Built-in tools for Threshold
//!
//! Only ExecTool remains — read, write, and edit tools have been removed
//! because Claude has native capabilities for file operations.
//! ExecTool is retained for the scheduler's direct Script action execution.

mod exec;

pub use exec::ExecTool;
