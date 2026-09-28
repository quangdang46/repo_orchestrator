//! Default denylist paths.

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// Default denylist glob patterns.
///
/// These paths are never staged, committed, or touched by AI agents.
///
/// The four credential entries carry a `**/` prefix so they match at **any**
/// depth below the repo root, not just as the exact repo-relative path. They
/// used to be depth-less (`.env`, `id_rsa`, …), which meant `cfgdir/.env` and
/// `a/b/c/.env` were staged, committed and pushed while the sibling
/// `**/target/**` patterns correctly blocked at any depth — an omission that
/// reads as deliberate because the three patterns beside them were not
/// omitted. FEATURES.md promises these are "never committed" without
/// qualifying depth, so the code now matches the promise.
pub const DEFAULT_DENYLIST: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "*.pem",
    "*.key",
    "**/id_rsa",
    "**/id_ed25519",
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

    /// A `.env` at any depth below the repo root is denied, not only the
    /// one sitting at the root. The four depth-less entries used to match
    /// only the exact repo-relative path, so `cfgdir/.env` and `a/b/c/.env`
    /// were committed and pushed while the sibling `*.pem` / `**/target/**`
    /// patterns blocked at any depth — which made the omission look
    /// deliberate when it was not.
    #[test]
    fn denies_env_and_keys_at_any_depth() {
        let d = Denylist::new_default().unwrap();
        for p in [
            "sub/.env",
            "sub/.env.local",
            "a/b/c/.env",
            "a/b/c/.env.production",
            "sub/id_rsa",
            "sub/id_ed25519",
        ] {
            assert!(d.is_denied(p), "{p} must be denied at any depth");
        }
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
