//! Tool profile enforcement - extends core ToolProfile with permission checking
//!
//! Profiles control what the **scheduler** can do internally. Claude's access
//! to CLI subcommands is governed by the system prompt and which commands are
//! available on the system.

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
            // Minimal: no internal tools (read-only agents)
            Self::Minimal => Some(HashSet::new()),
            // Standard: can run scripts via scheduler's ExecTool
            Self::Standard => Some(HashSet::from(["exec"])),
            // Full: all internal tools
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
    fn minimal_profile_allows_no_tools() {
        let profile = ToolProfile::Minimal;
        assert!(!profile.allows("exec"));
        assert!(!profile.allows("gmail"));
        assert!(!profile.allows("arbitrary_tool"));
    }

    #[test]
    fn standard_profile_allows_exec_only() {
        let profile = ToolProfile::Standard;
        assert!(profile.allows("exec"));
        assert!(!profile.allows("gmail"));
        assert!(!profile.allows("arbitrary_tool"));
    }

    #[test]
    fn full_profile_allows_all_tools() {
        let profile = ToolProfile::Full;
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
    fn allowed_tools_returns_empty_set_for_minimal_profile() {
        let profile = ToolProfile::Minimal;
        let tools = profile.allowed_tools().unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn allowed_tools_returns_exec_for_standard_profile() {
        let profile = ToolProfile::Standard;
        let tools = profile.allowed_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools.contains("exec"));
    }
}
