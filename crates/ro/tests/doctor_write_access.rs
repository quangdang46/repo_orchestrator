//! `ro doctor` must say which repository will fail a push — before the run.
//!
//! The scenario the plan describes: `quangdang46` authenticated,
//! `quangdang46/repo_orchestrator` as the remote, `push: false`, discovered
//! only at the push step as a 403 *after four other repos had already been
//! pushed*. What prevents that is a check that runs first and names the repo.
//!
//! So these tests drive the real binary against a real registry, with a fake
//! GitHub behind it, and assert on the three properties that matter:
//!
//!   1. a repo the credential cannot write is named and reported `write: NO`
//!   2. the healthy rows are still printed alongside it
//!   3. the exit code is non-zero
//!
//! And on the property the design turns on: the probe READS, so the fake
//! remote records zero writes across the whole run.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A PAT-shaped value that is not a real credential.
const FAKE_TOKEN: &str = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";

/// What the fake answers for one repository.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Push,
    Maintain,
    ReadOnly,
    Hidden,
    Reject,
}

impl Answer {
    fn label(self) -> &'static str {
        match self {
            Answer::Push => "write: yes",
            Answer::Maintain => "write: yes",
            Answer::ReadOnly | Answer::Hidden => "write: NO ",
            Answer::Reject => "write: ?? ",
        }
    }
}

struct Fake {
    base_uri: String,
    writes: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

/// A GitHub that serves the two GETs the probe makes and records every request,
/// counting the ones that are not GETs.
impl Fake {
    fn start(login: &str, repos: &[(&str, Answer)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().expect("a bound address");
        let writes = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));

        let worker_writes = Arc::clone(&writes);
        let worker_requests = Arc::clone(&requests);
        let login = login.to_string();
        let table: Vec<(String, Answer)> =
            repos.iter().map(|(k, v)| ((*k).to_string(), *v)).collect();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let w = Arc::clone(&worker_writes);
                let r = Arc::clone(&worker_requests);
                let login = login.clone();
                let table = table.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, w, r, &login, &table);
                });
            }
        });

        Self {
            base_uri: format!("http://{addr}"),
            writes,
            requests,
        }
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.requests
            .lock()
            .expect("the log is not poisoned")
            .clone()
    }
}

