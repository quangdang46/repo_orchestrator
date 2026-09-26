//! A fake GitHub API, for testing code that must never write.
//!
//! The requirement the permission probe has to satisfy is *read, never
//! exercise*. Asserting that a real GitHub was not written to is impossible —
//! and asserting it against a real GitHub would itself be a write. So the tests
//! point the probe at this server, which serves the two GETs the probe makes
//! and **records every request it receives**.
//!
//! The assertion that matters is therefore [`FakeGitHub::writes`], and the
//! test that carries it is the one that would fail if someone "fixed" the
//! probe by trying a push instead of a read.

#![cfg(test)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
}

/// What the fake should answer with for one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoAnswer {
    /// 200 with these permissions.
    Permissions {
        push: bool,
        maintain: bool,
        admin: bool,
    },
    /// 404 — how GitHub hides a repository from a non-collaborator.
    NotFound,
    /// 403 — a collaborator whose role cannot write.
    Forbidden,
    /// 401 — the token itself is bad.
    Unauthorised,
}

struct Inner {
    login: String,
    repos: BTreeMap<String, RepoAnswer>,
    requests: Mutex<Vec<Recorded>>,
    writes: AtomicUsize,
}

pub struct FakeGitHub {
    base_uri: String,
    inner: Arc<Inner>,
}

impl FakeGitHub {
    /// Start a server on an ephemeral loopback port.
    ///
    /// `login` is what `/user` reports; `repos` maps `owner/name` to the answer.
    pub fn start(login: &str, repos: &[(&str, RepoAnswer)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback port should be available");
        let addr = listener.local_addr().expect("bound socket has an address");
        let inner = Arc::new(Inner {
            login: login.to_string(),
            repos: repos.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
            requests: Mutex::new(Vec::new()),
            writes: AtomicUsize::new(0),
        });
        let worker = Arc::clone(&inner);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let inner = Arc::clone(&worker);
                // One thread per connection: the client may open more than one,
                // and a single-threaded loop would serialise them in a way the
                // real client never does.
                std::thread::spawn(move || {
                    let _ = handle(stream, inner);
                });
            }
        });

        Self {
            base_uri: format!("http://{addr}"),
            inner,
        }
    }

    pub fn base_uri(&self) -> &str {
        &self.base_uri
    }

    /// Every request the probe made, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.inner
            .requests
            .lock()
            .expect("request log is not poisoned")
            .clone()
    }

    /// How many non-GET requests arrived.
    ///
    /// This is the number the whole design turns on. A probe that proved write
    /// access by pushing would make this non-zero, and the test would fail.
    pub fn writes(&self) -> usize {
        self.inner.writes.load(Ordering::SeqCst)
    }

    /// A token the fake accepts. Nothing here validates it — the point is to
    /// drive the code path, not to test authentication.
    pub fn token(&self) -> crate::auth::AuthToken {
        crate::auth::AuthToken::new("ghp_fake_token_for_tests_only_000")
    }
}

fn handle(mut stream: TcpStream, inner: Arc<Inner>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    // Drain the headers so the client sees a complete exchange.
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
    }

    if !method.eq_ignore_ascii_case("GET") {
        inner.writes.fetch_add(1, Ordering::SeqCst);
    }
    inner
        .requests
        .lock()
        .expect("request log is not poisoned")
        .push(Recorded {
            method: method.clone(),
            path: path.clone(),
        });

    let (status, body) = respond(&inner, &method, &path);
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        reason = reason_phrase(status),
        len = body.len(),
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

fn respond(inner: &Inner, method: &str, path: &str) -> (&'static str, String) {
    // Any non-GET is answered with a refusal, so a probe that "verifies" by
    // writing gets a hard failure rather than accidentally succeeding.
    if !method.eq_ignore_ascii_case("GET") {
        return (
            "405 Method Not Allowed",
            r#"{"message":"the probe must not write"}"#.into(),
        );
    }

    match path {
        "/user" => (
            "200 OK",
            format!(r#"{{"login":{}}}"#, json_string(&inner.login)),
        ),
        p if p.starts_with("/repos/") => {
            let key = p.trim_start_matches("/repos/");
            match inner.repos.get(key) {
                None => ("404 Not Found", r#"{"message":"Not Found"}"#.to_string()),
                Some(RepoAnswer::NotFound) => {
                    ("404 Not Found", r#"{"message":"Not Found"}"#.to_string())
                }
                Some(RepoAnswer::Forbidden) => {
                    ("403 Forbidden", r#"{"message":"Forbidden"}"#.to_string())
                }
                Some(RepoAnswer::Unauthorised) => (
                    "401 Unauthorized",
                    r#"{"message":"Bad credentials"}"#.to_string(),
                ),
                Some(RepoAnswer::Permissions {
                    push,
                    maintain,
                    admin,
                }) => (
                    "200 OK",
                    format!(
                        r#"{{"full_name":{},"permissions":{{"push":{push},"maintain":{maintain},"admin":{admin},"triage":false,"pull":true}}}}"#,
                        json_string(key),
                    ),
                ),
            }
        }
        _ => ("404 Not Found", r#"{"message":"Not Found"}"#.to_string()),
    }
}

fn reason_phrase(status: &str) -> &'static str {
    match status.split(' ').next().unwrap_or("") {
        "200" => "OK",
        "401" => "Unauthorized",
        "403" => "Forbidden",
        "404" => "Not Found",
        "405" => "Method Not Allowed",
        _ => "Error",
    }
}

/// Minimal JSON string escaping — enough for a login, and it avoids pulling a
/// serialisation dependency into a test helper.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Is the instrument measuring anything?
    ///
    /// Every permission test asserts `writes() == 0`. If the counter never
    /// increments, all of those assertions are true for a reason that has
    /// nothing to do with the probe being read-only — which is exactly the
    /// failure mode a guard against a vacuous pass has. So the counter is
    /// driven with a real non-GET first and must notice.
    #[test]
    fn the_write_counter_notices_a_real_write() {
        let server = FakeGitHub::start("me", &[("acme/api", RepoAnswer::Forbidden)]);
        assert_eq!(server.writes(), 0, "nothing sent yet");

        let mut stream = TcpStream::connect(
            server
                .base_uri()
                .trim_start_matches("http://")
                .parse::<std::net::SocketAddr>()
                .expect("base uri carries a host:port"),
        )
        .expect("the fake should accept a connection");
        let body = r#"{"name":"probe"}"#;
        stream
            .write_all(
                format!(
                    "POST /repos/acme/api/keys HTTP/1.1\r\n\
                     Host: localhost\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("the request should be sent");
        stream.flush().ok();

        let mut response = String::new();
        let mut reader = BufReader::new(&mut stream);
        let _ = reader.read_to_string(&mut response);

        assert_eq!(
            server.writes(),
            1,
            "the counter must see a POST, or 'zero writes' proves nothing. \
             response was: {response}"
        );
        // And the request is still recorded, so a test can name what was sent.
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r.method == "POST" && r.path == "/repos/acme/api/keys"),
            "requests seen: {:?}",
            server.requests()
        );
    }

    #[test]
    fn a_get_is_not_counted_as_a_write() {
        let server = FakeGitHub::start("me", &[("acme/api", RepoAnswer::Forbidden)]);
        let _ = crate::permissions::probe_write_access(
            &server.token(),
            "acme",
            "api",
            Some(server.base_uri()),
        );
        assert_eq!(server.writes(), 0);
        assert!(
            server.requests().iter().any(|r| r.path == "/user"),
            "the probe should have asked who it is: {:?}",
            server.requests()
        );
    }
}
