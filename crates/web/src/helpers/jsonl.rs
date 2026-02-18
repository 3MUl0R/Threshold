//! JSONL file reader with pagination support.

use std::path::Path;

use serde_json::Value;

/// Read a page of entries from a JSONL file in reverse chronological order.
///
/// Returns `(entries, total_count)`. Entries are reversed (newest first).
/// Malformed lines are skipped with a tracing warning.
pub async fn read_jsonl_page(
    path: &Path,
    offset: usize,
    limit: usize,
) -> anyhow::Result<(Vec<Value>, usize)> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((vec![], 0));
        }
        Err(e) => return Err(e.into()),
    };

    let mut entries: Vec<Value> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("Skipping malformed JSONL line: {e}");
                None
            }
        })
        .collect();

    let total = entries.len();

    // Reverse for newest-first ordering
    entries.reverse();

    // Apply pagination
    let page: Vec<Value> = entries.into_iter().skip(offset).take(limit).collect();

    Ok((page, total))
}

/// Count the number of valid JSONL entries in a file.
pub async fn count_lines(path: &Path) -> anyhow::Result<usize> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    Ok(content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;

    #[tokio::test]
    async fn read_empty_file() {
        let file = NamedTempFile::new().unwrap();
        let (entries, total) = read_jsonl_page(file.path(), 0, 10).await.unwrap();
        assert_eq!(entries.len(), 0);
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn read_missing_file() {
        let (entries, total) = read_jsonl_page(Path::new("/nonexistent/file.jsonl"), 0, 10)
            .await
            .unwrap();
        assert_eq!(entries.len(), 0);
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn read_entries_newest_first() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"n": 1}}"#).unwrap();
        writeln!(file, r#"{{"n": 2}}"#).unwrap();
        writeln!(file, r#"{{"n": 3}}"#).unwrap();

        let (entries, total) = read_jsonl_page(file.path(), 0, 10).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["n"], 3); // newest first
        assert_eq!(entries[2]["n"], 1); // oldest last
    }

    #[tokio::test]
    async fn pagination_offset_and_limit() {
        let mut file = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(file, r#"{{"n": {i}}}"#).unwrap();
        }

        // Page 2: offset=3, limit=3 → entries 7, 6, 5 (reversed)
        let (entries, total) = read_jsonl_page(file.path(), 3, 3).await.unwrap();
        assert_eq!(total, 10);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["n"], 7);
        assert_eq!(entries[1]["n"], 6);
        assert_eq!(entries[2]["n"], 5);
    }

    #[tokio::test]
    async fn malformed_lines_skipped() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"valid": true}}"#).unwrap();
        writeln!(file, "not json at all").unwrap();
        writeln!(file, r#"{{"also_valid": true}}"#).unwrap();

        let (entries, total) = read_jsonl_page(file.path(), 0, 10).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn count_lines_works() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"a": 1}}"#).unwrap();
        writeln!(file, r#"{{"b": 2}}"#).unwrap();
        writeln!(file, "").unwrap(); // empty line

        assert_eq!(count_lines(file.path()).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn count_lines_missing_file() {
        assert_eq!(
            count_lines(Path::new("/nonexistent/file.jsonl"))
                .await
                .unwrap(),
            0
        );
    }
}
