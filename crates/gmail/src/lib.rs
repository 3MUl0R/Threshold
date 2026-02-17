//! Gmail integration for Threshold.
//!
//! Provides `threshold gmail` CLI subcommands for reading and sending email
//! via the Google Gmail API. Supports multiple inboxes with per-inbox OAuth
//! token management.
//!
//! # Architecture
//!
//! This crate is a CLI-first design: Claude invokes `threshold gmail <command>`
//! via shell execution, gets JSON on stdout, and parses it naturally.
//!
//! # Security
//!
//! - OAuth 2.0 with per-inbox tokens stored in OS keychain
//! - Inbox allowlist enforcement via config
//! - Send/reply gated by `allow_send` config flag

pub mod auth;
pub mod cli;
pub mod client;
pub mod types;

pub use cli::{GmailArgs, handle_gmail_command};
