//! Tool registry and execution engine

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use threshold_core::{AuditTrail, Result, ThresholdError, ToolProfile};
use tracing;

use crate::{Tool, ToolContext, ToolProfileExt, ToolResult};

/// Configuration for the tools system
#[derive(Clone)]
pub struct ToolsConfig {
    /// Audit trail for tool execution logs
    pub audit: Arc<AuditTrail>,
}

impl ToolsConfig {
    /// Create a new ToolsConfig with default audit path
    pub fn new() -> Self {
        let audit = Arc::new(AuditTrail::new(PathBuf::from(
            ".threshold/audit/tools.jsonl",
        )));
        Self { audit }
    }

    /// Create a ToolsConfig with a custom audit path
    pub fn with_audit_path(path: PathBuf) -> Self {
        let audit = Arc::new(AuditTrail::new(path));
        Self { audit }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Audit trail entry for tool execution
#[derive(Debug, Serialize)]
struct AuditEntry {
    ts: chrono::DateTime<Utc>,
    tool: String,
    params: Value,
    agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    portal: Option<String>,
    duration_ms: u128,
    success: bool,
    result_size: usize,
}

/// Tool registry - manages tool registration and execution
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    audit: Arc<AuditTrail>,
}

impl ToolRegistry {
    /// Create a new tool registry
    pub fn new(config: &ToolsConfig) -> Self {
        Self {
            tools: HashMap::new(),
            audit: Arc::clone(&config.audit),
        }
    }

    /// Register a tool
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        tracing::debug!("Registering tool: {}", name);
        self.tools.insert(name, tool);
    }

