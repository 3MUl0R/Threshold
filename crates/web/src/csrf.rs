//! CSRF protection via cookie + hidden form field.
//!
//! On each request that renders a form, a random token is generated,
//! set as a cookie, and injected into the template context.
//! On POST, the form field value is compared against the cookie.

use rand::Rng;

/// Generate a random 32-byte hex-encoded CSRF token (64 chars).
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    hex::encode(bytes)
}

/// Validate that the form token matches the cookie token.
pub fn validate(cookie_value: &str, form_value: &str) -> bool {
    !cookie_value.is_empty() && cookie_value == form_value
}

/// Cookie name for CSRF tokens.
pub const COOKIE_NAME: &str = "__csrf";

/// Form field name for CSRF tokens.
pub const FIELD_NAME: &str = "_csrf";

/// Extract a cookie value by name from a Cookie header string.
pub fn extract_cookie(cookie_header: &str, name: &str) -> Option<String> {
    cookie_header.split(';').find_map(|pair| {
        let pair = pair.trim();
        let (key, value) = pair.split_once('=')?;
        if key.trim() == name {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex_chars() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tokens_are_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }

    #[test]
    fn validate_matching_tokens() {
        let token = generate_token();
        assert!(validate(&token, &token));
    }

    #[test]
    fn validate_mismatched_tokens() {
        assert!(!validate("abc", "def"));
    }

    #[test]
    fn validate_empty_token_rejected() {
        assert!(!validate("", ""));
    }

    #[test]
    fn extract_cookie_from_header() {
        let header = "__csrf=abc123; other=xyz";
        assert_eq!(extract_cookie(header, "__csrf"), Some("abc123".into()));
        assert_eq!(extract_cookie(header, "other"), Some("xyz".into()));
        assert_eq!(extract_cookie(header, "missing"), None);
    }
}
