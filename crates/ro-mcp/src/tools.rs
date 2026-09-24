//! MCP tool definitions.
//!
//! Exposes ro operations as MCP tools for AI agents.
//! See ADDITION.md §A3 for the canonical tool list.

use serde::{Deserialize, Serialize};

/// A tool definition for the MCP protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// List all available ro tools.
pub fn list_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "ro_repos".into(),
            description: "List all tracked repos".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "ro_health".into(),
            description: "Get health status for a repo".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_id": { "type": "string" }
                }
            }),
        },
        ToolDef {
            name: "ro_context".into(),
            description: "Get context for a repo".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_id": { "type": "string" }
                }
            }),
        },
        ToolDef {
            name: "ro_plan_create".into(),
            description: "Create a review plan for a repo".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "repo": { "type": "string" },
                    "summary": { "type": "string" },
                    "risk": { "type": "string", "enum": ["low", "medium", "high"] }
                },
                "required": ["repo"]
            }),
        },
        ToolDef {
            name: "ro_plan_list".into(),
            description: "List existing review plans".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Optional status filter" }
                }
            }),
        },
        ToolDef {
            name: "ro_plan_apply".into(),
            description: "Apply a previously-approved review plan".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" }
                },
                "required": ["plan_id"]
            }),
        },
        ToolDef {
            name: "ro_sweep_agent_plan".into(),
            description: "Plan a sweep on a repo without applying".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "repo": { "type": "string" }
                }
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_tools_returns_expected_tools() {
        let tools = list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"ro_repos"));
        assert!(names.contains(&"ro_health"));
        assert!(names.contains(&"ro_plan_create"));
        assert!(names.contains(&"ro_plan_apply"));
        assert!(names.contains(&"ro_sweep_agent_plan"));
    }

    #[test]
    fn all_tools_have_parameters() {
        let tools = list_tools();
        for tool in &tools {
            assert!(
                tool.parameters.is_object(),
                "{} has non-object params",
                tool.name
            );
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
        }
    }
}
