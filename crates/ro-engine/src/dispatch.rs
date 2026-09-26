//! Choosing an engine, and asking whether it is available.
//!
//! # Availability is checked at DISPATCH time only
//!
//! Not at `ro init`. **Not at `ro doctor`.** This is a single rule with no
//! exception: the doctor stays a fast environment check, and its
//! provider probes are already `Severity::Optional` so they can only ever
//! warn.
//!
//! A missing engine is an `EngineOutcome::Unavailable` **for one repo** —
//! a per-repo failure, not a fleet abort. A user with neither `claude` nor
//! `codex` installed gets a loud per-repo message naming the setting that
//! changes the answer, not a silent skip.
//!
//! # Three fixed slots, not a free-form table
//!
//! The registry of behaviour profiles is a fixed three-entry array. A
//! user-editable `[engines.*]` would reintroduce the silent-failure class
//! the config section exists to kill, so the *slots* are fixed and each
//! one rejects unknown keys at parse time.
//!
//! What makes a fourth agent possible is not a fourth slot: it is
//! `[agent] command` overriding the binary and its arguments, and
//! `[agent] prompt` replacing the instruction. Gemini, Amp, Kiro or a
//! nightly build is a config change, not a PR.
//!
//! And it cannot relax the agent-does-not-push boundary. Whatever binary
//! is named, ro still owns the push, so the credential and the identity
//! guard apply to it unchanged.

use std::time::Duration;

use ro_core::CommitIdentity;

use crate::agent::AgentEngine;
use crate::git_engine::GitEngine;
use crate::{Availability, Engine, EngineContext, EngineKind, EngineOutcome};

/// Which engine to use, and how to invoke it.
///
/// Resolved once, at dispatch, from the config. Everything below this
/// point takes a `ResolvedEngine` and cannot change its mind.
pub struct ResolvedEngine {
    engine: Box<dyn Engine>,
    /// The name the user asked for, kept for messages.
    label: String,
}

impl ResolvedEngine {
    /// The engine itself.
    pub fn as_engine(&self) -> &dyn Engine {
        self.engine.as_ref()
    }

    /// What to call it in a message.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Is it installed? **Called once, at dispatch.**
    pub fn availability(&self) -> Availability {
        self.engine.availability()
    }

    /// Run it against one repo, with a default deadline.
    pub fn checkpoint(&self, ctx: &EngineContext<'_>) -> EngineOutcome {
        self.engine.checkpoint(ctx)
    }
}

/// The engine slot config, in the shape the CLI passes down.
///
/// A struct rather than a `ro_config` type so `ro-engine` keeps depending
/// on `ro-core` alone; the CLI reads the config and fills this in.
#[derive(Debug, Clone, Default)]
pub struct EngineSlots {
    pub claude_bin: Option<String>,
    pub claude_args: Option<Vec<String>>,
    pub codex_bin: Option<String>,
    pub codex_args: Option<Vec<String>>,
    pub git_bin: Option<String>,
}

/// Resolve the engine to use.
///
/// `name` is the user's choice (`--engine`, or `[agent] engine`). `bin`
/// is a per-run override for a binary installed under an unusual name —
/// the one extensibility a fixed table has to allow, and it cannot
/// register a *new* engine, only repoint an existing one.
pub fn resolve(
    name: &str,
    slots: &EngineSlots,
    bin_override: Option<&str>,
) -> Result<ResolvedEngine, String> {
    let kind = EngineKind::parse(name).ok_or_else(|| {
        format!(
            "unknown engine {name:?}. The three built-ins are claude, codex \
             and git. For another agent, set `[agent] command` to its binary \
             and arguments — that is a config change, not a new engine."
        )
    })?;

    let engine: Box<dyn Engine> = match kind {
        EngineKind::Git => {
            let mut e = GitEngine::new();
            if let Some(b) = slots.git_bin.as_deref().or(bin_override) {
                e.set_bin(b);
            }
            Box::new(e)
        }
        EngineKind::Claude => {
            let e = AgentEngine::with(
                kind,
                bin_override
                    .map(str::to_string)
                    .or_else(|| slots.claude_bin.clone())
                    .unwrap_or_else(|| "claude".to_string()),
                slots.claude_args.clone(),
            );
            Box::new(e)
        }
        EngineKind::Codex => {
            let e = AgentEngine::with(
                kind,
                bin_override
                    .map(str::to_string)
                    .or_else(|| slots.codex_bin.clone())
                    .unwrap_or_else(|| "codex".to_string()),
                slots.codex_args.clone(),
            );
            Box::new(e)
        }
    };

    Ok(ResolvedEngine {
        engine,
        label: name.to_string(),
    })
}

