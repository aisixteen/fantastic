//! CPython `json.dumps`.

use std::fmt::Write as _;

use serde_json::Value;

use crate::repr::number_json;

/// `json.dumps(v, ensure_ascii=…, indent=…)` with CPython's defaults for
/// everything else: `, ` and `: ` separators (`,` + newline under `indent`),
/// insertion order, float repr, and `\uXXXX` escapes (surrogate pairs above the
/// BMP) under `ensure_ascii`.
#[must_use]
pub fn py_json_dumps(v: &Value, ensure_ascii: bool, indent: Option<usize>) -> String {
    let mut out = String::new();
    dump(v, ensure_ascii, indent, 0, &mut out);
    out
}

fn newline_indent(indent: usize, level: usize, out: &mut String) {
    out.push('\n');
    out.push_str(&" ".repeat(indent * level));
}

fn dump(v: &Value, ensure_ascii: bool, indent: Option<usize>, level: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&number_json(n)),
        Value::String(s) => encode_str(s, ensure_ascii, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                separator(i, indent, level + 1, out);
                dump(item, ensure_ascii, indent, level + 1, out);
            }
            if let Some(n) = indent {
                newline_indent(n, level, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                separator(i, indent, level + 1, out);
                encode_str(k, ensure_ascii, out);
                out.push_str(": ");
                dump(item, ensure_ascii, indent, level + 1, out);
            }
            if let Some(n) = indent {
                newline_indent(n, level, out);
            }
            out.push('}');
        }
    }
}

fn separator(i: usize, indent: Option<usize>, level: usize, out: &mut String) {
    match indent {
        Some(n) => {
            if i > 0 {
                out.push(',');
            }
            newline_indent(n, level, out);
        }
        None => {
            if i > 0 {
                out.push_str(", ");
            }
        }
    }
}

fn encode_str(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ensure_ascii && (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{:04x}", unit);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
