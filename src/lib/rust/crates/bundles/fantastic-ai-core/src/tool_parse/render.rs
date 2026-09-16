//! The two renderers — and they are TWO, deliberately.
//!
//! [`render_call`] is the inverse of the parser: it writes a `{name, arguments}`
//! back as the grammar's own text, so a test builds its fixtures with it and
//! they cannot drift from what the parser accepts.
//!
//! [`render_reply`] writes a REPLY to the model in that same tag notation — one
//! notation on the whole model↔harness path, no JSON in either direction (user,
//! 2026-09-09: "align answers in the same format"). Measured why: a JSON
//! envelope escapes every newline and quote, so the model could not see a read's
//! lines, and (2026-09-09 program_tests) a refusal carrying a JSON-dumped list
//! showed doubled backslashes that cost a run.
//!
//! They differ in whitespace and must not share a helper: `render_call` puts no
//! newline after an open tag, `render_reply` does; `render_reply` renders a
//! top-level list as `<items>` and spans a multiline scalar across lines.

use fantastic_bundle::py_str;
use serde_json::{Map, Value};

use super::grammar::ROUTING;

/// A call as the ONE grammar's text. `send` arguments are the parser's own shape
/// (`target_id`/`to` + routing + `payload`) and come back out inline, in the
/// routing order the parser reads them in.
#[must_use]
pub fn render_call(name: &str, arguments: &Map<String, Value>) -> String {
    let mut args = arguments.clone();
    if name == "send" {
        let payload = match args.remove("payload") {
            Some(Value::Object(p)) => p,
            _ => Map::new(),
        };
        let mut flat = Map::new();
        for key in ROUTING {
            if let Some(v) = args.get(key) {
                flat.insert(key.to_string(), v.clone());
            }
        }
        for (k, v) in payload {
            flat.insert(k, v);
        }
        args = flat;
    }
    let body: String = args.iter().map(|(k, v)| render_field(k, v)).collect();
    format!("<call name=\"{name}\">\n{body}</call>")
}

fn render_field(name: &str, value: &Value) -> String {
    if name == "bytes" {
        return format!("<bytes>{}</bytes>\n", flat_text(value));
    }
    match value {
        Value::Object(map) => {
            let inner: String = map.iter().map(|(k, v)| render_field(k, v)).collect();
            format!("<{name}>{inner}</{name}>\n")
        }
        Value::Array(items) => {
            let inner: String = items.iter().map(|v| render_field("item", v)).collect();
            format!("<{name}>{inner}</{name}>\n")
        }
        other => format!("<{name}>{}</{name}>\n", flat_text(other)),
    }
}

/// A reply to the model. A dict is one `<k>…</k>` per line; a list is its outer
/// tag with one `<item>` per element; text with newlines spans lines verbatim;
/// `null` is an empty tag.
#[must_use]
pub fn render_reply(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let out: String = map.iter().map(|(k, v)| render_reply_field(k, v)).collect();
            out.trim_end_matches('\n').to_string()
        }
        Value::Array(_) => render_reply_field("items", value)
            .trim_end_matches('\n')
            .to_string(),
        other => flat_text(other),
    }
}

fn render_reply_field(name: &str, value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let inner: String = map.iter().map(|(k, v)| render_reply_field(k, v)).collect();
            format!("<{name}>\n{inner}</{name}>\n")
        }
        Value::Array(items) => {
            // A list of STRUCTURES nests; a list holding any scalar renders one
            // `<item>` per element. An empty list takes the first branch, as
            // python's `all(...)` over nothing is true.
            let inner: String = if items
                .iter()
                .all(|v| matches!(v, Value::Object(_) | Value::Array(_)))
            {
                items
                    .iter()
                    .map(|v| render_reply_field("item", v))
                    .collect()
            } else {
                items
                    .iter()
                    .map(|v| format!("<item>{}</item>\n", flat_text(v)))
                    .collect()
            };
            format!("<{name}>\n{inner}</{name}>\n")
        }
        other => {
            let text = flat_text(other);
            if text.contains('\n') {
                format!("<{name}>\n{text}\n</{name}>\n")
            } else {
                format!("<{name}>{text}</{name}>\n")
            }
        }
    }
}

/// One value as the text the model reads — python `_reply_text`: `None` is
/// empty, a bool is lower-case `true`/`false`, and EVERYTHING ELSE is `str(v)`.
///
/// `str(v)` and not JSON: a float is python's repr (`1e-05`, `1e+16`, `inf`,
/// `nan`), and a structure the notation cannot say is python's own repr
/// (`{'a': 1}`), which is the last resort for a list that mixes scalars with
/// objects. Rust's `ryu` digits (`0.00001`, `1e16`) and compact JSON objects
/// were a second dialect the model read on exactly those turns.
fn flat_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        other => py_str(other),
    }
}
