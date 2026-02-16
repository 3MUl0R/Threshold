//! Discord message event handler.

use crate::bot::BotData;
use threshold_core::ThresholdError;

/// Event handler for Discord events
pub async fn event_handler(
    _ctx: &serenity::all::Context,
    _event: &serenity::all::FullEvent,
    _framework: poise::FrameworkContext<'_, BotData, ThresholdError>,
    _data: &BotData,
) -> Result<(), ThresholdError> {
    // TODO: Phase 4.3 implementation
    Ok(())
}
