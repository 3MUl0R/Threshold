//! Security middleware for authorization checks.

use threshold_core::config::DiscordConfig;

/// Check if a user is authorized to interact with the bot.
pub fn is_authorized(config: &DiscordConfig, guild_id: Option<u64>, user_id: u64) -> bool {
    // User must always be in the allowlist
    if !config.allowed_user_ids.contains(&user_id) {
        return false;
    }

    match guild_id {
        // Guild messages: must be the correct guild
        Some(gid) => gid == config.guild_id,
        // DMs: allowed if the user is in the allowlist (checked above)
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DiscordConfig {
        DiscordConfig {
            guild_id: 123456789,
            allowed_user_ids: vec![111, 222, 333],
        }
    }

    #[test]
    fn authorized_user_in_correct_guild() {
        let config = test_config();
        assert!(is_authorized(&config, Some(123456789), 111));
    }

    #[test]
    fn unauthorized_user_not_in_allowlist() {
        let config = test_config();
        assert!(!is_authorized(&config, Some(123456789), 999));
    }

    #[test]
    fn unauthorized_user_in_wrong_guild() {
        let config = test_config();
        assert!(!is_authorized(&config, Some(999999), 111));
    }

    #[test]
    fn authorized_dm_from_allowlisted_user() {
        let config = test_config();
        assert!(is_authorized(&config, None, 111));
    }

    #[test]
    fn unauthorized_dm_from_non_allowlisted_user() {
        let config = test_config();
        assert!(!is_authorized(&config, None, 999));
    }
}
