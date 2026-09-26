//! The output of a real run must not contain a credential.
//!
//! Unit tests can assert that `SecretString`'s `Debug` renders `***`, and
//! that is necessary but not sufficient: it says nothing about whether the
//! token reached a log line by some other route — a `tracing` field, an error
//! that embeds it, a panic message, a `format!` that reached for `Display`.
//!
//! So this drives the actual binary with a PAT-shaped value in the
//! environment, captures everything it writes, and scans the result for
//! anything token-shaped. The assertion is against the *pattern*, not against
//! one known string, so it keeps working when the token changes.
//!
//! What this cannot cover, stated plainly. Two things.
//!
//! First: **today nothing in the tree Debug-formats a token**, so this file is
//! a guard on the *future*, not a witness to the present. It was verified to
//! be capable of failing — see the negative control below — but injecting a
//! leak into `AuthToken`'s `Debug` does NOT turn it red, because no command
//! reaches that code. The check with teeth for the present is
//! `ro-github`'s `debug_of_a_token_contains_no_pat`, which asserts on `Debug`
//! directly and does fail when the leak is reintroduced. Both exist: this one
//! catches a command that starts printing, that one catches the type.
//!
//! Second: a secret handed to a subprocess can reach that process's own
//! stdout, and an agent's transcript is designed to be read and shared.
//! `SecretString` protects ro's own output. The other half of the defence is
//! never putting the secret where the agent can see it.

use assert_cmd::Command;
use tempfile::TempDir;

/// A value shaped like a real GitHub token. Not a live credential.
const FAKE_PAT: &str = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";
const FAKE_FINE_GRAINED: &str = "github_pat_11ABCDEFG0aBcDeFgHiJkLmNoPqRsT";

/// True if any whitespace-or-punctuation-delimited token in `haystack` looks
/// like a PAT.
///
/// Splitting on non-alphanumerics rather than running one regex is deliberate:
/// a redaction that leaves the tail visible still trips this, which is the
/// point, and the split keeps a partially-masked value from hiding behind a
/// prefix character.
fn contains_pat_shaped_token(haystack: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|token| {
            (token.starts_with("ghp_") || token.starts_with("github_pat_"))
                && token.len() >= 20
                && token.chars().any(|c| c.is_ascii_digit())
        })
}

fn assert_no_credential(label: &str, output: &str) {
    assert!(
        !contains_pat_shaped_token(output),
        "{label} leaked a PAT-shaped token.\n--- output ---\n{output}\n---------------"
    );
}

/// Every command that runs without needing the network or a real repository,
/// driven with a credential present in the environment.
fn run_everything(config_dir: &TempDir, state_dir: &TempDir) -> Vec<(String, String)> {
    let invocations: &[&[&str]] = &[
        &["init"],
        &["list"],
        &["status"],
        &["doctor"],
        &["schema"],
        &["config", "show"],
        &["sweep", "commit-sweep", "--all"],
        &["prune", "--dry-run"],
    ];

    invocations
        .iter()
        .map(|args| {
            let mut cmd = Command::cargo_bin("ro").expect("ro binary should compile");
            cmd.arg("--config-dir")
                .arg(config_dir.path())
                .arg("--state-dir")
                .arg(state_dir.path())
                .args(*args)
                // Both names, because the precedence between them is itself
                // under test elsewhere and either could be the one that leaks.
                .env("GH_TOKEN", FAKE_PAT)
                .env("GITHUB_TOKEN", FAKE_PAT)
                .env("CI_GH_TOKEN", FAKE_FINE_GRAINED);
            let out = cmd.output().expect("the command should run");
            let label = args.join(" ");
            let combined = format!(
                "{}{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                // The exit code is rendered by the process, not by us, but a
                // panic message reaches stderr and is the most likely leak of
                // all — a Debug-formatted panic.
                String::from_utf8_lossy(&out.stdout)
            );
            (label, combined)
        })
        .collect()
}

#[test]
fn no_command_prints_a_credential() {
    let config_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();

    let outputs = run_everything(&config_dir, &state_dir);
    assert!(
        outputs.len() >= 8,
        "the sweep should have exercised every command, ran {}",
        outputs.len()
    );
    for (label, output) in &outputs {
        assert_no_credential(label, output);
    }
}

/// The same check with a fine-grained PAT shape, because the two have
/// different prefixes and a guard that only knew one would pass the other.
#[test]
fn no_command_prints_a_fine_grained_pat() {
    let config_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("ro").expect("ro binary should compile");
    cmd.arg("--config-dir")
        .arg(config_dir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .arg("doctor")
        .env("GH_TOKEN", FAKE_FINE_GRAINED)
        .env("GITHUB_TOKEN", FAKE_FINE_GRAINED);
    let out = cmd.output().expect("doctor should run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_no_credential("doctor with a fine-grained PAT", &combined);
}

/// A negative control, so a vacuous pass is impossible. If the scanner cannot
/// see a PAT it claims to look for, every assertion above is worthless.
#[test]
fn the_scanner_would_catch_a_leak() {
    assert!(
        contains_pat_shaped_token(&format!("token: {FAKE_PAT}")),
        "the scanner must see an unredacted PAT"
    );
    assert!(
        contains_pat_shaped_token("github_pat_11ABCDEFG0aBcDeFgHiJkLmNoPqRsT"),
        "the scanner must see a fine-grained PAT"
    );
    assert!(
        !contains_pat_shaped_token("ghp_short"),
        "a short prefix is not a PAT and must not trip the scanner"
    );
    assert!(
        !contains_pat_shaped_token("***"),
        "the redaction itself must not trip the scanner"
    );
}
