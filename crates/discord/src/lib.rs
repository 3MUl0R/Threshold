//! Discord bot implementation for Threshold.
//!
//! Provides Discord integration via poise/serenity framework.

mod bot;
mod chunking;
mod commands;
mod handler;
mod outbound;
mod portals;
mod scheduler_commands;
mod security;

pub use bot::{BotData, build_and_start};
pub use outbound::DiscordOutbound;
