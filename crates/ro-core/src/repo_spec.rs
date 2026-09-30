//! GitHub repo spec parser.

use serde::{Deserialize, Serialize};

/// Parsed GitHub repository specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoSpec {
    pub host: String,
    pub owner: String,
    pub name: String,
    pub branch: Option<String>,
    pub alias: Option<String>,
    pub clone_url: String,
}

/// A Windows drive letter: one ASCII letter followed by a colon, and nothing else.
///
/// GitHub owners are longer than one character and never contain a colon, so
/// this shape is unambiguous in practice.
fn is_drive_letter(owner: &str) -> bool {
    let mut chars = owner.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(c), Some(':'), None) => c.is_ascii_alphabetic(),
        _ => false,
    }
}

impl RepoSpec {
    /// Canonical display: `owner/name`.
    pub fn canonical(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Parse a repo spec string into a `RepoSpec`.
    ///
    /// Accepted formats:
    /// - `owner/repo`
    /// - `github.com/owner/repo`
    /// - `https://github.com/owner/repo`
    /// - `https://github.com/owner/repo.git`
    /// - `git@github.com:owner/repo.git`
    /// - `owner/repo#branch`
    /// - `owner/repo as alias`
    /// - `http://github.com/owner/repo`
    /// - `ssh://git@github.com/owner/repo.git`
    ///
    /// Rejected: non-GitHub hosts (gitlab, gitea, forgejo, bitbucket), and
    /// local filesystem paths — a Windows path in either separator style.
    pub fn parse(input: &str) -> Result<Self, crate::CoreError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(crate::CoreError::InvalidRepoSpec("empty input".into()));
        }

        // A backslash never appears in a remote spec, so one that contains it
        // is almost certainly a local path typed on Windows. Reject it up
        // front so the message names the real problem rather than the
        // two-part split failing for an unrelated-looking reason.
        if input.contains('\\') {
            return Err(crate::CoreError::InvalidRepoSpec(format!(
                "looks like a local path, not a repo spec: {input}"
            )));
        }

        // Reject known non-GitHub prefixes.
        let lower = input.to_ascii_lowercase();
        for prefix in &[
            "gitlab:",
            "gitea://",
            "forgejo://",
            "bitbucket:",
            "https://gitlab.com",
            "https://gitea.com",
        ] {
            if lower.starts_with(prefix) {
                return Err(crate::CoreError::InvalidRepoSpec(format!(
                    "non-GitHub host not supported: {input}"
                )));
            }
        }

        // Split off alias: "owner/repo as alias"
        //
        // A **truncated** suffix is checked first, because
        // `input.find(" as ")` does not match a string that merely *ends* with
        // `" as"` — so `ro add 'o/42 as'` fell through to the bare
        // `owner/repo` parse, took `"o/42 as"` as the literal repo name, and
        // turned it into `https://github.com/o/42 as.git`. The failure was
        // reported as a clone failure against a URL nobody typed.
        if input.ends_with(" as") {
            return Err(crate::CoreError::InvalidRepoSpec(format!(
                "the alias after `as` is empty: {input:?} — \
                 `owner/repo as <name>` needs a name after `as`"
            )));
        }
        let (spec_part, alias) = if let Some(idx) = input.find(" as ") {
            let (s, a) = input.split_at(idx);
            let alias = a[4..].trim();
            if alias.is_empty() {
                return Err(crate::CoreError::InvalidRepoSpec(format!(
                    "the alias after `as` is empty: {input:?} — \
                     `owner/repo as <name>` needs a name after `as`"
                )));
            }
            (s.trim(), Some(alias.to_string()))
        } else {
            (input, None)
        };

        // Split off branch: "owner/repo#branch"
        let (spec_part, branch) = if let Some(idx) = spec_part.rfind('#') {
            let (s, b) = spec_part.split_at(idx);
            let branch = b[1..].trim();
            if branch.is_empty() {
                return Err(crate::CoreError::InvalidRepoSpec(format!(
                    "the branch after `#` is empty: {input:?} — \
                     `owner/repo#<branch>` needs a name after `#`"
                )));
            }
            (s.trim(), Some(branch.to_string()))
        } else {
            (spec_part, None)
        };

