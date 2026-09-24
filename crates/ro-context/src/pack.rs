//! Context pack generation.
//!
//! Generates structured context packs for AI agents.

use serde::{Deserialize, Serialize};

/// A context pack containing repo information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPack {
    pub repo_id: String,
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub health: serde_json::Value,
    pub open_issues: Vec<serde_json::Value>,
    pub open_prs: Vec<serde_json::Value>,
    pub recent_commits: Vec<serde_json::Value>,
}

impl ContextPack {
    pub fn new(repo_id: &str, owner: &str, name: &str) -> Self {
        Self {
            repo_id: repo_id.into(),
            owner: owner.into(),
            name: name.into(),
            branch: "main".into(),
            health: serde_json::json!({}),
            open_issues: Vec::new(),
            open_prs: Vec::new(),
            recent_commits: Vec::new(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_pack_new_has_defaults() {
        let pack = ContextPack::new("id-1", "owner", "repo");
        assert_eq!(pack.repo_id, "id-1");
        assert_eq!(pack.branch, "main");
    }

    #[test]
    fn context_pack_to_json() {
        let pack = ContextPack::new("id-1", "owner", "repo");
        let json = pack.to_json();
        assert!(json.get("repo_id").is_some());
    }
}
