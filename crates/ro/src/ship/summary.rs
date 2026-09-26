//! The per-repo summary, and the exit code it implies.
//!
//! # This replaces the run history
//!
//! `ro_jobs::open_run` and friends have zero production callers today —
//! they exist, they are tested, and nothing calls them. With no `ro run`
//! verb there is nothing to read the rows with, and a database nobody
//! queries is not an audit trail.
//!
//! What replaces it is this: **one line per repo** naming the branch, the
//! engine, what happened, and — for a push — the account the credential
//! resolved to. That answers *"what did ro do to this repo"*. It does not
//! answer *"what did ro do last Tuesday"*, and that gap is named in the
//! plan rather than hidden behind a table nobody queries.

use std::fmt::Write;

use crate::ship::orchestrator::RepoOutcome;

/// One repo's line.
#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub label: String,
    pub branch: String,
    pub engine: String,
    /// The account the credential resolved to, when a push happened.
    pub account: Option<String>,
    pub outcome: RepoOutcome,
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub rows: Vec<SummaryRow>,
}

impl Summary {
    pub fn new(rows: Vec<SummaryRow>) -> Self {
        Self { rows }
    }

    /// How many repos failed.
    pub fn failures(&self) -> usize {
        self.rows.iter().filter(|r| r.outcome.is_failure()).count()
    }

    /// How many committed.
    pub fn committed(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    r.outcome,
                    RepoOutcome::Committed { .. } | RepoOutcome::Pushed { .. }
                )
            })
            .count()
    }

    /// How many pushed.
    pub fn pushed(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.outcome, RepoOutcome::Pushed { .. }))
            .count()
    }

    /// The counts the exit table needs.
    ///
    /// The code itself comes from `exit::RunExit`, because a summary that
    /// assigns 0/1 on its own would collapse "all failed" and "some
    /// failed" — which is the exact distinction the table exists for.
    pub fn counts(&self) -> (usize, usize) {
        (self.rows.len() - self.failures(), self.failures())
    }

    /// The human-readable table.
    pub fn render(&self) -> String {
        if self.rows.is_empty() {
            return "no repos selected.\n".to_string();
        }
        let mut out = String::new();
        for r in &self.rows {
            let account = match &r.account {
                Some(a) => format!(" as {a}"),
                None => String::new(),
            };
            let _ = writeln!(
                out,
                "{:<32} {:<24} {:<10} {}{account}",
                r.label,
                r.branch,
                r.engine,
                r.outcome.render()
            );
        }
        let _ = writeln!(
            out,
            "\n{} committed, {} pushed, {} failed.",
            self.committed(),
            self.pushed(),
            self.failures()
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, outcome: RepoOutcome) -> SummaryRow {
        SummaryRow {
            label: label.into(),
            branch: "main".into(),
            engine: "claude".into(),
            account: None,
            outcome,
        }
    }

    #[test]
    fn a_clean_run_exits_zero() {
        let s = Summary::new(vec![
            row("acme/a", RepoOutcome::Pushed { oid: "aaa".into() }),
            row("acme/b", RepoOutcome::Pushed { oid: "bbb".into() }),
        ]);
        assert_eq!(s.counts(), (2, 0));
        assert_eq!(s.pushed(), 2);
    }

    /// The bead's test. One failing repo must not report success, or the
    /// seventeen that worked hide the one that did not.
    #[test]
    fn one_failure_among_successes_exits_one() {
        let s = Summary::new(vec![
            row("acme/a", RepoOutcome::Pushed { oid: "aaa".into() }),
            row("acme/bad", RepoOutcome::Failed { error: "no".into() }),
            row("acme/c", RepoOutcome::Pushed { oid: "ccc".into() }),
        ]);
        assert_eq!(
            s.counts(),
            (2, 1),
            "one failure among successes is partial, not total"
        );
        assert_eq!(s.pushed(), 2, "and the successes are still reported");
        assert_eq!(s.failures(), 1);
    }

    #[test]
    fn a_block_counts_as_a_failure() {
        let s = Summary::new(vec![row(
            "acme/a",
            RepoOutcome::Blocked {
                reason: "secret".into(),
                detail: "github_token".into(),
            },
        )]);
        assert_eq!(s.counts().1, 1, "a safety block is not a success");
    }

    #[test]
    fn a_skip_is_not_a_failure() {
        // Mid-conflict is the user's to resolve, not a ro failure, and a
        // run that exits 1 because one repo is mid-merge trains people to
        // ignore the exit code.
        let s = Summary::new(vec![
            row(
                "acme/a",
                RepoOutcome::SkippedConflict {
                    detail: "merge".into(),
                },
            ),
            row("acme/b", RepoOutcome::NothingToCommit),
        ]);
        assert_eq!(s.counts(), (2, 0));
    }

    #[test]
    fn the_summary_names_the_failure_and_its_cause() {
        let s = Summary::new(vec![row(
            "acme/bad",
            RepoOutcome::Failed {
                error: "push failed: permission denied".into(),
            },
        )]);
        let text = s.render();
        assert!(text.contains("acme/bad"), "got: {text}");
        assert!(
            text.contains("permission denied"),
            "the cause must be visible, got: {text}"
        );
    }

    #[test]
    fn an_empty_selection_says_so() {
        assert!(Summary::default().render().contains("no repos selected"));
        assert_eq!(Summary::default().counts(), (0, 0));
    }
}
