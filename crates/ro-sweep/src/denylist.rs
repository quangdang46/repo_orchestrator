//! Default denylist paths.

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// Default denylist glob patterns.
///
/// These paths are never staged, committed, or touched by AI agents.
pub const DEFAULT_DENYLIST: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "id_rsa",
    "id_ed25519",
    "**/.git/**",
    "**/target/**",
    "**/node_modules/**",
];

/// Compiled denylist matcher.
#[derive(Debug, Clone)]
pub struct Denylist {
    patterns: Vec<String>,
    set: GlobSet,
}

impl Denylist {
    /// Build a denylist from glob pattern strings.
    pub fn new(patterns: &[&str]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut collected = Vec::with_capacity(patterns.len());
        for pat in patterns {
            builder.add(Glob::new(pat)?);
            collected.push(pat.to_string());
        }
        let set = builder.build()?;
        Ok(Self {
            patterns: collected,
            set,
        })
    }

    /// Build from the default denylist patterns.
    pub fn new_default() -> Result<Self> {
        Self::new(DEFAULT_DENYLIST)
    }

    /// Return true if `path` matches any denylisted pattern.
    ///
    /// Paths should use forward slashes (relative to repo root).
    pub fn is_denied(&self, path: &str) -> bool {
        self.set.is_match(path)
    }

    /// Filter a list of paths, returning only the denied ones.
    pub fn filter_denied<'a>(&self, paths: &'a [String]) -> Vec<&'a str> {
        paths
            .iter()
            .filter(|p| self.is_denied(p))
            .map(|p| p.as_str())
            .collect()
    }

    /// Return the raw patterns.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_builds_ok() {
        let d = Denylist::new_default().unwrap();
        assert!(!d.patterns().is_empty());
    }

    #[test]
    fn denies_env_files() {
        let d = Denylist::new_default().unwrap();
        assert!(d.is_denied(".env"));
        assert!(d.is_denied(".env.local"));
        assert!(d.is_denied(".env.production"));
    }

    #[test]
    fn denies_key_files() {
        let d = Denylist::new_default().unwrap();
        assert!(d.is_denied("deploy.pem"));
        assert!(d.is_denied("tls.key"));
        assert!(d.is_denied("id_rsa"));
        assert!(d.is_denied("id_ed25519"));
    }

    #[test]
    fn denies_git_and_target() {
        let d = Denylist::new_default().unwrap();
        assert!(d.is_denied(".git/config"));
        assert!(d.is_denied("target/debug/ro"));
        assert!(d.is_denied("node_modules/lodash/index.js"));
    }

    #[test]
    fn allows_normal_source() {
        let d = Denylist::new_default().unwrap();
        assert!(!d.is_denied("src/main.rs"));
        assert!(!d.is_denied("Cargo.toml"));
        assert!(!d.is_denied("README.md"));
    }

    #[test]
    fn filter_denied_mixed() {
        let d = Denylist::new_default().unwrap();
        let paths: Vec<String> = vec![
            "src/main.rs".into(),
            ".env".into(),
            "Cargo.toml".into(),
            "id_rsa".into(),
        ];
        let denied = d.filter_denied(&paths);
        assert_eq!(denied, vec![".env", "id_rsa"]);
    }

    #[test]
    fn custom_patterns() {
        let d = Denylist::new(&["*.secret", "private/**"]).unwrap();
        assert!(d.is_denied("api.secret"));
        assert!(d.is_denied("private/keys.txt"));
        assert!(!d.is_denied("public/keys.txt"));
    }
}
