//! Tool profile enforcement - extends core ToolProfile with permission checking

use std::collections::HashSet;
use threshold_core::ToolProfile;

/// Extension trait for ToolProfile to check tool permissions
pub trait ToolProfileExt {
    /// Get the set of allowed tools for this profile.
    /// Returns None for Full profile (allows all tools).
    fn allowed_tools(&self) -> Option<HashSet<&'static str>>;

    /// Check if a tool is allowed in this profile
    fn allows(&self, tool_name: &str) -> bool;
}

impl ToolProfileExt for ToolProfile {
    fn allowed_tools(&self) -> Option<HashSet<&'static str>> {
        match self {
            Self::Minimal => Some(HashSet::from(["web_search", "web_fetch", "read"])),
            Self::Coding => Some(HashSet::from([
                "web_search", "web_fetch", "read", "write", "edit", "exec",
            ])),
            Self::Full => None, // None means "allow all"
        }
    }

    fn allows(&self, tool_name: &str) -> bool {
        match self.allowed_tools() {
            Some(set) => set.contains(tool_name),
            None => true, // Full profile allows all
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_profile_allows_only_read_only_tools() {
        let profile = ToolProfile::Minimal;
        assert!(profile.allows("read"));
        assert!(profile.allows("web_search"));
        assert!(profile.allows("web_fetch"));
        assert!(!profile.allows("write"));
        assert!(!profile.allows("edit"));
        assert!(!profile.allows("exec"));
        assert!(!profile.allows("gmail"));
    }

    #[test]
    fn coding_profile_allows_read_write_tools() {
        let profile = ToolProfile::Coding;
        assert!(profile.allows("read"));
        assert!(profile.allows("write"));
        assert!(profile.allows("edit"));
        assert!(profile.allows("exec"));
        assert!(profile.allows("web_search"));
        assert!(profile.allows("web_fetch"));
        assert!(!profile.allows("gmail"));
    }

    #[test]
    fn full_profile_allows_all_tools() {
        let profile = ToolProfile::Full;
        assert!(profile.allows("read"));
        assert!(profile.allows("write"));
        assert!(profile.allows("exec"));
        assert!(profile.allows("gmail"));
        assert!(profile.allows("arbitrary_tool"));
    }

    #[test]
    fn allowed_tools_returns_none_for_full_profile() {
        let profile = ToolProfile::Full;
        assert!(profile.allowed_tools().is_none());
    }

    #[test]
    fn allowed_tools_returns_set_for_minimal_profile() {
        let profile = ToolProfile::Minimal;
        let tools = profile.allowed_tools().unwrap();
        assert_eq!(tools.len(), 3);
        assert!(tools.contains("read"));
        assert!(tools.contains("web_search"));
        assert!(tools.contains("web_fetch"));
    }

    #[test]
    fn allowed_tools_returns_set_for_coding_profile() {
        let profile = ToolProfile::Coding;
        let tools = profile.allowed_tools().unwrap();
        assert_eq!(tools.len(), 6);
        assert!(tools.contains("read"));
        assert!(tools.contains("write"));
        assert!(tools.contains("edit"));
        assert!(tools.contains("exec"));
    }
}
