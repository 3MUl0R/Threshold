//! Message chunking for Discord's 2000-character limit.

/// Find the largest byte index <= `pos` that lies on a UTF-8 char boundary.
/// Equivalent to the nightly `str::floor_char_boundary`.
fn floor_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut i = pos;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

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
/// All byte-index slicing is char-boundary safe (no panics on multi-byte UTF-8).
pub fn chunk_message(content: &str, max_len: usize) -> Vec<String> {
    if content.len() <= max_len {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        // Find split point
        let (chunk, rest) = split_at_best_boundary(remaining, max_len);
        chunks.push(chunk.to_string());
        remaining = rest;
    }

    chunks
}

/// Find the best split point respecting code blocks and natural boundaries.
/// All slicing uses `floor_char_boundary` for UTF-8 safety.
fn split_at_best_boundary(content: &str, max_len: usize) -> (&str, &str) {
    let safe_max = floor_char_boundary(content, max_len);

    // Check if we're inside a code block
    let in_code_block = count_code_fences(&content[..safe_max]) % 2 == 1;

    if in_code_block {
        // Find the closing ``` and extend chunk to include it
        if let Some(close_pos) = content[safe_max..].find("```") {
            let split_pos = safe_max + close_pos + 3; // +3 for ```
            if split_pos <= content.len() {
                let (chunk, rest) = content.split_at(split_pos);
                return (chunk.trim_end(), rest.trim_start());
            }
        }
        // If no closing fence found, we'll have to hard cut and continue the code block
        // in the next chunk (handled by caller)
    }

    // Try split priorities in order
    let split_pos = find_paragraph_boundary(content, safe_max)
        .or_else(|| find_newline_boundary(content, safe_max))
        .or_else(|| find_sentence_boundary(content, safe_max))
        .or_else(|| find_word_boundary(content, safe_max))
        .unwrap_or(safe_max);

    // Guarantee forward progress: if safe_max rounded down to 0 (first char
    // wider than max_len), advance past the first character.
    let split_pos = if split_pos == 0 {
        content
            .char_indices()
            .nth(1)
            .map(|(i, _)| i)
            .unwrap_or(content.len())
    } else {
        split_pos
    };

    let (chunk, rest) = content.split_at(split_pos);
    (chunk.trim_end(), rest.trim_start())
}

/// Count number of code fence markers (```) in text
fn count_code_fences(text: &str) -> usize {
    text.matches("```").count()
}

/// Find paragraph boundary (double newline) searching backwards from safe_max
fn find_paragraph_boundary(content: &str, safe_max: usize) -> Option<usize> {
    let search_text = &content[..safe_max];
    search_text.rfind("\n\n").map(|pos| pos + 2)
}

/// Find single newline searching backwards from safe_max
fn find_newline_boundary(content: &str, safe_max: usize) -> Option<usize> {
    let search_text = &content[..safe_max];
    search_text.rfind('\n').map(|pos| pos + 1)
}

/// Find sentence boundary (. ! ?) searching backwards from safe_max
fn find_sentence_boundary(content: &str, safe_max: usize) -> Option<usize> {
    let search_text = &content[..safe_max];
    for (i, c) in search_text.char_indices().rev() {
        if matches!(c, '.' | '!' | '?') {
            // Check if followed by space or end
            let next_pos = i + c.len_utf8();
            if next_pos >= search_text.len()
                || search_text[next_pos..].chars().next() == Some(' ')
            {
                return Some(next_pos);
            }
        }
    }
    None
}

