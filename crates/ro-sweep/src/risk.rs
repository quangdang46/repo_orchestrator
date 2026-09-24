//! Risk classification.
//!
//! LOW, MEDIUM, HIGH based on:
//! - touches many files
//! - modifies CI workflow
//! - modifies dependency file
//! - deletes files
//! - no tests changed
//! - quality gates unavailable
//! - similar failure happened recently

/// Risk level for a plan or operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "LOW"),
            RiskLevel::Medium => write!(f, "MEDIUM"),
            RiskLevel::High => write!(f, "HIGH"),
        }
    }
}

/// Individual risk reason that contributes to the overall classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskReason {
    /// Plan touches >10 files.
    TouchesManyFiles,
    /// Changes `.github/workflows/` or similar CI configuration.
    ModifiesCiWorkflow,
    /// Changes `Cargo.toml`, `package.json`, `requirements.txt`, etc.
    ModifiesDependencyFile,
    /// Any file deletion.
    DeletesFiles,
    /// No test files were added or modified.
    NoTestsChanged,
    /// Quality gate tooling is missing or broken for the repo.
    QualityGatesUnavailable,
    /// A similar failure was observed recently in this repo.
    SimilarFailureRecently,
}

impl std::fmt::Display for RiskReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskReason::TouchesManyFiles => {
                write!(f, "touches many files")
            }
            RiskReason::ModifiesCiWorkflow => {
                write!(f, "modifies CI workflow")
            }
            RiskReason::ModifiesDependencyFile => {
                write!(f, "modifies dependency file")
            }
            RiskReason::DeletesFiles => {
                write!(f, "deletes files")
            }
            RiskReason::NoTestsChanged => {
                write!(f, "no tests changed")
            }
            RiskReason::QualityGatesUnavailable => {
                write!(f, "quality gates unavailable")
            }
            RiskReason::SimilarFailureRecently => {
                write!(f, "similar failure happened recently")
            }
        }
    }
}

/// Classify an operation from a set of risk reasons.
///
/// Rules (per PLAN.md §6.2):
/// - **LOW**: no risk reasons.
/// - **HIGH**: immediately if any of:
///   `ModifiesCiWorkflow`, `DeletesFiles`, or `SimilarFailureRecently`.
///   Also if two or more reasons are present.
/// - **MEDIUM**: exactly one non-HIGH reason.
pub fn classify(reasons: &[RiskReason]) -> RiskLevel {
    if reasons.is_empty() {
        return RiskLevel::Low;
    }
    let has_high_reason = reasons.iter().any(|r| {
        matches!(
            r,
            RiskReason::ModifiesCiWorkflow
                | RiskReason::DeletesFiles
                | RiskReason::SimilarFailureRecently
        )
    });
    if has_high_reason || reasons.len() >= 2 {
        RiskLevel::High
    } else {
        RiskLevel::Medium
    }
}

/// Serialize a risk block for plan JSON.
///
/// Returns `{ "class": "low|medium|high", "reasons": [...] }`.
pub fn to_json(class: RiskLevel, reasons: &[RiskReason]) -> serde_json::Value {
    serde_json::json!({
        "class": class.to_string().to_lowercase(),
        "reasons": reasons.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_reasons_is_low() {
        assert_eq!(classify(&[]), RiskLevel::Low);
    }

    #[test]
    fn single_non_critical_is_medium() {
        assert_eq!(classify(&[RiskReason::TouchesManyFiles]), RiskLevel::Medium);
        assert_eq!(classify(&[RiskReason::NoTestsChanged]), RiskLevel::Medium);
    }

    #[test]
    fn modifies_ci_is_high() {
        assert_eq!(classify(&[RiskReason::ModifiesCiWorkflow]), RiskLevel::High);
    }

    #[test]
    fn deletes_files_is_high() {
        assert_eq!(classify(&[RiskReason::DeletesFiles]), RiskLevel::High);
    }

    #[test]
    fn similar_failure_is_high() {
        assert_eq!(
            classify(&[RiskReason::SimilarFailureRecently]),
            RiskLevel::High
        );
    }

    #[test]
    fn two_reasons_is_high() {
        assert_eq!(
            classify(&[RiskReason::TouchesManyFiles, RiskReason::NoTestsChanged]),
            RiskLevel::High
        );
    }

    #[test]
    fn many_reasons_is_high() {
        assert_eq!(
            classify(&[
                RiskReason::TouchesManyFiles,
                RiskReason::ModifiesDependencyFile,
                RiskReason::NoTestsChanged,
            ]),
            RiskLevel::High
        );
    }

    #[test]
    fn to_json_roundtrip() {
        let v = to_json(RiskLevel::Medium, &[RiskReason::DeletesFiles]);
        assert_eq!(v["class"], "medium");
        assert_eq!(v["reasons"][0], "deletes files");
    }
}
