//! Per-repository write-access probe.
//!
//! This is the check that actually prevents a failed run. Verifying that *a*
//! token exists lets a wrong-account situation straight through: a token with
//! `repo` scope and no write access on one repository is an entirely ordinary
//! state, and it is the state this very repository was in when the plan was
//! written — discovered only at the push step, as a 403, after four other repos
//! had already been pushed.
//!
//! ## The probe READS. It never exercises.
//!
//! A doctor that writes to prove it can write is a doctor that mutates state.
//! `GET /repos/{owner}/{repo}` carries a `permissions` object with `push` and
//! the caller's role; that is the whole answer, and the test asserts the fake
//! remote records **zero** non-GET requests.
//!
//! ## Unresolvable and forbidden are different problems
//!
//! They look identical from the outside — the push does not work — and they
//! have completely different fixes. One is "your token is missing or expired",
//! the other is "your token is fine and this repository will still reject you".
//! Collapsing them into a bool sends the user to re-login when re-login cannot
//! possibly help.

use octocrab::Octocrab;
use octocrab::models::Permissions;

use crate::auth::AuthToken;

/// The outcome of asking GitHub what this credential may do to one repository.
///
/// Deliberately not a `bool`. A bool cannot distinguish the two failures, and
/// that distinction is the reason this type exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteAccess {
    /// The credential authenticates and may push.
    Granted {
        /// The login the credential actually resolved to — not the one it was
        /// expected to be. This is the value a user compares against what they
        /// expected, which is why it is reported even when the answer is yes.
        login: String,
    },
    /// The credential authenticates and is refused write access to this
    /// repository. The push will 403.
    Forbidden {
        login: String,
        /// True when GitHub reported the repository as absent to this
        /// credential, which is how it answers "you are not a collaborator".
        /// 403 instead means a collaborator whose role cannot write. The two
        /// need different conversations with a repository admin.
        not_a_collaborator: bool,
    },
    /// The credential could not be used at all: missing, expired, or rejected.
    /// No write-access conclusion is possible, and saying so is more honest
    /// than guessing either way.
    Unresolvable { reason: String },
}

impl WriteAccess {
    pub fn login(&self) -> Option<&str> {
        match self {
            WriteAccess::Granted { login } | WriteAccess::Forbidden { login, .. } => Some(login),
            WriteAccess::Unresolvable { .. } => None,
        }
    }

    pub fn can_push(&self) -> bool {
        matches!(self, WriteAccess::Granted { .. })
    }

    /// One line, shaped so the columns line up down a fleet listing.
    pub fn render(&self) -> String {
        match self {
            WriteAccess::Granted { login } => format!("write: yes  ({login})"),
            WriteAccess::Forbidden { login, .. } => format!("write: NO   ({login})"),
            WriteAccess::Unresolvable { reason } => format!("write: ??   ({reason})"),
        }
    }
}

/// GitHub's roles are cumulative.
///
/// A maintainer or admin can push even when the `push` flag alone would say
/// no. Reporting NO for a maintainer sends someone to fix something that is not
/// broken, which is worse than the failure this check exists to prevent.
fn permissions_allow_push(p: &Permissions) -> bool {
    p.push || p.maintain || p.admin
}

#[derive(Debug, serde::Deserialize)]
struct UserResponse {
    login: String,
}

/// Extract the HTTP status from an octocrab error.
///
/// Matched on the `GitHub` variant rather than stringified, because the
/// classification below turns on the exact code and a substring match on a
/// Display impl is how "403" in an error *message* gets mistaken for a 403.
fn status_of(err: &octocrab::Error) -> Option<u16> {
    match err {
        octocrab::Error::GitHub { source, .. } => Some(source.status_code.as_u16()),
        _ => None,
    }
}