    /// Execute a tool by name
    ///
    /// Handles:
    /// 1. Permission check (is tool in the active profile?)
    /// 2. Execution with cancellation support
    /// 3. Result size guard (truncate if > 100KB)
    /// 4. Audit log completion (duration, success/failure)
    pub async fn execute(
        &self,
        tool_name: &str,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult> {
        // 1. Check if tool exists
        let tool = self
            .tools
            .get(tool_name)
            .ok_or_else(|| ThresholdError::ToolError {
                tool: tool_name.to_string(),
                message: format!("Tool '{}' not found", tool_name),
            })?;

        // 2. Check profile permissions
        if !ctx.profile.allows(tool_name) {
            return Err(ThresholdError::ToolNotPermitted {
                tool: tool_name.to_string(),
                profile: format!("{:?}", ctx.profile),
            });
        }

        // 3. Execute tool with timing and cancellation support
        let start = Instant::now();
        let result = tokio::select! {
            result = tool.execute(params.clone(), ctx) => result,
            _ = ctx.cancellation.cancelled() => {
                return Err(ThresholdError::SchedulerShutdown);
            }
        };
        let duration = start.elapsed();

        // 4. Truncate result if needed
        let result = match result {
            Ok(r) => Ok(r.truncate()),
            Err(e) => Err(e),
        };

        // 5. Write audit log
        let success = result.is_ok();
        let result_size = result
            .as_ref()
            .map(|r| r.content.len())
            .unwrap_or(0);

        self.audit
            .append_raw(&AuditEntry {
                ts: Utc::now(),
                tool: tool_name.to_string(),
                params,
                agent: ctx.agent_id.clone(),
                conversation: ctx.conversation_id.map(|id| id.0.to_string()),
                portal: ctx.portal_id.map(|id| id.0.to_string()),
                duration_ms: duration.as_millis(),
                success,
                result_size,
            })
            .await
            .ok(); // Don't fail execution if audit write fails

        result
    }

    /// Get tools available for a given profile
    pub fn tools_for_profile(&self, profile: &ToolProfile) -> Vec<&dyn Tool> {
        self.tools
            .values()
            .filter_map(|tool| {
                if profile.allows(tool.name()) {
                    Some(tool.as_ref())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get tool schemas as JSON (for system prompt injection)
    pub fn schemas_for_profile(&self, profile: &ToolProfile) -> Vec<Value> {
        self.tools_for_profile(profile)
            .into_iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "parameters": tool.schema(),
                })
            })
            .collect()
    }

    /// List all registered tool names
    pub fn list(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::tempdir;

    // Mock tool for testing
    struct MockTool {
        name: String,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "A mock tool for testing"
        }

        fn schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {}
            })
        }

        async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
            Ok(ToolResult::success("mock output"))
        }
    }

    #[test]
    fn registry_new_creates_empty_registry() {
        let config = ToolsConfig::default();
        let registry = ToolRegistry::new(&config);
        assert_eq!(registry.list().len(), 0);
    }

    #[test]
    fn registry_register_adds_tool() {
        let mut registry = ToolRegistry::new(&ToolsConfig::default());
        let tool = Arc::new(MockTool {
            name: "test".to_string(),
        });
        registry.register(tool);
        assert_eq!(registry.list().len(), 1);
        assert!(registry.list().contains(&"test"));
    }

    #[test]
    fn registry_list_returns_all_tool_names() {
        let mut registry = ToolRegistry::new(&ToolsConfig::default());
        registry.register(Arc::new(MockTool {
            name: "tool1".to_string(),
        }));
        registry.register(Arc::new(MockTool {
            name: "tool2".to_string(),
        }));
        let names = registry.list();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"tool1"));
        assert!(names.contains(&"tool2"));
    }

    #[tokio::test]
    async fn registry_execute_runs_tool() {
        let mut registry = ToolRegistry::new(&ToolsConfig::default());
        registry.register(Arc::new(MockTool {
            name: "test".to_string(),
        }));

        let ctx = ToolContext::new("test-agent");
        let result = registry.execute("test", json!({}), &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "mock output");
    }

    #[tokio::test]
    async fn registry_execute_fails_for_unknown_tool() {
        let registry = ToolRegistry::new(&ToolsConfig::default());
        let ctx = ToolContext::new("test-agent");
        let result = registry.execute("unknown", json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ThresholdError::ToolError { .. }));
    }

    #[tokio::test]
    async fn registry_execute_enforces_profile() {
        let mut registry = ToolRegistry::new(&ToolsConfig::default());
        registry.register(Arc::new(MockTool {
            name: "exec".to_string(),
        }));

        // Minimal profile should block exec
        let ctx = ToolContext::new("test-agent").with_profile(ToolProfile::Minimal);
        let result = registry.execute("exec", json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ThresholdError::ToolNotPermitted { .. }
        ));
    }

    #[tokio::test]
    async fn registry_execute_writes_audit_log() {
        let temp_dir = tempdir().unwrap();
        let audit_path = temp_dir.path().join("tools.jsonl");
        let config = ToolsConfig::with_audit_path(audit_path.clone());

        let mut registry = ToolRegistry::new(&config);
        registry.register(Arc::new(MockTool {
            name: "test".to_string(),
        }));

        let ctx = ToolContext::new("test-agent");
        registry.execute("test", json!({}), &ctx).await.unwrap();

        // Verify audit log was created and contains an entry
        assert!(audit_path.exists());
        let content = std::fs::read_to_string(&audit_path).unwrap();
        assert!(content.contains("\"tool\":\"test\""));
        assert!(content.contains("\"success\":true"));
    }

    #[test]
    fn tools_for_profile_filters_by_profile() {
        let mut registry = ToolRegistry::new(&ToolsConfig::default());
        registry.register(Arc::new(MockTool {
            name: "read".to_string(),
        }));
        registry.register(Arc::new(MockTool {
            name: "exec".to_string(),
        }));

        let minimal_tools = registry.tools_for_profile(&ToolProfile::Minimal);
        assert_eq!(minimal_tools.len(), 1);
        assert_eq!(minimal_tools[0].name(), "read");

        let coding_tools = registry.tools_for_profile(&ToolProfile::Coding);
        assert_eq!(coding_tools.len(), 2);

        let full_tools = registry.tools_for_profile(&ToolProfile::Full);
        assert_eq!(full_tools.len(), 2);
    }

    #[test]
    fn schemas_for_profile_returns_tool_schemas() {
        let mut registry = ToolRegistry::new(&ToolsConfig::default());
        registry.register(Arc::new(MockTool {
            name: "read".to_string(),
        }));

        let schemas = registry.schemas_for_profile(&ToolProfile::Minimal);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["name"], "read");
        assert_eq!(schemas[0]["description"], "A mock tool for testing");
        assert!(schemas[0]["parameters"].is_object());
    }
}
