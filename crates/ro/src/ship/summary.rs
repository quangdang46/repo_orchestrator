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

use clap::ValueEnum;

use crate::ship::orchestrator::RepoOutcome;

/// How a run's result is written out.
///
/// One type for every command that takes `--format`, so the fleet verbs and
/// the read commands cannot drift into two enums with the same variants and
/// different meanings — which is exactly what happened when the fleet verbs
/// grew their own.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// The aligned table, for a person.
    #[default]
    Text,
    Json,
    /// One JSON object per line, for a consumer that reads a stream.
    ///
    /// Not a second machine format: it is the same objects `json` emits,
    /// framed so a reader can process twenty repos without holding all
    /// twenty in memory. That framing is the only difference, and it is
    /// only worth a variant where there is more than one row.
    Ndjson,
}

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

    /// The machine-readable form.
    ///
    /// The text table is padded for a terminal, which is exactly what a
    /// script does not want: it has to strip columns, and a column that
    /// changes width between runs breaks the strip. These are the stable
    /// shapes — one object per repo with named fields, and a summary
    /// object carrying the same counts the exit code is derived from, so a
    /// script reading the result and the process status can never disagree.
    pub fn render_json(&self, format: OutputFormat) -> String {
        let rows: Vec<serde_json::Value> = self
            .rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "label": r.label,
                    "branch": r.branch,
                    "engine": r.engine,
                    "account": r.account,
                    "outcome": r.outcome.render(),
                    "failed": r.outcome.is_failure(),
                })
            })
            .collect();
        let (succeeded, failed) = self.counts();
        let summary = serde_json::json!({
            "committed": self.committed(),
            "pushed": self.pushed(),
            "failed": self.failures(),
            "exit": crate::exit::RunExit::from_counts(succeeded, failed).code(),
        });

        match format {
            OutputFormat::Ndjson => {
                let mut out = String::new();
                for row in &rows {
                    out.push_str(&row.to_string());
                    out.push('\n');
                }
                let mut last = summary.as_object().expect("a json object").clone();
                last.insert("summary".into(), serde_json::Value::Bool(true));
                out.push_str(&serde_json::Value::Object(last).to_string());
                out.push('\n');
                out
            }
            OutputFormat::Json => {
                let doc = serde_json::json!({ "repos": rows, "summary": summary });
                serde_json::to_string_pretty(&doc)
                    .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
            }
            // The caller renders text itself. Reaching here would mean a new
            // variant was added with no branch, and handing back a table a
            // script would try to parse is the failure that hides.
            OutputFormat::Text => self.render(),
        }
    }

    /// Repos that were not acted on and that need a person to.
    ///
    /// Distinct from a failure — ro did not fail, it declined — and
    /// distinct from `NothingToCommit`, which needs nobody. A conflict
    /// stops the pipeline because continuing would pick a side, and
    /// picking a side is the one thing this tool must never do on its own.
    ///
    /// `HandedOver` belongs here as much as `SkippedConflict` does: both are
    /// a conflict that stopped the pipeline and left the work unlanded, and
    /// the two differ only in *when* the pipeline stopped — during this run,
    /// or on a previous one. Counting only the first left a fleet of repos
    /// waiting on a human reading "0 committed, 0 pushed, 0 failed".
    pub fn skipped_needing_action(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    r.outcome,
                    RepoOutcome::SkippedConflict { .. } | RepoOutcome::HandedOver { .. }
                )
            })
            .count()
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
        // Skips are counted and named. The exit code deliberately does not
        // turn on them — see `RepoOutcome::is_failure` — but "0 failed" over
        // a fleet where three repos are wedged mid-merge is a sentence
        // nobody believes, and this is the number a reader takes away.
        let _ = write!(
            out,
            "\n{} committed, {} pushed, {} failed",
            self.committed(),
            self.pushed(),
            self.failures()
        );
        if self.skipped_needing_action() > 0 {
            let _ = write!(
                out,
                ", {} skipped (need a human)",
                self.skipped_needing_action()
            );
        }
        out.push_str(".\n");
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
            row(
                "acme/bad",
                RepoOutcome::Failed {
                    error: "no".into(),
                    class: ro_core::FailureClass::MissingProvider,
                },
            ),
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
                class: ro_core::FailureClass::GithubPermissionDenied,
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


