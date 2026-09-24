//! Secret scanning.
//!
//! Modes: off, warn, block (default: block).
//! Scan staged files and generated patches before commit.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;

/// Secret scan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretScanMode {
    /// Disable scanning entirely.
    Off,
    /// Report findings but allow commits to proceed.
    Warn,
    /// Block any commit/apply when secrets are found.
    Block,
}

#[allow(clippy::derivable_impls)]
impl Default for SecretScanMode {
    fn default() -> Self {
        Self::Block
    }
}

impl std::fmt::Display for SecretScanMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Block => "block",
        })
    }
}

impl std::str::FromStr for SecretScanMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "warn" => Ok(Self::Warn),
            "block" => Ok(Self::Block),
            other => Err(anyhow::anyhow!(
                "invalid secret_scan mode: {other} (expected off|warn|block)"
            )),
        }
    }
}

/// A single secret match found in a file or patch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretFinding {
    /// Path of the file (or patch identifier) where the secret was found.
    pub path: String,
    /// Line number (1-indexed). 0 means unknown.
    pub line: usize,
    /// Rule name that fired (e.g. "aws_access_key", "github_token").
    pub rule: String,
    /// Redacted preview of the matched substring.
    pub redacted: String,
}

impl SecretFinding {
    /// Build a finding with the matched text already redacted.
    pub fn new(path: impl Into<String>, line: usize, rule: impl Into<String>, raw: &str) -> Self {
        Self {
            path: path.into(),
            line,
            rule: rule.into(),
            redacted: redact(raw),
        }
    }
}

/// Redact a secret to a short preview.
fn redact(s: &str) -> String {
    if s.len() <= 4 {
        "****".to_string()
    } else {
        let head: String = s.chars().take(4).collect();
        format!("{head}…")
    }
}

struct Rule {
    name: &'static str,
    pattern: &'static str,
}

const RULES: &[Rule] = &[
    Rule {
        name: "github_token",
        // ghp_/gho_/ghu_/ghs_/ghr_ + 36+ chars
        pattern: r"\bgh[pousr]_[A-Za-z0-9]{36,}\b",
    },
    Rule {
        name: "github_pat",
        // Fine-grained PAT
        pattern: r"\bgithub_pat_[A-Za-z0-9_]{82,}\b",
    },
    Rule {
        name: "aws_access_key",
        pattern: r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    },
    Rule {
        name: "aws_secret_key",
        pattern: r#"(?i)aws[_-]?secret[_-]?(?:access[_-]?)?key["']?\s*[:=]\s*["']?[A-Za-z0-9/+=]{40}["']?"#,
    },
    Rule {
        name: "private_key_block",
        pattern: r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    },
    Rule {
        name: "google_api_key",
        pattern: r"\bAIza[0-9A-Za-z_-]{35}\b",
    },
    Rule {
        name: "slack_token",
        pattern: r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b",
    },
    Rule {
        name: "stripe_live_key",
        pattern: r"\bsk_live_[0-9a-zA-Z]{24,}\b",
    },
    Rule {
        name: "openai_key",
        pattern: r"\bsk-[A-Za-z0-9_-]{40,}\b",
    },
    Rule {
        name: "generic_high_entropy_assignment",
        // password/secret/token/api_key = "..." with at least 16 chars
        pattern: r#"(?i)(?:password|secret|token|api[_-]?key)\s*[:=]\s*["'][A-Za-z0-9+/=_-]{16,}["']"#,
    },
];

fn compiled() -> &'static [(Regex, &'static str)] {
    static CACHE: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        RULES
            .iter()
            .map(|r| (Regex::new(r.pattern).expect("rule regex"), r.name))
            .collect()
    })
}

/// Scan a single text blob for secrets. Returns one finding per match.
pub fn scan_text(path: &str, text: &str) -> Vec<SecretFinding> {
    let mut out = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        for (re, rule) in compiled() {
            if let Some(m) = re.find(line) {
                out.push(SecretFinding::new(path, line_no + 1, *rule, m.as_str()));
            }
        }
    }
    out
}

/// Scan a file on disk. Binary-looking content is skipped silently.
pub fn scan_file(path: &Path) -> Result<Vec<SecretFinding>> {
    let bytes = std::fs::read(path)?;
    if looks_binary(&bytes) {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&bytes);
    let label = path.to_string_lossy().to_string();
    Ok(scan_text(&label, &text))
}

/// Scan multiple files and aggregate findings.
pub fn scan_files(paths: &[&Path]) -> Result<Vec<SecretFinding>> {
    let mut all = Vec::new();
    for p in paths {
        match scan_file(p) {
            Ok(mut f) => all.append(&mut f),
            Err(e) => {
                tracing::warn!("scan_file failed for {}: {e}", p.display());
            }
        }
    }
    Ok(all)
}

/// Decide whether a set of findings should block based on the mode.
pub fn should_block(mode: SecretScanMode, findings: &[SecretFinding]) -> bool {
    matches!(mode, SecretScanMode::Block) && !findings.is_empty()
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tempfile::TempDir;

    #[test]
    fn mode_default_is_block() {
        assert_eq!(SecretScanMode::default(), SecretScanMode::Block);
    }

    #[test]
    fn mode_parses() {
        assert_eq!(
            SecretScanMode::from_str("off").unwrap(),
            SecretScanMode::Off
        );
        assert_eq!(
            SecretScanMode::from_str("WARN").unwrap(),
            SecretScanMode::Warn
        );
        assert_eq!(
            SecretScanMode::from_str("block").unwrap(),
            SecretScanMode::Block
        );
        assert!(SecretScanMode::from_str("nope").is_err());
    }

    #[test]
    fn detects_github_token() {
        let f = scan_text(
            "test",
            "let t = \"ghp_1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZab\";",
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "github_token");
        assert_eq!(f[0].line, 1);
        assert!(f[0].redacted.starts_with("ghp_"));
    }

    #[test]
    fn detects_aws_access_key() {
        let f = scan_text("test", "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "aws_access_key");
    }

    #[test]
    fn detects_private_key_header() {
        let body = "-----BEGIN RSA PRIVATE KEY-----\nblob\n-----END RSA PRIVATE KEY-----";
        let f = scan_text("test", body);
        assert!(f.iter().any(|x| x.rule == "private_key_block"));
    }

    #[test]
    fn detects_generic_assignment() {
        let f = scan_text("test", r#"password = "abcdefghij1234567890""#);
        assert!(
            f.iter()
                .any(|x| x.rule == "generic_high_entropy_assignment")
        );
    }

    #[test]
    fn ignores_clean_code() {
        let f = scan_text("test", "fn main() { println!(\"hello\"); }");
        assert!(f.is_empty());
    }

    #[test]
    fn scan_file_reads_disk() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("leak.txt");
        std::fs::write(&p, "ghp_1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZab\n").unwrap();
        let f = scan_file(&p).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "github_token");
    }

    #[test]
    fn scan_file_skips_binary() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bin");
        std::fs::write(&p, [0u8, 1, 2, 0, 4, 5]).unwrap();
        let f = scan_file(&p).unwrap();
        assert!(f.is_empty());
    }

    #[test]
    fn should_block_respects_mode() {
        let f = vec![SecretFinding::new("a", 1, "x", "raw")];
        assert!(should_block(SecretScanMode::Block, &f));
        assert!(!should_block(SecretScanMode::Warn, &f));
        assert!(!should_block(SecretScanMode::Off, &f));
        assert!(!should_block(SecretScanMode::Block, &[]));
    }
}
