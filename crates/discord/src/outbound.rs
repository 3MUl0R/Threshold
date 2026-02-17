//! Agent-initiated Discord actions.

use std::sync::Arc;
use threshold_core::Result;

/// Discord outbound for agent-initiated actions
pub struct DiscordOutbound {
    http: Arc<serenity::all::Http>,
}

impl DiscordOutbound {
    pub fn new(http: Arc<serenity::all::Http>) -> Self {
        Self { http }
    }

    /// Send a text message to a channel.
    pub async fn send_to_channel(&self, channel_id: u64, content: &str) -> Result<()> {
        let channel = serenity::all::ChannelId::new(channel_id);
        channel
            .say(&self.http, content)
            .await
            .map_err(|e| threshold_core::ThresholdError::Discord(e.to_string()))?;

        tracing::debug!(channel_id = channel_id, "Sent message to channel");
        Ok(())
    }

    /// Send a DM to a user.
    pub async fn send_dm(&self, user_id: u64, content: &str) -> Result<()> {
        let user = serenity::all::UserId::new(user_id);

        // Create DM channel
        let dm_channel = user
            .create_dm_channel(&self.http)
            .await
            .map_err(|e| threshold_core::ThresholdError::Discord(e.to_string()))?;

        // Send message
        dm_channel
            .say(&self.http, content)
            .await
            .map_err(|e| threshold_core::ThresholdError::Discord(e.to_string()))?;

        tracing::debug!(user_id = user_id, "Sent DM to user");
        Ok(())
    }

    /// Create a new text channel in the guild.
    pub async fn create_channel(
        &self,
        guild_id: u64,
        name: &str,
        topic: &str,
    ) -> Result<u64> {
        let guild = serenity::all::GuildId::new(guild_id);

        let channel = guild
            .create_channel(&self.http, serenity::all::CreateChannel::new(name).topic(topic))
            .await
            .map_err(|e| threshold_core::ThresholdError::Discord(e.to_string()))?;

        tracing::info!(
            guild_id = guild_id,
            channel_id = channel.id.get(),
            name = name,
            "Created channel"
        );

        Ok(channel.id.get())
    }

    /// Send a message with file attachments.
    pub async fn send_with_attachments(
        &self,
        channel_id: u64,
        content: &str,
        attachments: Vec<(String, Vec<u8>)>,
    ) -> Result<()> {
        let channel = serenity::all::ChannelId::new(channel_id);

        // Create attachment objects
        let files: Vec<serenity::all::CreateAttachment> = attachments
            .into_iter()
            .map(|(filename, data)| serenity::all::CreateAttachment::bytes(data, filename))
            .collect();

        let attachment_count = files.len();

        // Send message with attachments
        channel
            .send_message(
                &self.http,
                serenity::all::CreateMessage::new()
                    .content(content)
                    .files(files),
            )
            .await
            .map_err(|e| threshold_core::ThresholdError::Discord(e.to_string()))?;

        tracing::debug!(
            channel_id = channel_id,
            attachment_count = attachment_count,
            "Sent message with attachments"
        );

        Ok(())
    }
}

/// Implement `ResultSender` so the scheduler can deliver results via Discord.
///
/// This bridges the dependency inversion: the scheduler depends on the
/// `ResultSender` trait (in core), and Discord provides the concrete impl.
#[async_trait::async_trait]
impl threshold_core::ResultSender for DiscordOutbound {
    async fn send_to_channel(
        &self,
        channel_id: u64,
        message: &str,
    ) -> threshold_core::Result<()> {
        self.send_to_channel(channel_id, message).await
    }

    async fn send_dm(&self, user_id: u64, message: &str) -> threshold_core::Result<()> {
        self.send_dm(user_id, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_construction() {
        // Just verify we can construct the type
        // Actual Discord API calls require real bot token
        let http = Arc::new(serenity::all::Http::new("fake_token"));
        let outbound = DiscordOutbound::new(http);
        assert!(std::mem::size_of_val(&outbound) > 0);
    }
}