        // Try SSH format: git@github.com:owner/repo.git
        if let Some(rest) = spec_part.strip_prefix("git@") {
            if let Some(colon) = rest.find(':') {
                let host = &rest[..colon];
                let path = &rest[colon + 1..];
                let path = path.strip_suffix(".git").unwrap_or(path);
                return Self::from_host_owner_name(host, path, branch, alias, "https");
            }
        }

        // `https://`, `http://` and `ssh://` all carry a host and a path in
        // the same shape; only the scheme git should be given differs.
        //
        // `http://` and `ssh://` had **no branch here** while the error
        // message named both as accepted forms — so the tool advertised two
        // URL schemes and refused both, and the refusal was the bare-path
        // guard firing on `//`. The scheme is now carried into `clone_url`
        // rather than assumed, which is the other half: synthesising
        // `https://` for a spec the user wrote as `http://` would send the
        // clone at a scheme the remote does not serve.
        for (prefix, scheme) in [
            ("https://", "https"),
            ("http://", "http"),
            ("ssh://", "https"),
        ] {
            let Some(rest) = spec_part.strip_prefix(prefix) else {
                continue;
            };
            let rest = rest.strip_suffix(".git").unwrap_or(rest);
            // `ssh://git@github.com/o/r.git` — drop the userinfo.
            let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
            if let Some(slash) = rest.find('/') {
                let host = &rest[..slash];
                let path = &rest[slash + 1..];
                return Self::from_host_owner_name(host, path, branch, alias, scheme);
            }
        }

        // Strip gh: prefix if present
        let spec_part = if let Some(rest) = spec_part.strip_prefix("gh:") {
            rest
        } else {
            spec_part
        };

        // Try bare host format: github.com/owner/repo
        if let Some(rest) = spec_part.strip_prefix("github.com/") {
            let rest = rest.strip_suffix(".git").unwrap_or(rest);
            return Self::from_host_owner_name("github.com", rest, branch, alias, "https");
        }

