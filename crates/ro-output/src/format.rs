//! Shared formatting utilities.

/// Emit a single NDJSON line (no trailing newline).
///
/// NDJSON ("newline delimited JSON") is append-friendly: each line is a
/// complete JSON value, suitable for streaming progress events.
pub fn emit_ndjson(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Format an `OffsetDateTime` as an RFC 3339 string.
pub fn format_timestamp(ts: &time::OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

/// Format a duration like humans expect: "1.234s", "12m 34s", "1h 2m".
pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return format!("{:.3}s", d.as_secs_f64());
    }
    if secs < 60 {
        return format!("{:.3}s", d.as_secs_f64());
    }
    if secs < 3_600 {
        return format!("{}m {}s", secs / 60, secs % 60);
    }
    let hours = secs / 3_600;
    let mins = (secs % 3_600) / 60;
    format!("{hours}h {mins}m")
}

/// Format a byte count using SI suffixes: B, KB, MB, GB, TB.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndjson_is_compact_single_line() {
        let v = serde_json::json!({"a": 1, "b": [2, 3]});
        let s = emit_ndjson(&v);
        assert!(!s.contains('\n'));
        assert!(s.contains("\"a\":1"));
    }

    #[test]
    fn timestamp_is_rfc3339() {
        let ts = time::OffsetDateTime::from_unix_timestamp(0).unwrap();
        let s = format_timestamp(&ts);
        assert!(s.starts_with("1970-01-01T00:00:00"));
        assert!(s.ends_with('Z'));
    }

    #[test]
    fn duration_subsecond() {
        assert_eq!(
            format_duration(std::time::Duration::from_millis(123)),
            "0.123s"
        );
    }

    #[test]
    fn duration_minute_scale() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(90)),
            "1m 30s"
        );
    }

    #[test]
    fn duration_hour_scale() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(3_900)),
            "1h 5m"
        );
    }

    #[test]
    fn bytes_formatting() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GB");
    }
}
