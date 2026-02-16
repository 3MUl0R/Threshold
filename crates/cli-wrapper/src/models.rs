//! Model alias resolution.
//!
//! Resolves user-friendly model names to CLI model identifiers.

use std::borrow::Cow;

/// Resolve user-friendly model aliases to CLI model names.
///
/// Returns a `Cow<'static, str>` to avoid allocating for known aliases
/// (which are static strings) while still supporting pass-through of
/// unknown models.
///
/// # Examples
///
/// ```
/// use threshold_cli_wrapper::models::resolve_model_alias;
///
/// assert_eq!(resolve_model_alias("opus"), "opus");
/// assert_eq!(resolve_model_alias("OPUS-4.6"), "opus");
/// assert_eq!(resolve_model_alias("sonnet"), "sonnet");
/// assert_eq!(resolve_model_alias("custom-model"), "custom-model");
/// ```
pub fn resolve_model_alias(input: &str) -> Cow<'static, str> {
    let lower = input.to_lowercase();
    match lower.as_str() {
        "opus" | "opus-4" | "opus-4.5" | "opus-4.6" | "claude-opus" => Cow::Borrowed("opus"),
        "sonnet" | "sonnet-4" | "sonnet-4.1" | "sonnet-4.5" | "claude-sonnet" => {
            Cow::Borrowed("sonnet")
        }
        "haiku" | "haiku-3.5" | "haiku-4.5" | "claude-haiku" => Cow::Borrowed("haiku"),
        _ => Cow::Owned(input.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_opus_aliases() {
        assert_eq!(resolve_model_alias("opus"), "opus");
        assert_eq!(resolve_model_alias("OPUS"), "opus");
        assert_eq!(resolve_model_alias("opus-4"), "opus");
        assert_eq!(resolve_model_alias("opus-4.5"), "opus");
        assert_eq!(resolve_model_alias("opus-4.6"), "opus");
        assert_eq!(resolve_model_alias("claude-opus"), "opus");
    }

    #[test]
    fn resolve_sonnet_aliases() {
        assert_eq!(resolve_model_alias("sonnet"), "sonnet");
        assert_eq!(resolve_model_alias("SONNET"), "sonnet");
        assert_eq!(resolve_model_alias("Sonnet-4.5"), "sonnet");
        assert_eq!(resolve_model_alias("claude-sonnet"), "sonnet");
    }

    #[test]
    fn resolve_haiku_aliases() {
        assert_eq!(resolve_model_alias("haiku"), "haiku");
        assert_eq!(resolve_model_alias("HAIKU"), "haiku");
        assert_eq!(resolve_model_alias("haiku-3.5"), "haiku");
        assert_eq!(resolve_model_alias("haiku-4.5"), "haiku");
        assert_eq!(resolve_model_alias("claude-haiku"), "haiku");
    }

    #[test]
    fn unknown_model_passes_through() {
        assert_eq!(resolve_model_alias("gpt-4"), "gpt-4");
        assert_eq!(resolve_model_alias("custom-model"), "custom-model");
        assert_eq!(resolve_model_alias("llama-2"), "llama-2");
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(resolve_model_alias("OpUs"), "opus");
        assert_eq!(resolve_model_alias("SoNnEt-4.5"), "sonnet");
        assert_eq!(resolve_model_alias("HAIKU"), "haiku");
    }
}
