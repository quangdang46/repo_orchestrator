//! Repo status detection.
//!
//! States: Current, Behind { n }, Ahead { n }, Diverged { ahead, behind },
//! Conflict, Dirty, Missing, Unknown. Quantitative variants carry the
//! commit counts so callers can render "behind by 3 commits".

/// Repository status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RepoStatus {
    Current,
    Behind { n: u32 },
    Ahead { n: u32 },
    Diverged { ahead: u32, behind: u32 },
    Conflict,
    Dirty,
    Missing,
    Unknown,
}

impl std::fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoStatus::Current => write!(f, "Current"),
            RepoStatus::Behind { n } => write!(f, "Behind by {n}"),
            RepoStatus::Ahead { n } => write!(f, "Ahead by {n}"),
            RepoStatus::Diverged { ahead, behind } => {
                write!(f, "Diverged ({ahead} ahead, {behind} behind)")
            }
            RepoStatus::Conflict => write!(f, "Conflict"),
            RepoStatus::Dirty => write!(f, "Dirty"),
            RepoStatus::Missing => write!(f, "Missing"),
            RepoStatus::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Ahead/behind counts relative to a reference (typically upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AheadBehind {
    pub ahead: u32,
    pub behind: u32,
}

impl AheadBehind {
    pub const ZERO: Self = Self {
        ahead: 0,
        behind: 0,
    };

    /// Convert ahead/behind counts into a `RepoStatus`.
    pub fn to_status(self) -> RepoStatus {
        match (self.ahead, self.behind) {
            (0, 0) => RepoStatus::Current,
            (a, 0) => RepoStatus::Ahead { n: a },
            (0, b) => RepoStatus::Behind { n: b },
            (a, b) => RepoStatus::Diverged {
                ahead: a,
                behind: b,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ahead_behind_zero_is_current() {
        assert_eq!(AheadBehind::ZERO.to_status(), RepoStatus::Current);
    }

    #[test]
    fn ahead_only() {
        let ab = AheadBehind {
            ahead: 3,
            behind: 0,
        };
        assert_eq!(ab.to_status(), RepoStatus::Ahead { n: 3 });
    }

    #[test]
    fn behind_only() {
        let ab = AheadBehind {
            ahead: 0,
            behind: 5,
        };
        assert_eq!(ab.to_status(), RepoStatus::Behind { n: 5 });
    }

    #[test]
    fn diverged() {
        let ab = AheadBehind {
            ahead: 1,
            behind: 2,
        };
        assert_eq!(
            ab.to_status(),
            RepoStatus::Diverged {
                ahead: 1,
                behind: 2
            }
        );
    }

    #[test]
    fn display_formats() {
        assert_eq!(RepoStatus::Current.to_string(), "Current");
        assert_eq!(RepoStatus::Behind { n: 3 }.to_string(), "Behind by 3");
        assert_eq!(RepoStatus::Ahead { n: 1 }.to_string(), "Ahead by 1");
        assert_eq!(
            RepoStatus::Diverged {
                ahead: 2,
                behind: 3
            }
            .to_string(),
            "Diverged (2 ahead, 3 behind)"
        );
    }
}
