//! Cookie-based flash messages for mutation feedback.
//!
//! On success/error, set a `__flash` cookie with JSON.
//! On the next GET request, read and clear it.

use serde::{Deserialize, Serialize};

/// Cookie name for flash messages.
pub const COOKIE_NAME: &str = "__flash";

/// A flash message to display once after a redirect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashMessage {
    pub level: String,
    pub message: String,
}

impl FlashMessage {
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            level: "success".into(),
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: "error".into(),
            message: message.into(),
        }
    }

    /// Encode as a cookie value (URL-safe JSON).
    pub fn to_cookie_value(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Decode from a cookie value.
    pub fn from_cookie_value(value: &str) -> Option<Self> {
        serde_json::from_str(value).ok()
    }
}

/// Extract a flash message from the Cookie header, if present.
pub fn read_flash(cookie_header: &str) -> Option<FlashMessage> {
    crate::csrf::extract_cookie(cookie_header, COOKIE_NAME)
        .and_then(|v| FlashMessage::from_cookie_value(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_round_trip() {
        let flash = FlashMessage::success("Task toggled");
        let cookie = flash.to_cookie_value();
        let restored = FlashMessage::from_cookie_value(&cookie).unwrap();
        assert_eq!(restored.level, "success");
        assert_eq!(restored.message, "Task toggled");
    }

    #[test]
    fn flash_error_level() {
        let flash = FlashMessage::error("Something broke");
        assert_eq!(flash.level, "error");
    }

    #[test]
    fn invalid_cookie_returns_none() {
        assert!(FlashMessage::from_cookie_value("not-json").is_none());
    }
}