fn serve(
    mut stream: TcpStream,
    writes: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    login: &str,
    table: &[(String, Answer)],
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        if h.trim().is_empty() {
            break;
        }
    }

    if !method.eq_ignore_ascii_case("GET") {
        writes.fetch_add(1, Ordering::SeqCst);
    }
    requests
        .lock()
        .expect("the log is not poisoned")
        .push((method.clone(), path.clone()));

    let (status, body) = if !method.eq_ignore_ascii_case("GET") {
        (
            "405 Method Not Allowed",
            r#"{"message":"the probe must not write"}"#.to_string(),
        )
    } else {
        match path.as_str() {
            "/user" => ("200 OK", format!(r#"{{"login":"{login}"}}"#)),
            p if p.starts_with("/repos/") => {
                let key = p.trim_start_matches("/repos/");
                match table.iter().find(|(k, _)| k == key).map(|(_, a)| *a) {
                    None => ("404 Not Found", r#"{"message":"Not Found"}"#.to_string()),
                    Some(Answer::Push) => (
                        "200 OK",
                        r#"{"permissions":{"push":true,"maintain":false,"admin":false,"triage":false,"pull":true}}"#.to_string(),
                    ),
                    Some(Answer::Maintain) => (
                        "200 OK",
                        r#"{"permissions":{"push":false,"maintain":true,"admin":false,"triage":false,"pull":true}}"#.to_string(),
                    ),
                    Some(Answer::ReadOnly) => (
                        "200 OK",
                        r#"{"permissions":{"push":false,"maintain":false,"admin":false,"triage":false,"pull":true}}"#.to_string(),
                    ),
                    Some(Answer::Hidden) => (
                        "404 Not Found",
                        r#"{"message":"Not Found"}"#.to_string(),
                    ),
                    Some(Answer::Reject) => (
                        "401 Unauthorized",
                        r#"{"message":"Bad credentials"}"#.to_string(),
                    ),
                }
            }
            _ => ("404 Not Found", r#"{"message":"Not Found"}"#.to_string()),
        }
    };

    let reason = status.split(' ').next().unwrap_or("");
    let reason = match reason {
        "200" => "OK",
        "401" => "Unauthorized",
        "403" => "Forbidden",
        "404" => "Not Found",
        "405" => "Method Not Allowed",
        _ => "Error",
    };
    stream.write_all(
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body.len()
        )
        .as_bytes(),
    )?;
    stream.flush()
}

/// Build a state database with these repos tracked, and return the config/state
/// directories.
///
/// Each repo is a real local checkout, adopted with `ro add <path>` rather than
/// cloned. Cloning would need the network, and worse, it would have to succeed
/// before the row exists — so a test about a *row's* remote permission would
/// also be a test about reaching github.com.
fn registry_with(repos: &[&str]) -> (TempDir, TempDir) {
    let config_dir = TempDir::new().expect("a config dir");
    let state_dir = TempDir::new().expect("a state dir");

    let ro_cmd = || {
        let mut c = StdCommand::new(assert_cmd::cargo::cargo_bin("ro"));
        c.arg("--config-dir")
            .arg(config_dir.path())
            .arg("--state-dir")
            .arg(state_dir.path());
        c
    };
    ro_cmd().arg("init").output().expect("ro init runs");

    for spec in repos {
        // owner/name, laid out as <owner>/<name> so `ro add <path>` derives the
        // same owner and name a clone would have produced.
        let (owner, name) = spec.split_once('/').expect("specs are owner/name");
        let checkout = state_dir.path().join("checkouts").join(owner).join(name);
        std::fs::create_dir_all(&checkout).expect("the checkout directory is creatable");

        let mut init = StdCommand::new("git");
        init.args(["init", "-q"]).current_dir(&checkout);
        assert!(
            init.status().expect("the child process runs").success(),
            "git init failed"
        );

        for (key, value) in [("user.email", "test@example.com"), ("user.name", "Test")] {
            let mut cfg = StdCommand::new("git");
            cfg.args(["config", key, value]).current_dir(&checkout);
            assert!(
                cfg.status().expect("the child process runs").success(),
                "git config {key} failed"
            );
        }

        std::fs::write(checkout.join("README.md"), "# fixture\n").expect("a file to commit");

        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "initial"]] {
            let mut c = StdCommand::new("git");
            c.args(&args).current_dir(&checkout);
            assert!(
                c.status().expect("the child process runs").success(),
                "git {args:?} failed"
            );
        }

        let out = ro_cmd()
            .arg("add")
            .arg(&checkout)
            .output()
            .expect("ro add runs");
        assert!(
            out.status.success(),
            "ro add {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    (config_dir, state_dir)
}

/// Run `ro doctor` with the fake behind it, returning the combined output and
/// the exit code.
fn doctor_against(fake: &Fake, config_dir: &TempDir, state_dir: &TempDir) -> (String, i32) {
    // The binary reads its base URI from the GitHub host, so the fake is
    // injected by pointing GITHUB_API_URL at it — which is the only seam that
    // does not require a production-only code path.
    let out = StdCommand::new(assert_cmd::cargo::cargo_bin("ro"))
        .arg("--config-dir")
        .arg(config_dir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .arg("doctor")
        .env("GH_TOKEN", FAKE_TOKEN)
        .env("GITHUB_API_URL", &fake.base_uri)
        .output()
        .expect("ro doctor runs");
    (
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn a_read_only_repo_is_named_and_the_healthy_rows_still_print() {
    let fake = Fake::start(
        "quangdang46",
        &[
            ("acme/alpha", Answer::Push),
            ("acme/beta", Answer::ReadOnly),
            ("acme/gamma", Answer::Maintain),
        ],
    );
    let (config_dir, state_dir) = registry_with(&["acme/alpha", "acme/beta", "acme/gamma"]);

    let (output, code) = doctor_against(&fake, &config_dir, &state_dir);

    // The one that will fail is named, with the reason.
    assert!(
        output.contains("acme/beta"),
        "the failing repo must be named, got:\n{output}"
    );
    assert!(
        output.contains(Answer::ReadOnly.label()),
        "expected 'write: NO' for acme/beta, got:\n{output}"
    );
    // The healthy rows are still there. A doctor that stops at the first bad
    // row tells the user one thing when there are three.
    for good in ["acme/alpha", "acme/gamma"] {
        assert!(
            output.contains(good),
            "the healthy repo {good} must still be reported, got:\n{output}"
        );
    }
    assert_eq!(
        code, 1,
        "a repo that cannot be written must exit non-zero, got:\n{output}"
    );

    // The property the design turns on.
    assert_eq!(
        fake.writes(),
        0,
        "doctor must READ the permission, never exercise it. requests: {:?}",
        fake.requests()
    );
    assert!(
        fake.requests().iter().all(|(m, _)| m == "GET"),
        "every request must be a GET, saw: {:?}",
        fake.requests()
    );
}

#[test]
fn an_all_healthy_fleet_reports_yes_and_exits_zero() {
    let fake = Fake::start(
        "quangdang46",
        &[
            ("acme/alpha", Answer::Push),
            ("acme/beta", Answer::Maintain),
        ],
    );
    let (config_dir, state_dir) = registry_with(&["acme/alpha", "acme/beta"]);

    let (output, code) = doctor_against(&fake, &config_dir, &state_dir);

    assert!(
        output.contains(Answer::Push.label()),
        "expected 'write: yes' for a writable repo, got:\n{output}"
    );
    assert_eq!(
        code, 0,
        "a fully healthy fleet must exit zero, got:\n{output}"
    );
    assert!(
        !output.contains(Answer::ReadOnly.label()),
        "a healthy fleet must not report a NO, got:\n{output}"
    );
    assert_eq!(fake.writes(), 0, "still read-only");
}

/// A rejected token and a forbidden repository both make a push fail, and they
/// are fixed by opposite actions. The output must not blur them.
#[test]
fn a_rejected_token_is_reported_differently_from_a_forbidden_repo() {
    let forbidden = Fake::start(
        "quangdang46",
        &[("acme/alpha", Answer::Push), ("acme/beta", Answer::Hidden)],
    );
    let (c1, s1) = registry_with(&["acme/alpha", "acme/beta"]);
    let (hidden_output, _) = doctor_against(&forbidden, &c1, &s1);

    let rejected = Fake::start("nobody", &[("acme/alpha", Answer::Reject)]);
    let (c2, s2) = registry_with(&["acme/alpha"]);
    let (rejected_output, _) = doctor_against(&rejected, &c2, &s2);

    assert!(
        !hidden_output.contains("credential problem"),
        "a hidden repository is a permission problem, not a credential one:\n{hidden_output}"
    );
    assert!(
        rejected_output.contains("credential problem"),
        "a rejected token is a credential problem and must say so:\n{rejected_output}"
    );
    assert_eq!(forbidden.writes(), 0);
    assert_eq!(rejected.writes(), 0);
}