/// Ask GitHub whether `token` may push to `owner/name`.
///
/// Blocking, because its only caller is `ro doctor`, which is synchronous and
/// runs once per remote.
///
/// The client is built **inside** `block_on` on purpose.
/// `Octocrab::builder().build()` panics with "there is no reactor running"
/// outside a runtime — the bug that made `ro import --stars` panic on any
/// machine with a discoverable token, and which passed CI only because `gh`
/// was off `PATH` there.
pub fn probe_write_access(
    token: &AuthToken,
    owner: &str,
    name: &str,
    base_uri: Option<&str>,
) -> WriteAccess {
    if token.as_str().is_empty() {
        return WriteAccess::Unresolvable {
            reason: "no token".to_string(),
        };
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return WriteAccess::Unresolvable {
                reason: format!("could not start a runtime: {e}"),
            };
        }
    };

    rt.block_on(async move {
        let client: anyhow::Result<Octocrab> = match base_uri {
            Some(base) => crate::auth::build_client_with_base_uri(token, base),
            None => crate::auth::build_client(token, None),
        };
        let client = match client {
            Ok(c) => c,
            Err(e) => {
                return WriteAccess::Unresolvable {
                    reason: format!("client could not be built: {e}"),
                };
            }
        };

        // The login first. Without it a Forbidden on the repo is ambiguous
        // between "this account cannot push" and "this token is not valid",
        // and separating those is the entire reason this type exists.
        let login = match client.get::<UserResponse, _, ()>("/user", None).await {
            Ok(user) => user.login,
            Err(e) => {
                return WriteAccess::Unresolvable {
                    reason: match status_of(&e) {
                        Some(code) => format!("token rejected by GitHub (HTTP {code})"),
                        None => format!("could not read the authenticated login: {e}"),
                    },
                };
            }
        };

        // Raw JSON rather than octocrab's typed `Repository`.
        //
        // The probe needs one field. Deserializing the whole model makes the
        // answer depend on every required field octocrab happens to declare
        // this version — a response missing `id` fails to parse and is
        // reported as "could not read", which is a wrong answer rather than a
        // missing one. Asking for a route and reading `permissions` out of it
        // is the smallest thing that works.
        let route = format!("/repos/{owner}/{name}");
        match client.get::<serde_json::Value, _, ()>(&route, None).await {
            Ok(value) => match value.get("permissions") {
                Some(p) => match serde_json::from_value::<Permissions>(p.clone()) {
                    Ok(perms) if permissions_allow_push(&perms) => WriteAccess::Granted { login },
                    Ok(_) => WriteAccess::Forbidden {
                        login,
                        not_a_collaborator: false,
                    },
                    Err(e) => WriteAccess::Unresolvable {
                        reason: format!("could not read the permissions object: {e}"),
                    },
                },
                // A successful read with no `permissions` object. The token
                // could see the repository, which requires *some* access. The
                // failure this check exists to prevent is the 403, not a
                // missing field, so this is not reported as a NO nobody can
                // act on.
                None => WriteAccess::Granted { login },
            },
            Err(e) => match status_of(&e) {
                // 404 is how GitHub hides a repository from a non-collaborator;
                // 403 is a collaborator without write. Same verdict, different
                // conversation to have with the repository's admin.
                Some(404) | Some(403) => WriteAccess::Forbidden {
                    login,
                    not_a_collaborator: status_of(&e) == Some(404),
                },
                Some(401) => WriteAccess::Unresolvable {
                    reason: "token rejected by GitHub (HTTP 401)".to_string(),
                },
                Some(code) => WriteAccess::Unresolvable {
                    reason: format!("GitHub returned HTTP {code} for {owner}/{name}"),
                },
                None => WriteAccess::Unresolvable {
                    reason: format!("could not read {owner}/{name}: {}", first_line(&e)),
                },
            },
        }
    })
}

/// octocrab's `Error` Display embeds a full backtrace, which turns a
/// one-line diagnosis into sixty. `WriteAccess::Unresolvable` carries its
/// reason into a log line and a table, so only the first line is useful.
fn first_line(e: &octocrab::Error) -> String {
    e.to_string()
        .lines()
        .next()
        .unwrap_or("unknown error")
        .to_string()
}

