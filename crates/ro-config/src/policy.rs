//! Policy YAML parser and checker.
//!
//! Loads `$XDG_CONFIG_HOME/ro/policies.yaml`, validates the schema,
//! and checks repos against the declared rules.
//!
//! # Example policy
//!
//! ```yaml
//! required_files:
//!   - README.md
//!   - LICENSE
//!   - SECURITY.md
//!
//! github_labels:
//!   - bug
//!   - enhancement
//!   - good first issue
//!   - priority/high
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Top-level policy document.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    /// Files that must exist in every managed repo.
    #[serde(default)]
    pub required_files: Vec<String>,

    /// GitHub labels that should exist on every managed repo.
    #[serde(default)]
    pub github_labels: Vec<String>,

    /// Extensible key/value rules for future policy types.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

impl Policy {
    /// Load a policy from a YAML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading policy file: {}", path.display()))?;
        Self::from_yaml(&text)
    }

    /// Parse a policy from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let policy: Policy = serde_yaml::from_str(yaml).context("parsing policy YAML")?;
        Ok(policy)
    }

    /// Return a default policy that enforces common best practices.
    pub fn default_policy() -> Self {
        Self {
            required_files: vec!["README.md".into(), "LICENSE".into()],
            github_labels: vec![
                "bug".into(),
                "enhancement".into(),
                "good first issue".into(),
            ],
            extra: HashMap::new(),
        }
    }

    /// Check a repo path against the policy. Returns a list of violations.
    pub fn check_files(&self, repo_path: &Path) -> Vec<FileViolation> {
        let mut violations = Vec::new();
        for required in &self.required_files {
            let full = repo_path.join(required);
            if !full.exists() {
                violations.push(FileViolation {
                    path: required.clone(),
                    message: format!("required file missing: {required}"),
                });
            }
        }
        violations
    }

    /// Check GitHub labels against the policy. Returns labels that are missing.
    pub fn check_labels(&self, existing: &[String]) -> Vec<String> {
        let existing_lower: Vec<String> = existing.iter().map(|s| s.to_ascii_lowercase()).collect();
        self.github_labels
            .iter()
            .filter(|required| !existing_lower.contains(&required.to_ascii_lowercase()))
            .cloned()
            .collect()
    }

    /// Returns true if the policy has no rules.
    pub fn is_empty(&self) -> bool {
        self.required_files.is_empty() && self.github_labels.is_empty() && self.extra.is_empty()
    }
}

/// A single file-level policy violation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileViolation {
    pub path: String,
    pub message: String,
}

/// A complete policy check report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyReport {
    pub file_violations: Vec<FileViolation>,
    pub missing_labels: Vec<String>,
}

impl PolicyReport {
    pub fn is_clean(&self) -> bool {
        self.file_violations.is_empty() && self.missing_labels.is_empty()
    }

    pub fn total_violations(&self) -> usize {
        self.file_violations.len() + self.missing_labels.len()
    }
}

/// Run a full policy check against a repo path and existing labels.
pub fn check_policy(policy: &Policy, repo_path: &Path, existing_labels: &[String]) -> PolicyReport {
    let file_violations = policy.check_files(repo_path);
    let missing_labels = policy.check_labels(existing_labels);
    PolicyReport {
        file_violations,
        missing_labels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_full_policy() {
        let yaml = r#"
required_files:
  - README.md
  - LICENSE
  - SECURITY.md
github_labels:
  - bug
  - enhancement
"#;
        let p = Policy::from_yaml(yaml).unwrap();
        assert_eq!(
            p.required_files,
            vec!["README.md", "LICENSE", "SECURITY.md"]
        );
        assert_eq!(p.github_labels, vec!["bug", "enhancement"]);
    }

    #[test]
    fn parse_empty_policy() {
        let p = Policy::from_yaml("").unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn parse_partial_policy() {
        let yaml = "required_files:\n  - README.md\n";
        let p = Policy::from_yaml(yaml).unwrap();
        assert_eq!(p.required_files, vec!["README.md"]);
        assert!(p.github_labels.is_empty());
    }

    #[test]
    fn check_files_detects_missing() {
        let dir = tempfile::tempdir().unwrap();
        // Create only README.md, not LICENSE
        fs::write(dir.path().join("README.md"), "# Hi").unwrap();

        let p = Policy::default_policy();
        let violations = p.check_files(dir.path());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "LICENSE");
    }

    #[test]
    fn check_files_all_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "# Hi").unwrap();
        fs::write(dir.path().join("LICENSE"), "MIT").unwrap();

        let p = Policy::default_policy();
        let violations = p.check_files(dir.path());
        assert!(violations.is_empty());
    }

    #[test]
    fn check_labels_missing() {
        let p = Policy::default_policy();
        let existing = vec!["bug".into(), "docs".into()];
        let missing = p.check_labels(&existing);
        assert_eq!(missing, vec!["enhancement", "good first issue"]);
    }

    #[test]
    fn check_labels_case_insensitive() {
        let p = Policy::default_policy();
        let existing = vec![
            "Bug".into(),
            "Enhancement".into(),
            "Good First Issue".into(),
        ];
        let missing = p.check_labels(&existing);
        assert!(missing.is_empty());
    }

    #[test]
    fn full_report() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "# Hi").unwrap();

        let p = Policy::default_policy();
        let report = check_policy(&p, dir.path(), &["bug".into()]);
        assert!(!report.is_clean());
        assert_eq!(report.total_violations(), 3); // LICENSE missing + 2 labels missing
    }

    #[test]
    fn default_policy_has_rules() {
        let p = Policy::default_policy();
        assert!(!p.is_empty());
        assert_eq!(p.required_files.len(), 2);
        assert_eq!(p.github_labels.len(), 3);
    }

    #[test]
    fn extra_fields_preserved() {
        let yaml = r#"
required_files:
  - README.md
custom_rule:
  enabled: true
  threshold: 42
"#;
        let p = Policy::from_yaml(yaml).unwrap();
        assert!(p.extra.contains_key("custom_rule"));
    }
}