/// The summary line must not read "0 failed" over a fleet that needs help.
#[cfg(test)]
mod skip_reporting_tests {
    use super::*;
    use crate::ship::orchestrator::RepoOutcome;

    fn row(label: &str, outcome: RepoOutcome) -> SummaryRow {
        SummaryRow {
            label: label.into(),
            branch: "main".into(),
            engine: "git".into(),
            account: None,
            outcome,
        }
    }

    /// The exit code deliberately ignores a mid-conflict — ro did not fail,
    /// and a signal that fires for an already-diagnosed condition is one
    /// people learn to ignore. The **sentence** is a different question, and
    /// "0 failed" over three wedged repos is one nobody believes.
    #[test]
    fn a_wedged_fleet_says_so_even_though_the_exit_code_is_clean() {
        let s = Summary::new(vec![
            row(
                "acme/a",
                RepoOutcome::SkippedConflict {
                    detail: "merge".into(),
                },
            ),
            row(
                "acme/b",
                RepoOutcome::SkippedConflict {
                    detail: "merge".into(),
                },
            ),
            row("acme/c", RepoOutcome::NothingToCommit),
        ]);
        let text = s.render();
        assert!(
            text.contains("2 skipped (need a human)"),
            "the line must name what still needs doing, got:\n{text}"
        );
        // And it is counted separately from failures, which stays the
        // deliberate answer above.
        assert_eq!(s.skipped_needing_action(), 2);
        assert_eq!(s.failures(), 0);
        assert_eq!(s.counts(), (3, 0), "the exit code is a separate question");
    }

    /// A `HandedOver` row is counted as **nothing at all**.
    ///
    /// The enum's own doc comment says "Not a success either — the work did
    /// not land — so the summary says so", and it does not: `HandedOver` is
    /// excluded from `committed()`, `pushed()`, `failures()` *and*
    /// `skipped_needing_action()`. So a fleet of three repos each waiting on
    /// a human conflict resolution printed "0 committed, 0 pushed, 0 failed"
    /// and exited 0 — the reader takes away that nothing happened, while
    /// every repo's work is stranded.
    ///
    /// `SkippedConflict` was fixed in the same shape; this is the other half.
    #[test]
    fn a_handed_over_fleet_is_counted_as_needing_a_human() {
        let s = Summary::new(vec![
            row(
                "acme/a",
                RepoOutcome::HandedOver {
                    detail: "conflict: 1 file(s) need you".into(),
                },
            ),
            row(
                "acme/b",
                RepoOutcome::HandedOver {
                    detail: "conflict: 1 file(s) need you".into(),
                },
            ),
            row("acme/c", RepoOutcome::NothingToCommit),
        ]);

        let text = s.render();
        assert!(
            text.contains("2 skipped (need a human)"),
            "a handed-over repo must be counted as needing a human, got:\n{text}"
        );
        assert_eq!(
            s.skipped_needing_action(),
            2,
            "HandedOver belongs in the same count as SkippedConflict"
        );
        // Still not a failure: nothing is broken, and the exit code stays
        // clean for the same reason SkippedConflict's does.
        assert_eq!(s.failures(), 0);
        assert_eq!(s.counts(), (3, 0), "the exit code is a separate question");
    }

    /// A clean fleet says nothing extra — no line, no noise.
    #[test]
    fn a_clean_fleet_names_no_skips() {
        let s = Summary::new(vec![row("acme/a", RepoOutcome::NothingToCommit)]);
        let text = s.render();
        assert!(!text.contains("skipped"), "got:\n{text}");
        assert!(text.contains("0 failed."), "got:\n{text}");
    }
}
