//! Path resolution utilities for Threshold configuration paths.

use std::path::{Path, PathBuf};

/// Resolve a configuration path string to an absolute `PathBuf`.
///
/// Rules:
/// 1. If `path` starts with `~/`, expand `~` to the user's home directory.
/// 2. If the (possibly expanded) path is absolute, return it as-is.
/// 3. Otherwise, resolve it relative to `data_dir`.
pub fn resolve_path(path: &str, data_dir: &Path) -> PathBuf {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else {
        PathBuf::from(path)
    };

    if expanded.is_absolute() {
        expanded
    } else {
        data_dir.join(expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let result = resolve_path("~/Documents/notes.md", Path::new("/data"));
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("Documents/notes.md"));
    }

    #[test]
    fn relative_path_resolved_against_data_dir() {
        let result = resolve_path("schedules.json", Path::new("/var/threshold"));
        assert_eq!(result, PathBuf::from("/var/threshold/schedules.json"));
    }

    #[test]
    fn absolute_path_passes_through() {
        let result = resolve_path("/etc/threshold/config.toml", Path::new("/data"));
        assert_eq!(result, PathBuf::from("/etc/threshold/config.toml"));
    }
}
