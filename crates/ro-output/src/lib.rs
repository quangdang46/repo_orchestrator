//! Output rendering for ro.
//!
//! Provides consistent output in text, JSON, NDJSON, and TOON formats.
//! Every command produces structured data; this crate handles display.
//!
//! # Output formats
//!
//! - **Text** — concise, human-readable, with optional color via `owo-colors`.
//! - **Json** — stable, schema-backed, pretty-printed or compact.
//! - **Ndjson** — newline-delimited JSON, append-friendly for streaming.
//! - **Toon** — compact tabular format retained for `ru` compatibility.

pub mod format;
pub mod json;
pub mod ndjson;
pub mod text;

use std::io::Write;

// Re-export NDJSON types for ergonomic use
pub use ndjson::{NdjsonEvent, NdjsonWriter};

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Ndjson,
    Toon,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Text => write!(f, "text"),
            OutputFormat::Json => write!(f, "json"),
            OutputFormat::Ndjson => write!(f, "ndjson"),
            OutputFormat::Toon => write!(f, "toon"),
        }
    }
}

/// Parse an output format from a string (CLI flag).
pub fn parse_output_format(s: &str) -> Option<OutputFormat> {
    match s.to_ascii_lowercase().as_str() {
        "text" => Some(OutputFormat::Text),
        "json" => Some(OutputFormat::Json),
        "ndjson" => Some(OutputFormat::Ndjson),
        "toon" => Some(OutputFormat::Toon),
        _ => None,
    }
}

/// Render a JSON value to a writer in the given format.
pub fn render<W: Write>(
    w: &mut W,
    format: OutputFormat,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    match format {
        OutputFormat::Json => json::write_pretty(w, value),
        OutputFormat::Ndjson => {
            let line = format::emit_ndjson(value);
            writeln!(w, "{line}")?;
            Ok(())
        }
        OutputFormat::Text => text::write_value(w, value),
        OutputFormat::Toon => text::write_toon(w, value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_formats() {
        assert_eq!(parse_output_format("text"), Some(OutputFormat::Text));
        assert_eq!(parse_output_format("json"), Some(OutputFormat::Json));
        assert_eq!(parse_output_format("ndjson"), Some(OutputFormat::Ndjson));
        assert_eq!(parse_output_format("toon"), Some(OutputFormat::Toon));
        assert_eq!(parse_output_format("TEXT"), Some(OutputFormat::Text));
        assert_eq!(parse_output_format("bogus"), None);
    }

    #[test]
    fn display_round_trip() {
        for fmt in [
            OutputFormat::Text,
            OutputFormat::Json,
            OutputFormat::Ndjson,
            OutputFormat::Toon,
        ] {
            let s = fmt.to_string();
            assert_eq!(parse_output_format(&s), Some(fmt));
        }
    }

    #[test]
    fn render_json() {
        let val = serde_json::json!({"a": 1});
        let mut buf = Vec::new();
        render(&mut buf, OutputFormat::Json, &val).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\"a\""));
        assert!(out.contains("1"));
    }

    #[test]
    fn render_ndjson() {
        let val = serde_json::json!({"k": "v"});
        let mut buf = Vec::new();
        render(&mut buf, OutputFormat::Ndjson, &val).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.trim(), r#"{"k":"v"}"#);
    }

    #[test]
    fn render_text() {
        let val = serde_json::json!({"name": "ro", "version": "0.1.0"});
        let mut buf = Vec::new();
        render(&mut buf, OutputFormat::Text, &val).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ro"));
    }
}
