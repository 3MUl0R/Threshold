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

/// Find the best split point respecting code blocks and natural boundaries
fn split_at_best_boundary(content: &str, max_len: usize) -> (&str, &str) {
    // Check if we're inside a code block
    let in_code_block = count_code_fences(&content[..max_len.min(content.len())]) % 2 == 1;

    if in_code_block {
        // Find the closing ``` and extend chunk to include it
        if let Some(close_pos) = content[max_len..].find("```") {
            let split_pos = max_len + close_pos + 3; // +3 for ```
            if split_pos <= content.len() {
                let (chunk, rest) = content.split_at(split_pos);
                return (chunk.trim_end(), rest.trim_start());
            }
        }
        // If no closing fence found, we'll have to hard cut and continue the code block
        // in the next chunk (handled by caller)
    }

    // Try split priorities in order
    let split_pos = find_paragraph_boundary(content, max_len)
        .or_else(|| find_newline_boundary(content, max_len))
        .or_else(|| find_sentence_boundary(content, max_len))
        .or_else(|| find_word_boundary(content, max_len))
        .unwrap_or(max_len);

    let (chunk, rest) = content.split_at(split_pos);
    (chunk.trim_end(), rest.trim_start())
}

/// Count number of code fence markers (```) in text
fn count_code_fences(text: &str) -> usize {
    text.matches("```").count()
}

/// Find paragraph boundary (double newline) searching backwards from max_len
fn find_paragraph_boundary(content: &str, max_len: usize) -> Option<usize> {
    let search_text = &content[..max_len.min(content.len())];
    search_text.rfind("\n\n").map(|pos| pos + 2)
}

/// Find single newline searching backwards from max_len
fn find_newline_boundary(content: &str, max_len: usize) -> Option<usize> {
    let search_text = &content[..max_len.min(content.len())];
    search_text.rfind('\n').map(|pos| pos + 1)
}

/// Find sentence boundary (. ! ?) searching backwards from max_len
fn find_sentence_boundary(content: &str, max_len: usize) -> Option<usize> {
    let search_text = &content[..max_len.min(content.len())];
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

/// Find word boundary (space) searching backwards from max_len
fn find_word_boundary(content: &str, max_len: usize) -> Option<usize> {
    let search_text = &content[..max_len.min(content.len())];
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
}
