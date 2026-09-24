//! Human-readable text output helpers.
//!
//! Text rendering converts a `serde_json::Value` into a concise display
//! suitable for terminals. It's intentionally simple: this is for
//! interactive humans, not machine consumption.

use std::io::Write;

const INDENT: &str = "  ";

/// Write a section header.
pub fn header(text: &str) {
    println!("{}\n{}", text, "─".repeat(text.chars().count()));
}

/// Write a key-value pair to stdout.
pub fn kv(key: &str, value: &str) {
    println!("{INDENT}{key}: {value}");
}

/// Render a JSON value as text to a writer.
///
/// Objects render as `key: value` lines. Arrays render as bullet
/// lists. Scalars render as their string form.
pub fn write_value<W: Write>(w: &mut W, value: &serde_json::Value) -> anyhow::Result<()> {
    write_indented(w, value, 0)
}

/// Render a JSON value as TOON (table-of-objects-notation).
///
/// TOON is the compact tabular format `ru` uses. For arrays of
/// uniform objects we emit a header row plus one row per element.
/// Scalars and non-uniform arrays fall back to text.
pub fn write_toon<W: Write>(w: &mut W, value: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(rows) = value.as_array()
        && let Some(headers) = uniform_object_keys(rows)
    {
        let mut header = String::new();
        for (i, h) in headers.iter().enumerate() {
            if i > 0 {
                header.push('\t');
            }
            header.push_str(h);
        }
        writeln!(w, "{header}")?;
        for row in rows {
            let mut line = String::new();
            for (i, h) in headers.iter().enumerate() {
                if i > 0 {
                    line.push('\t');
                }
                line.push_str(&scalar_to_string(&row[h.as_str()]));
            }
            writeln!(w, "{line}")?;
        }
        return Ok(());
    }
    // Fallback: text rendering.
    write_value(w, value)
}

fn uniform_object_keys(rows: &[serde_json::Value]) -> Option<Vec<String>> {
    let first = rows.first()?.as_object()?;
    let keys: Vec<String> = first.keys().cloned().collect();
    for row in rows.iter().skip(1) {
        let obj = row.as_object()?;
        if obj.len() != keys.len() {
            return None;
        }
        for k in &keys {
            if !obj.contains_key(k) {
                return None;
            }
        }
    }
    Some(keys)
}

fn scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

fn write_indented<W: Write>(
    w: &mut W,
    value: &serde_json::Value,
    depth: usize,
) -> anyhow::Result<()> {
    let pad = INDENT.repeat(depth);
    match value {
        serde_json::Value::Null => writeln!(w, "{pad}null")?,
        serde_json::Value::Bool(b) => writeln!(w, "{pad}{b}")?,
        serde_json::Value::Number(n) => writeln!(w, "{pad}{n}")?,
        serde_json::Value::String(s) => writeln!(w, "{pad}{s}")?,
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        writeln!(w, "{pad}-")?;
                        write_indented(w, item, depth + 1)?;
                    }
                    _ => {
                        writeln!(w, "{pad}- {}", scalar_to_string(item))?;
                    }
                }
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                match v {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        writeln!(w, "{pad}{k}:")?;
                        write_indented(w, v, depth + 1)?;
                    }
                    _ => {
                        writeln!(w, "{pad}{k}: {}", scalar_to_string(v))?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_object_renders_key_value() {
        let v = serde_json::json!({"name": "ro", "version": "0.1.0"});
        let mut buf = Vec::new();
        write_value(&mut buf, &v).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("name: ro"));
        assert!(out.contains("version: 0.1.0"));
    }

    #[test]
    fn text_nested_object_indents() {
        let v = serde_json::json!({"core": {"parallel": 8}});
        let mut buf = Vec::new();
        write_value(&mut buf, &v).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("core:"));
        assert!(out.contains("  parallel: 8"));
    }

    #[test]
    fn text_array_renders_bullets() {
        let v = serde_json::json!(["a", "b", "c"]);
        let mut buf = Vec::new();
        write_value(&mut buf, &v).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("- a"));
        assert!(out.contains("- b"));
        assert!(out.contains("- c"));
    }

    #[test]
    fn toon_uniform_array_emits_table() {
        let v = serde_json::json!([
            {"owner": "rust-lang", "name": "rust"},
            {"owner": "tokio-rs", "name": "tokio"},
        ]);
        let mut buf = Vec::new();
        write_toon(&mut buf, &v).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        // serde_json::json! uses BTreeMap → alphabetical key order
        assert_eq!(lines[0], "name\towner");
        assert_eq!(lines[1], "rust\trust-lang");
        assert_eq!(lines[2], "tokio\ttokio-rs");
    }

    #[test]
    fn toon_falls_back_to_text_for_non_uniform() {
        let v = serde_json::json!({"a": 1});
        let mut buf = Vec::new();
        write_toon(&mut buf, &v).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("a: 1"));
    }
}
