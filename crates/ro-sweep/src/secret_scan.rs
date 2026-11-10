//! Secret scanning.
//!
//! Modes: off, warn, block (default: block).
//! Scan staged files and generated patches before commit.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::Read;
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
/// Redact a matched secret to a short preview.
///
/// Delegates to the one rule in `ro-core::secret`. This was a *second* rule
/// (first-4 only) and the tree now has exactly one, because the identity guard
/// compares redacted forms and a guard that redacts differently on each side
/// of a comparison is not a guard.
fn redact(s: &str) -> String {
    ro_core::redact(s)
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

/// Files larger than this are skipped rather than read into memory whole:
/// a multi-GB worktree file would otherwise turn a preflight scan into an
/// OOM. The skip is loud — a file the scan never saw must not read as
/// "scanned clean".
pub const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// Scan a file on disk. Binary-looking content is skipped silently —
/// **except** UTF-16/UTF-32 text, which is ~50% NUL bytes by construction
/// and would otherwise bypass the scan entirely: a `ghp_` token in a
/// UTF-16 file is a credential the moment it is committed, and the NUL
/// density test cannot tell it from a PNG.
pub fn scan_file(path: &Path) -> Result<Vec<SecretFinding>> {
    if let Ok(meta) = path.metadata() {
        if meta.len() > MAX_SCAN_BYTES {
            warn_skipped(path, meta.len());
            return Ok(Vec::new());
        }
    }
    // `take` bounds the read even when metadata lies (or is unavailable),
    // so a pathological file can never allocate more than the cap.
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_SCAN_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SCAN_BYTES {
        warn_skipped(path, bytes.len() as u64);
        return Ok(Vec::new());
    }
    let Some(text) = text_for_scan(&bytes) else {
        return Ok(Vec::new());
    };
    let label = path.to_string_lossy().to_string();
    Ok(scan_text(&label, &text))
}

fn warn_skipped(path: &Path, size: u64) {
    let msg = format!(
        "secret scan skipped {}: {size} bytes exceeds the {MAX_SCAN_BYTES}-byte cap",
        path.display()
    );
    tracing::warn!("{msg}");
    eprintln!("warning: {msg}");
}

/// The text a file's bytes should be scanned as, or `None` when the file
/// is genuinely binary and there is nothing to scan.
///
/// Order matters. A BOM is checked **first**: UTF-32LE starts with the same
/// two bytes as UTF-16LE (`FF FE`), so the longer signature has to win or
/// every UTF-32 file decodes as mojibake. A BOM-less UTF-16 file is caught
/// by the NUL-*parity* test inside [`looks_binary`]'s caller — ASCII text
/// encoded as UTF-16 puts a NUL on exactly one byte of every pair, so the
/// NULs are not spread through the file the way a binary's are, they sit
/// on a lattice. Decoding that lattice back to UTF-8 is what keeps the
/// scan honest on files Notepad and PowerShell produce without a BOM.
fn text_for_scan(bytes: &[u8]) -> Option<String> {
    // UTF-32 signatures before UTF-16: `FF FE 00 00` begins with the
    // UTF-16LE BOM.
    if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        return Some(decode_utf32(bytes, Endian::Little, 4));
    }
    if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        return Some(decode_utf32(bytes, Endian::Big, 4));
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(decode_utf16(bytes, Endian::Little, 2));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(decode_utf16(bytes, Endian::Big, 2));
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        // A UTF-8 BOM is not an encoding change; strip it and read on.
        return Some(String::from_utf8_lossy(&bytes[3..]).into_owned());
    }
    if looks_binary(bytes) {
        // No BOM. A BOM-less UTF-16 file still has its NULs on a parity
        // lattice; anything else really is binary.
        return bomless_utf16(bytes).map(|endian| decode_utf16(bytes, endian, 0));
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

/// Decode `bytes` as UTF-16, skipping `skip` leading bytes (the BOM).
/// Unpaired surrogates become U+FFFD rather than aborting the scan.
fn decode_utf16(bytes: &[u8], endian: Endian, skip: usize) -> String {
    let body = &bytes[skip..];
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|c| match endian {
            Endian::Little => u16::from_le_bytes([c[0], c[1]]),
            Endian::Big => u16::from_be_bytes([c[0], c[1]]),
        })
        .collect();
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Decode `bytes` as UTF-32. There is no `char::decode_utf32` in std, so
/// each 4-byte unit goes through `char::from_u32`; an unpaired surrogate
/// or out-of-range value becomes U+FFFD rather than aborting the scan —
/// a partially-decoded file still scans the part that decoded.
fn decode_utf32(bytes: &[u8], endian: Endian, skip: usize) -> String {
    bytes[skip..]
        .chunks_exact(4)
        .map(|c| {
            let v = match endian {
                Endian::Little => u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                Endian::Big => u32::from_be_bytes([c[0], c[1], c[2], c[3]]),
            };
            char::from_u32(v).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect()
}

/// Is this BOM-less content UTF-16 text? Returns the endianness when the
/// NUL bytes sit on one parity of the byte stream — the signature of
/// 16-bit text — and `None` for a genuinely binary blob.
///
/// UTF-16LE ASCII is `[char, 0x00]` pairs, so every NUL is at an **odd**
/// offset; UTF-16BE puts them at **even** offsets. A binary scatters its
/// NULs across both parities. The test needs enough pairs to be sure and
/// enough skew that coincidence cannot explain it.
fn bomless_utf16(bytes: &[u8]) -> Option<Endian> {
    let head = &bytes[..bytes.len().min(8000)];
    let pairs = head.len() / 2;
    if pairs < 64 {
        return None;
    }
    let mut nul_even = 0usize;
    let mut nul_odd = 0usize;
    for i in 0..pairs {
        if head[2 * i] == 0 {
            nul_even += 1;
        }
        if head[2 * i + 1] == 0 {
            nul_odd += 1;
        }
    }
    let total = nul_even + nul_odd;
    // Fewer than ~10% NULs is not the UTF-16 signature at all.
    if total * 10 <= head.len() {
        return None;
    }
    // One parity must hold essentially all of them. 85% leaves room for
    // the occasional NUL inside a multi-byte character while still
    // rejecting a binary, whose NULs split roughly evenly.
    let dominant = nul_even.max(nul_odd);
    if dominant * 20 < total * 17 {
        return None;
    }
    // The *other* byte of each pair should mostly be non-NUL text. If both
    // parities are NUL-dense the file is zeros, not text.
    let sparse = nul_even.min(nul_odd);
    if sparse * 4 > pairs {
        return None;
    }
    // NULs on odd offsets means the high byte is zero → little-endian.
    Some(if nul_odd > nul_even {
        Endian::Little
    } else {
        Endian::Big
    })
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

/// Decide whether a file is binary enough that scanning its text is
/// pointless.
///
/// A **density** test, not a presence test. The old version returned true if
/// *any* of the first 8000 bytes was zero, so a single `0x00` at offset 0
/// switched the scan off for the whole file — and a real-looking text file
/// carrying a `ghp_`-shaped token committed and pushed as a clean run. That
/// is exactly the class this scan exists to catch, so it was a clean bypass
/// of the safety net.
///
/// A stray NUL inside a text file is rare; a binary file is *mostly* NULs and
/// other non-text bytes. Requiring a meaningful proportion keeps genuinely
/// binary blobs skipped (the `scan_file_skips_binary` fixture is 2 NULs in 6
/// bytes) while a text file with one odd byte is still scanned. A NUL inside
/// a credential is not something a real secret has, so matching the
/// surrounding shape still works over the lossy-UTF8 text.
fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8000)];
    if head.is_empty() {
        return false;
    }
    let nul = head.iter().filter(|&&b| b == 0).count();
    // More than 5% NUL bytes is a binary file; a stray byte in text is not.
    nul * 20 > head.len()
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

    /// A NUL byte anywhere in the first 8000 bytes must not switch the scan
    /// off for the whole file. `looks_binary` used to return true on a
    /// single zero byte, so one `0x00` at offset 0 let a real-looking text
    /// file carrying a `ghp_`-shaped token commit and push as a clean run.
    #[test]
    fn a_nul_byte_does_not_disable_the_scan() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("notes.bin");
        let mut bytes: Vec<u8> = vec![0u8];
        bytes.extend_from_slice(b"AAAAAAAAAAAAAAAAAAAA\n");
        // A full-length ghp_ token (36+ chars after the prefix) so the
        // github_token rule — the most specific one — is what must fire.
        bytes.extend_from_slice(b"token = \"ghp_ZZZsecretZZZ1234567890ABCDEFGHIJKLMNOP\"\n");
        std::fs::write(&p, &bytes).unwrap();

        let f = scan_file(&p).unwrap();
        assert!(
            f.iter().any(|x| x.rule == "github_token"),
            "a NUL byte must not disable the scan; findings were: {f:?}"
        );
    }

    /// Encode `s` as UTF-16LE/BE bytes, with or without a BOM.
    fn utf16_bytes(s: &str, endian: Endian, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if bom {
            match endian {
                Endian::Little => out.extend_from_slice(&[0xFF, 0xFE]),
                Endian::Big => out.extend_from_slice(&[0xFE, 0xFF]),
            }
        }
        for u in s.encode_utf16() {
            match endian {
                Endian::Little => out.extend_from_slice(&u.to_le_bytes()),
                Endian::Big => out.extend_from_slice(&u.to_be_bytes()),
            }
        }
        out
    }

    /// The bypass this fix exists for: UTF-16 text is ~50% NUL bytes, the
    /// binary heuristic skipped it whole, and a `ghp_` token inside
    /// committed and pushed clean. Both BOM and BOM-less spellings, both
    /// endiannesses.
    #[test]
    fn a_secret_in_utf16_is_found() {
        let tmp = TempDir::new().unwrap();
        let token = "token = \"ghp_ZZZsecretZZZ1234567890ABCDEFGHIJKLMNOP\"";
        for (name, endian, bom) in [
            ("le_bom", Endian::Little, true),
            ("le_nobom", Endian::Little, false),
            ("be_bom", Endian::Big, true),
            ("be_nobom", Endian::Big, false),
        ] {
            let p = tmp.path().join(format!("creds_{name}.txt"));
            // Pad past the 64-pair minimum the BOM-less heuristic needs.
            let body = format!("{token}\n{}", "x".repeat(400));
            std::fs::write(&p, utf16_bytes(&body, endian, bom)).unwrap();
            let f = scan_file(&p).unwrap();
            assert!(
                f.iter().any(|x| x.rule == "github_token"),
                "{name}: a ghp_ token in UTF-16 must be found, got {f:?}"
            );
        }
    }

    /// UTF-32LE's BOM begins with the UTF-16LE BOM's two bytes; the longer
    /// signature has to win or the file decodes as mojibake.
    #[test]
    fn a_secret_in_utf32le_is_found() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("creds32.txt");
        let body = format!(
            "token = \"ghp_ZZZsecretZZZ1234567890ABCDEFGHIJKLMNOP\"\n{}",
            "x".repeat(400)
        );
        let mut bytes = vec![0xFF, 0xFE, 0x00, 0x00];
        for c in body.chars() {
            bytes.extend_from_slice(&(c as u32).to_le_bytes());
        }
        std::fs::write(&p, &bytes).unwrap();
        let f = scan_file(&p).unwrap();
        assert!(
            f.iter().any(|x| x.rule == "github_token"),
            "a ghp_ token in UTF-32LE must be found, got {f:?}"
        );
    }

    /// The negative control: a real binary is still skipped. NULs spread
    /// across both parities, which is exactly what the UTF-16 lattice test
    /// rejects.
    #[test]
    fn a_real_binary_is_still_skipped() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("blob.bin");
        // Deterministic bytes, NUL-dense on **both** parities — the shape a
        // real binary has and the UTF-16 lattice test exists to reject.
        // (Uniformly random bytes are only ~0.4% NUL and never reach the
        // binary branch at all.)
        let mut bytes = Vec::new();
        let mut x = 0x1234_5678u32;
        for i in 0..2048usize {
            if i % 5 < 2 {
                bytes.extend_from_slice(&[0, 0]);
            } else {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let b = x.to_le_bytes();
                bytes.extend_from_slice(&[b[0] | 1, b[1] | 1]);
            }
        }
        std::fs::write(&p, &bytes).unwrap();
        let f = scan_file(&p).unwrap();
        assert!(
            f.is_empty(),
            "a binary blob must not produce findings: {f:?}"
        );
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