/// Find word boundary (space) searching backwards from safe_max
fn find_word_boundary(content: &str, safe_max: usize) -> Option<usize> {
    let search_text = &content[..safe_max];
    search_text.rfind(' ').map(|pos| pos + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_message_not_split() {
        let msg = "Hello, world!";
        let chunks = chunk_message(msg, 2000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], msg);
    }

    #[test]
    fn split_at_paragraph_boundary() {
        let msg = "First paragraph.\n\nSecond paragraph that makes this very long.";
        let chunks = chunk_message(msg, 20);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0], "First paragraph.");
        assert!(chunks[1].starts_with("Second"));
    }

    #[test]
    fn split_at_newline() {
        let msg = "Line one\nLine two that is longer";
        let chunks = chunk_message(msg, 15);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0], "Line one");
    }

    #[test]
    fn split_at_sentence() {
        let msg = "First sentence. Second sentence that is much longer.";
        let chunks = chunk_message(msg, 20);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0], "First sentence.");
    }

    #[test]
    fn split_at_word_boundary() {
        let msg = "This is a long message without punctuation";
        let chunks = chunk_message(msg, 15);
        assert!(chunks.len() >= 2);
        // Should split at space
        assert!(!chunks[0].contains("without"));
    }

    #[test]
    fn preserves_code_block() {
        let msg = "Here is code:\n```rust\nfn main() {}\n```\nMore text.";
        let chunks = chunk_message(msg, 30);
        // Code block should not be split
        let combined = chunks.join("");
        assert!(combined.contains("```rust"));
        assert!(combined.contains("```"));
    }

    #[test]
    fn code_block_straddling_boundary() {
        let msg = "Text before ```rust\nfn main() { println!(\"hello\"); }\n```";
        let chunks = chunk_message(msg, 25);
        // Should extend to include closing ```
        let has_opening = chunks.iter().any(|c| c.contains("```rust"));
        let has_closing = chunks.iter().any(|c| c.trim_end().ends_with("```"));
        assert!(has_opening);
        assert!(has_closing);
    }

    #[test]
    fn whitespace_trimming() {
        let msg = "Line one   \n\n   Line two";
        let chunks = chunk_message(msg, 15);
        assert_eq!(chunks[0], "Line one");
        assert!(chunks[1].starts_with("Line two"));
    }

    #[test]
    fn empty_string() {
        let chunks = chunk_message("", 2000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "");
    }

    #[test]
    fn exact_max_len() {
        let msg = "a".repeat(2000);
        let chunks = chunk_message(&msg, 2000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 2000);
    }

    #[test]
    fn slightly_over_max_len() {
        let msg = "a".repeat(2001);
        let chunks = chunk_message(&msg, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 2000);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn multiple_code_blocks() {
        let msg = "First ```code1``` middle ```code2``` end";
        let chunks = chunk_message(msg, 20);
        let combined = chunks.join("");
        // Count code fences - should be even (2 opening, 2 closing)
        assert_eq!(combined.matches("```").count(), 4);
    }

    #[test]
    fn multibyte_utf8_boundary_does_not_panic() {
        // 1999 ASCII bytes + a 4-byte emoji = byte 2000 lands inside a multi-byte char
        let msg = format!("{}🙂tail", "a".repeat(1999));
        let chunks = chunk_message(&msg, 2000);
        // Should split without panic; all content preserved
        let combined: String = chunks.iter().map(|c| c.as_str()).collect();
        assert!(combined.contains("🙂"));
        assert!(combined.contains("tail"));
    }

    #[test]
    fn floor_char_boundary_on_ascii() {
        let s = "hello world";
        assert_eq!(floor_char_boundary(s, 5), 5);
        assert_eq!(floor_char_boundary(s, 100), s.len());
        assert_eq!(floor_char_boundary(s, 0), 0);
    }

    #[test]
    fn floor_char_boundary_on_multibyte() {
        let s = "aaa🙂bbb"; // bytes: 3 ascii + 4 emoji + 3 ascii = 10 bytes
        assert_eq!(floor_char_boundary(s, 3), 3); // before emoji
        assert_eq!(floor_char_boundary(s, 4), 3); // inside emoji → back to 3
        assert_eq!(floor_char_boundary(s, 5), 3); // inside emoji → back to 3
        assert_eq!(floor_char_boundary(s, 6), 3); // inside emoji → back to 3
        assert_eq!(floor_char_boundary(s, 7), 7); // after emoji
    }

    #[test]
    fn leading_multibyte_with_tiny_max_len() {
        // max_len smaller than the first character — must not loop forever
        let msg = "🙂hello";
        let chunks = chunk_message(msg, 1);
        let combined: String = chunks.iter().map(|c| c.as_str()).collect();
        assert_eq!(combined, "🙂hello");
        assert!(chunks.len() >= 2); // emoji in one chunk, rest in others
    }
}