/// Every built-in, in the order a user is offered them.
///
/// The array is the registry. A fourth agent does not go here: it goes
/// through `[agent] command`, which repoints an existing slot.
pub fn all_engines() -> Vec<EngineKind> {
    vec![EngineKind::Claude, EngineKind::Codex, EngineKind::Git]
}

/// Availability of all three, for a dispatch-time summary.
///
/// One probe per binary, not per repo — a fleet of twenty repos sharing
/// three engines would otherwise walk `PATH` sixty times to learn three
/// facts.
pub fn availability_summary() -> Vec<(EngineKind, Availability)> {
    all_engines()
        .into_iter()
        .map(|k| {
            let bin = match k {
                EngineKind::Claude => "claude",
                EngineKind::Codex => "codex",
                EngineKind::Git => "git",
            };
            let availability = match ro_git::which(bin) {
                Some(p) => Availability::Present(p),
                None => Availability::Missing,
            };
            (k, availability)
        })
        .collect()
}

/// A deadline for one run, clamped to something a fleet can wait for.
pub fn default_timeout() -> Duration {
    Duration::from_secs(600)
}

/// The identity an engine should commit with, if any.
pub fn identity_or_none(v: Option<CommitIdentity>) -> Option<CommitIdentity> {
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_built_ins_resolve() {
        for kind in all_engines() {
            let r = resolve(kind.as_str(), &EngineSlots::default(), None)
                .unwrap_or_else(|e| panic!("{kind} should resolve: {e}"));
            assert_eq!(r.as_engine().kind(), kind);
        }
    }

    /// The failure that costs an afternoon: a typo'd name used to parse,
    /// register a phantom engine, and fail at dispatch. It is a parse
    /// error here, naming the three that exist.
    #[test]
    fn an_unknown_engine_name_is_an_error_naming_the_three() {
        // `is_err` rather than `unwrap_err`: the Ok type holds a trait
        // object, and deriving Debug on it would mean every engine has to
        // be printable just so a failure message can name it.
        assert!(resolve("cladue", &EngineSlots::default(), None).is_err());
        let err = match resolve("cladue", &EngineSlots::default(), None) {
            Ok(_) => unreachable!("a typo must not resolve"),
            Err(e) => e,
        };
        assert!(err.contains("claude"), "got: {err}");
        assert!(err.contains("codex"), "got: {err}");
        assert!(err.contains("git"), "got: {err}");
        // And it points at the config route, so the user is not left
        // hunting for a flag that does not exist.
        assert!(err.contains("[agent] command"), "got: {err}");
    }

    /// The per-run override, for a binary installed under another name.
    /// It repoints an existing engine; it does not create one.
    #[test]
    fn a_bin_override_repoints_rather_than_registers() {
        let r = resolve(
            "claude",
            &EngineSlots::default(),
            Some("/opt/bin/claude-nightly"),
        )
        .unwrap();
        assert_eq!(r.as_engine().bin(), "/opt/bin/claude-nightly");
        assert_eq!(
            r.as_engine().kind(),
            EngineKind::Claude,
            "the override must not change which engine this is"
        );
    }

    /// A slot override beats the per-run one, so a saved config is the
    /// default and the flag is the exception.
    #[test]
    fn a_slot_value_is_used_when_no_override_is_given() {
        let slots = EngineSlots {
            codex_bin: Some("/usr/local/bin/codex".into()),
            ..Default::default()
        };
        let r = resolve("codex", &slots, None).unwrap();
        assert_eq!(r.as_engine().bin(), "/usr/local/bin/codex");
    }

    /// Availability is probed per binary, not per repo.
    #[test]
    fn availability_is_summarised_over_three_entries() {
        let summary = availability_summary();
        assert_eq!(summary.len(), 3);
        // `git` is present on any machine that could run this test.
        let git = summary
            .iter()
            .find(|(k, _)| *k == EngineKind::Git)
            .map(|(_, a)| a.clone())
            .expect("git is in the summary");
        assert!(git.is_present(), "git must be found: {git:?}");
    }
}
