//! Redaction helpers for tokens and secrets.

/// Redact a secret, showing only the first `visible` chars + `…`.
pub fn redact_secret(secret: &str, visible: usize) -> String {
    if secret.len() <= visible {
        "****".to_string()
    } else {
        format!("{}…", &secret[..visible])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_short() {
        assert_eq!(redact_secret("ab", 4), "****");
    }

    #[test]
    fn redact_long() {
        assert_eq!(redact_secret("ghp_ABCDEFGH", 4), "ghp_…");
    }
}
