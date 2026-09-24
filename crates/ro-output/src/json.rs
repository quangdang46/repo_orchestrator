//! JSON output helpers.
//!
//! JSON output is stable and schema-backed. All JSON output uses
//! `serde_json::Value` as the intermediate representation so every
//! command produces the same shape regardless of format.

use std::io::Write;

/// Write a pretty-printed JSON value to a writer.
pub fn write_pretty<W: Write>(w: &mut W, value: &serde_json::Value) -> anyhow::Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    writeln!(w, "{s}")?;
    Ok(())
}

/// Write a compact (single-line) JSON value to a writer.
pub fn write_compact<W: Write>(w: &mut W, value: &serde_json::Value) -> anyhow::Result<()> {
    let s = serde_json::to_string(value)?;
    writeln!(w, "{s}")?;
    Ok(())
}

/// Write a pretty-printed JSON value to stdout.
pub fn print_pretty(value: &serde_json::Value) {
    let _ = write_pretty(&mut std::io::stdout(), value);
}

/// Write a compact JSON value to stdout.
pub fn print_compact(value: &serde_json::Value) {
    let _ = write_compact(&mut std::io::stdout(), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_pretty_produces_indented_json() {
        let val = serde_json::json!({"a": 1, "b": [2, 3]});
        let mut buf = Vec::new();
        write_pretty(&mut buf, &val).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\"a\": 1"));
        assert!(out.contains('\n'));
    }

    #[test]
    fn write_compact_produces_single_line() {
        let val = serde_json::json!({"a": 1});
        let mut buf = Vec::new();
        write_compact(&mut buf, &val).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.trim(), r#"{"a":1}"#);
    }
}