/// Convenience for the common case: probe against the default API base.
pub fn probe(token: &AuthToken, owner: &str, name: &str) -> WriteAccess {
    probe_write_access(token, owner, name, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_github::{FakeGitHub, RepoAnswer};

    /// The bead's central claim, checked against a remote that counts writes.
    ///
    /// A doctor that writes to prove it can write is a doctor that mutates
    /// state, so the fake records every non-GET and this test asserts there
    /// were none. Both the yes and the no case are checked, because a probe
    /// that short-circuits on the happy path would otherwise never reach the
    /// call that writes.
    #[test]
    fn a_writable_repo_reports_granted_and_the_remote_records_no_writes() {
        let server = FakeGitHub::start(
            "me",
            &[(
                "acme/api",
                RepoAnswer::Permissions {
                    push: true,
                    maintain: false,
                    admin: false,
                },
            )],
        );

        let result = probe_write_access(&server.token(), "acme", "api", Some(server.base_uri()));

        assert_eq!(
            result,
            WriteAccess::Granted {
                login: "me".to_string()
            },
            "a credential that may push should report Granted with the login it resolved to"
        );
        assert_eq!(
            server.writes(),
            0,
            "the probe must READ. requests seen: {:?}",
            server.requests()
        );
        assert!(
            server.requests().iter().all(|r| r.method == "GET"),
            "every request must be a GET, saw: {:?}",
            server.requests()
        );
    }

    /// The case this whole check exists for. A token with `repo` scope and no
    /// write access on one repository is an entirely ordinary state, and it
    /// used to be discovered as a 403 at the push step, after four other repos
    /// had already been pushed.
    #[test]
    fn a_read_only_credential_reports_no_write_and_still_never_writes() {
        let server = FakeGitHub::start(
            "someone-else",
            &[(
                "acme/api",
                RepoAnswer::Permissions {
                    push: false,
                    maintain: false,
                    admin: false,
                },
            )],
        );

        let result = probe_write_access(&server.token(), "acme", "api", Some(server.base_uri()));

        match &result {
            WriteAccess::Forbidden { login, .. } => {
                assert_eq!(login, "someone-else", "the resolved login must be reported")
            }
            other => panic!("a read-only credential must report Forbidden, got {other:?}"),
        }
        assert!(!result.can_push());
        assert_eq!(server.writes(), 0, "the probe must not write to find out");
    }

    /// A maintainer can push even with `push: false`. Reporting NO would send
    /// someone to fix something that is not broken.
    #[test]
    fn a_maintainer_is_reported_as_writable() {
        let server = FakeGitHub::start(
            "maintainer",
            &[(
                "acme/api",
                RepoAnswer::Permissions {
                    push: false,
                    maintain: true,
                    admin: false,
                },
            )],
        );
        let result = probe_write_access(&server.token(), "acme", "api", Some(server.base_uri()));
        assert!(result.can_push(), "a maintainer can push, got {result:?}");
        assert_eq!(server.writes(), 0);
    }

    /// The distinction the type exists for. Both make a push fail; only one is
    /// fixed by re-authenticating, and they must not look the same.
    #[test]
    fn a_rejected_token_is_distinct_from_a_forbidden_repository() {
        let forbidden_server =
            FakeGitHub::start("someone-else", &[("acme/api", RepoAnswer::Forbidden)]);
        let forbidden = probe_write_access(
            &forbidden_server.token(),
            "acme",
            "api",
            Some(forbidden_server.base_uri()),
        );

        let rejected_server = FakeGitHub::start("me", &[("acme/api", RepoAnswer::Unauthorised)]);
        let rejected = probe_write_access(
            &rejected_server.token(),
            "acme",
            "api",
            Some(rejected_server.base_uri()),
        );

        assert!(
            matches!(forbidden, WriteAccess::Forbidden { .. }),
            "a 403 on a valid token is Forbidden, got {forbidden:?}"
        );
        assert!(
            matches!(rejected, WriteAccess::Unresolvable { .. }),
            "a 401 is Unresolvable, got {rejected:?}"
        );
        // The visible difference a user acts on: only one of them knows who
        // they are, so only one of them can say "wrong account".
        assert!(forbidden.login().is_some());
        assert!(rejected.login().is_none());
        // And the reason text points at the right fix.
        let WriteAccess::Unresolvable { reason } = &rejected else {
            unreachable!("asserted above")
        };
        assert!(
            reason.contains("401"),
            "the reason must carry the status so the fix is obvious: {reason}"
        );
    }

    /// 404 is how GitHub hides a repository from a non-collaborator, and it is
    /// a different conversation with an admin than a 403 is.
    #[test]
    fn a_repository_hidden_from_a_non_collaborator_is_flagged_as_such() {
        let server = FakeGitHub::start("stranger", &[("acme/private", RepoAnswer::NotFound)]);
        let result =
            probe_write_access(&server.token(), "acme", "private", Some(server.base_uri()));
        match result {
            WriteAccess::Forbidden {
                not_a_collaborator, ..
            } => assert!(
                not_a_collaborator,
                "a 404 means not-a-collaborator, which is a different fix"
            ),
            other => panic!("expected Forbidden, got {other:?}"),
        }
        assert_eq!(server.writes(), 0);
    }

    /// The login is read first, precisely so a 401 there can be told apart from
    /// a 404 on the repository. Without that ordering both are "the push does
    /// not work" and the two fixes are indistinguishable.
    #[test]
    fn the_login_is_read_before_the_repository() {
        let server = FakeGitHub::start("me", &[]);
        let _ = probe_write_access(&server.token(), "acme", "api", Some(server.base_uri()));
        let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(
            paths.first().map(String::as_str),
            Some("/user"),
            "the login must be resolved first, got {paths:?}"
        );
    }

    /// The whole design in one number: across every outcome, the fake remote
    /// sees no writes.
    #[test]
    fn no_outcome_produces_a_write() {
        for answer in [
            RepoAnswer::Permissions {
                push: true,
                maintain: false,
                admin: false,
            },
            RepoAnswer::Permissions {
                push: false,
                maintain: false,
                admin: false,
            },
            RepoAnswer::Forbidden,
            RepoAnswer::NotFound,
            RepoAnswer::Unauthorised,
        ] {
            let server = FakeGitHub::start("me", &[("acme/api", answer)]);
            let _ = probe_write_access(&server.token(), "acme", "api", Some(server.base_uri()));
            assert_eq!(
                server.writes(),
                0,
                "{answer:?} must be answered by reading, not by writing"
            );
        }
    }

    #[test]
    fn rendering_lines_up_and_names_the_login() {
        let granted = WriteAccess::Granted {
            login: "me@corp.com".into(),
        };
        let forbidden = WriteAccess::Forbidden {
            login: "me@gmail.com".into(),
            not_a_collaborator: false,
        };
        // The columns start at the same offset, because the value is meant to
        // be read down a column in a fleet listing.
        assert!(granted.render().starts_with("write: yes"));
        assert!(forbidden.render().starts_with("write: NO "));
        assert!(granted.render().contains("me@corp.com"));
        assert!(forbidden.render().contains("me@gmail.com"));
    }

    /// The distinction the whole type exists for. Both make a push fail; only
    /// one of them is fixed by re-authenticating.
    #[test]
    fn forbidden_and_unresolvable_are_not_the_same_answer() {
        let forbidden = WriteAccess::Forbidden {
            login: "someone".into(),
            not_a_collaborator: true,
        };
        let unresolvable = WriteAccess::Unresolvable {
            reason: "token expired".into(),
        };
        assert!(!forbidden.can_push());
        assert!(!unresolvable.can_push());
        // Only one of them knows who you are, and only one has a login to
        // compare against what the user expected.
        assert_eq!(forbidden.login(), Some("someone"));
        assert_eq!(unresolvable.login(), None);
    }

    /// GitHub's roles are cumulative. Reporting NO for a maintainer sends
    /// someone to fix something that is not broken.
    ///
    /// Built by deserializing rather than a struct literal, because
    /// `Permissions` is `#[non_exhaustive]` and a literal would not compile —
    /// and because going through JSON tests the shape GitHub actually sends,
    /// not the shape this file believes it sends.
    #[test]
    fn a_maintainer_or_admin_counts_as_can_push() {
        let p = |json: &str| serde_json::from_str::<Permissions>(json).unwrap();

        assert!(permissions_allow_push(&p(
            r#"{"admin":false,"push":false,"pull":true,"triage":false,"maintain":true}"#
        )));
        assert!(permissions_allow_push(&p(
            r#"{"admin":true,"push":false,"pull":true,"triage":false,"maintain":false}"#
        )));
        assert!(!permissions_allow_push(&p(
            r#"{"admin":false,"push":false,"pull":true,"triage":false,"maintain":false}"#
        )));
        assert!(permissions_allow_push(&p(
            r#"{"admin":false,"push":true,"pull":true,"triage":false,"maintain":false}"#
        )));
    }

    #[test]
    fn an_empty_token_is_unresolvable_without_a_request() {
        let t = AuthToken::new("");
        assert!(matches!(
            probe(&t, "acme", "api"),
            WriteAccess::Unresolvable { .. }
        ));
    }
}
