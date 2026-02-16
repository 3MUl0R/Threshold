//! Message chunking for Discord's 2000-character limit.

/// Split a message respecting Discord's character limit.
///
/// Split priorities:
/// 1. Paragraph boundary (double newline)
/// 2. Single newline
/// 3. Sentence boundary (. ! ?)
/// 4. Word boundary (space)
/// 5. Hard cut (last resort)
///
/// Never splits inside a markdown code block (``` ... ```).
pub fn chunk_message(content: &str, max_len: usize) -> Vec<String> {
    // TODO: Phase 4.4 implementation
    vec![content.to_string()]
}