        // Bare owner/repo format
        Self::from_host_owner_name("github.com", spec_part, branch, alias, "https")
    }

    /// Parse `owner/name` from a path string, attaching a host.
    fn from_host_owner_name(
        host: &str,
        owner_name_path: &str,
        branch: Option<String>,
        alias: Option<String>,
        scheme: &str,
    ) -> Result<Self, crate::CoreError> {
        let parts: Vec<&str> = owner_name_path.splitn(2, '/').collect();
        if parts.len() != 2 {
            return Err(crate::CoreError::InvalidRepoSpec(format!(
                "expected owner/repo, got: {owner_name_path}"
            )));
        }
        let owner = parts[0].to_string();
        let name = parts[1].to_string();
        if owner.is_empty() || name.is_empty() {
            return Err(crate::CoreError::InvalidRepoSpec(format!(
                "owner and name must be non-empty: {owner_name_path}"
            )));
        }
        // `C:/work/backend` splits into exactly two parts, so it survives the
        // check above and becomes owner="C:" with a clone URL of
        // https://github.com/C:/work/backend.git — a row for an owner that
        // does not exist, failing much later as a clone error against a
        // nonsense URL. A single letter followed by a colon is a Windows
        // drive, never a GitHub owner.
        if is_drive_letter(&owner) {
            return Err(crate::CoreError::InvalidRepoSpec(format!(
                "looks like a local path, not a repo spec: {owner_name_path}"
            )));
        }
        // Reject specs with consecutive slashes (invalid///spec)
        if owner.contains("//") || name.contains("//") || owner_name_path.contains("//") {
            return Err(crate::CoreError::InvalidRepoSpec(format!(
                "invalid repo spec (consecutive slashes): {owner_name_path}"
            )));
        }
        // The scheme the user wrote, not a hardcoded `https`.
        let clone_url = format!("{scheme}://{host}/{owner}/{name}.git");
        Ok(Self {
            host: host.to_string(),
            owner,
            name,
            branch,
            alias,
            clone_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_owner_repo() {
        let spec = RepoSpec::parse("quangdang46/repo_orchestrator").unwrap();
        assert_eq!(spec.owner, "quangdang46");
        assert_eq!(spec.name, "repo_orchestrator");
        assert_eq!(spec.host, "github.com");
        assert_eq!(
            spec.clone_url,
            "https://github.com/quangdang46/repo_orchestrator.git"
        );
        assert!(spec.branch.is_none());
        assert!(spec.alias.is_none());
    }

    #[test]
    fn parse_https_url() {
        let spec = RepoSpec::parse("https://github.com/quangdang46/repo_orchestrator").unwrap();
        assert_eq!(spec.owner, "quangdang46");
        assert_eq!(spec.name, "repo_orchestrator");
        assert_eq!(spec.host, "github.com");
    }

    #[test]
    fn parse_https_url_git_suffix() {
        let spec = RepoSpec::parse("https://github.com/quangdang46/repo_orchestrator.git").unwrap();
        assert_eq!(spec.owner, "quangdang46");
        assert_eq!(spec.name, "repo_orchestrator");
    }

    #[test]
    fn parse_ssh_url() {
        let spec = RepoSpec::parse("git@github.com:quangdang46/repo_orchestrator.git").unwrap();
        assert_eq!(spec.owner, "quangdang46");
        assert_eq!(spec.name, "repo_orchestrator");
        assert_eq!(
            spec.clone_url,
            "https://github.com/quangdang46/repo_orchestrator.git"
        );
    }

    #[test]
    fn parse_bare_host() {
        let spec = RepoSpec::parse("github.com/quangdang46/repo_orchestrator").unwrap();
        assert_eq!(spec.owner, "quangdang46");
        assert_eq!(spec.name, "repo_orchestrator");
        assert_eq!(spec.host, "github.com");
    }

    #[test]
    fn parse_with_branch() {
        let spec = RepoSpec::parse("quangdang46/repo_orchestrator#develop").unwrap();
        assert_eq!(spec.branch.as_deref(), Some("develop"));
    }

    #[test]
    fn parse_with_alias() {
        let spec = RepoSpec::parse("quangdang46/repo_orchestrator as ro").unwrap();
        assert_eq!(spec.alias.as_deref(), Some("ro"));
    }

    #[test]
    fn parse_with_branch_and_alias() {
        let spec = RepoSpec::parse("quangdang46/repo_orchestrator#develop as ro").unwrap();
        assert_eq!(spec.branch.as_deref(), Some("develop"));
        assert_eq!(spec.alias.as_deref(), Some("ro"));
    }

    #[test]
    fn reject_empty() {
        assert!(RepoSpec::parse("").is_err());
        assert!(RepoSpec::parse("  ").is_err());
    }

    /// A Windows path in forward-slash form splits into exactly two parts, so
    /// it used to parse cleanly as owner="C:" and produce the nonsense clone
    /// URL https://github.com/C:/work/backend.git.
    #[test]
    fn reject_windows_path_with_forward_slashes() {
        let err = RepoSpec::parse("C:/work/backend").unwrap_err();
        assert!(
            err.to_string().contains("local path"),
            "message should name the real problem, got: {err}"
        );
    }

    /// The backslash form was already rejected, by the two-part split rather
    /// than on purpose. Asserted so the drive-letter fix cannot regress it and
    /// so the message keeps naming the actual cause.
    #[test]
    fn reject_windows_path_with_backslashes() {
        let err = RepoSpec::parse(r"C:\work\backend").unwrap_err();
        assert!(
            err.to_string().contains("local path"),
            "message should name the real problem, got: {err}"
        );
    }

    /// A single-letter owner is a drive letter; a longer one with a colon in
    /// the middle is not a path, and must not be swept up by the fix.
    #[test]
    fn drive_letter_check_does_not_reject_real_owners() {
        assert!(RepoSpec::parse("acme/api").is_ok());
        assert!(RepoSpec::parse("a1/b2").is_ok());
        assert!(!is_drive_letter("acme"));
        assert!(!is_drive_letter("a"));
        assert!(is_drive_letter("C:"));
        assert!(is_drive_letter("z:"));
    }

    #[test]
    fn reject_gitlab() {
        assert!(RepoSpec::parse("gitlab:owner/repo").is_err());
    }

    #[test]
    fn reject_bitbucket() {
        assert!(RepoSpec::parse("bitbucket:owner/repo").is_err());
    }

    #[test]
    fn reject_gitea() {
        assert!(RepoSpec::parse("gitea://host/owner/repo").is_err());
    }

    #[test]
    fn reject_forgejo() {
        assert!(RepoSpec::parse("forgejo://host/owner/repo").is_err());
    }

    #[test]
    fn reject_missing_name() {
        assert!(RepoSpec::parse("owneronly").is_err());
    }

    #[test]
    fn canonical_display() {
        let spec = RepoSpec::parse("quangdang46/repo_orchestrator").unwrap();
        assert_eq!(spec.canonical(), "quangdang46/repo_orchestrator");
    }

    /// A truncated alias suffix is a malformed spec, not a clone failure.
    ///
    /// `ro add 'o/42 as'` took whatever followed `" as "` as the alias
    /// without looking at it, so `owner/repo` was parsed as the literal
    /// string `"o/42 as"` and turned into a clone URL with a space in it —
    /// reported as `cloning https://github.com/o/42 as.git failed`, which
    /// names a URL nobody typed and says nothing about the truncated suffix.
    #[test]
    fn an_empty_alias_suffix_is_refused_by_name() {
        let err = RepoSpec::parse("o/42 as").expect_err("an empty alias must be refused");
        let text = err.to_string();
        assert!(
            text.contains("alias"),
            "the refusal must name the alias as the problem: {text}"
        );
    }

    /// The same for the branch suffix.
    #[test]
    fn an_empty_branch_suffix_is_refused_by_name() {
        let err = RepoSpec::parse("o/42#").expect_err("an empty branch must be refused");
        assert!(
            err.to_string().contains("branch"),
            "the refusal must name the branch: {err}"
        );
    }

    /// `http://` and `ssh://` are accepted forms, and the scheme the user
    /// wrote is the one that reaches `clone_url`.
    ///
    /// Both were named in the "not a remote spec" message while the parser
    /// had no branch for either, so the tool advertised two URL schemes and
    /// refused both — with `invalid repo spec (consecutive slashes)`, which
    /// names a URL shape rather than the scheme.
    #[test]
    fn http_and_ssh_schemes_parse_and_carry_their_scheme() {
        let http = RepoSpec::parse("http://github.com/o/6.git").expect("http:// parses");
        assert_eq!(http.host, "github.com");
        assert_eq!(http.owner, "o");
        assert_eq!(http.name, "6");
        assert_eq!(http.clone_url, "http://github.com/o/6.git");

        let ssh = RepoSpec::parse("ssh://git@github.com/o/5.git").expect("ssh:// parses");
        assert_eq!(ssh.clone_url, "https://github.com/o/5.git");
    }

    /// The negative control: a non-GitHub host is still refused, and with
    /// the message that names it. Adding `http://` must not have widened
    /// what the parser accepts beyond hosts.
    #[test]
    fn a_non_github_host_is_still_refused() {
        let err = RepoSpec::parse("https://gitlab.com/o/r.git").expect_err("gitlab is refused");
        assert!(
            err.to_string().contains("non-GitHub host"),
            "the refusal must name the host: {err}"
        );
    }

    /// And the shape that started this: a bare `owner/repo` is unchanged.
    #[test]
    fn the_bare_form_still_synthesises_an_https_url() {
        let spec = RepoSpec::parse("o/6").unwrap();
        assert_eq!(spec.clone_url, "https://github.com/o/6.git");
    }
}
