//! The message log's on-disk form: ONE XML document, rows appended, root never
//! closed.
//!
//! ```text
//! <messages v="1">
//! <row id="412" agent="ai" kind="body" parent="408" ts="1789000000.12">
//! <content>PRJ-1BCA4F
//!
//! # Report — a line with &lt;/content&gt; inside is fine, the library escaped it
//! </content>
//! </row>
//! …
//! ```
//!
//! A rule-for-rule port of python's
//! `bundled_agents/loader/kernel_state/src/kernel_state/messages_xml.py`, which
//! is canonical — and BYTE-FOR-BYTE with it, because both runtimes append to the
//! same kind of file and a row written by one must be indistinguishable from a
//! row written by the other.
//!
//! Why XML and not JSON lines (user, 2026-09-09): the model↔harness path carries
//! no JSON, and the XML parsers in the stdlib are the mature streaming ones — the
//! file is a STREAM whose root is left open, read one completed `<row>` at a
//! time; EOF without `</messages>` is the normal end. Written and read by this
//! module, never by hand: `<` and `&` in a stored body are escaped on the way out
//! and restored on the way in, so a document that contains `</content>` cannot
//! break the log. Real newlines stay real.
//!
//! Mapping (generic, no per-kind schema): a scalar field is an ATTRIBUTE, typed
//! on the way back by the ONE scalar rule the CLI and the call grammar share
//! ([`crate::coerce_scalar`]); a text field (`content`, `title`, `why`, `text`,
//! `summary`, or any string holding a newline) is a child ELEMENT; a list is a
//! child element of `<item>`s; a nested object is a child element carrying its
//! own attributes/children. `None` is omitted (absent on read = None for every
//! reader that asks). A truncated last row (a crash mid-append) is a PARSE ERROR
//! naming the last good row — fail loud, no repair.

use serde_json::{Map, Value};

use crate::coerce_scalar;
use crate::rows::Row;

/// The log format's version, stamped in the root and checked on read.
pub const VERSION: &str = "1";
/// The root, written ONCE at the head of a new log and NEVER closed — the file
/// is a stream, and EOF is how it ends.
pub const HEADER: &str = "<messages v=\"1\">\n";

/// The field names that are always a child ELEMENT, whatever they hold.
const TEXT_KEYS: [&str; 5] = ["content", "title", "why", "text", "summary"];

// ── write ───────────────────────────────────────────────────────────────────

/// One row as one `<row>…</row>` element plus a newline.
#[must_use]
pub fn row_to_xml(row: &Row) -> String {
    let mut attrs = String::new();
    let mut children = String::new();
    fill(row, &mut attrs, &mut children);
    if children.is_empty() {
        // ElementTree writes `<row />` — with the space — for an element that
        // has no children and no text. Byte parity includes the space.
        format!("<row{attrs} />\n")
    } else {
        format!("<row{attrs}>{children}</row>\n")
    }
}

fn fill(obj: &Map<String, Value>, attrs: &mut String, children: &mut String) {
    for (k, v) in obj {
        match v {
            // `None` IS OMITTED. Absent on read is None for every reader that
            // asks, so writing it would only make the file bigger.
            Value::Null => {}
            Value::Bool(b) => attr(attrs, k, if *b { "true" } else { "false" }),
            Value::Number(n) => attr(attrs, k, &number(n)),
            Value::String(s) => {
                if TEXT_KEYS.contains(&k.as_str()) || s.contains('\n') || s.contains('\r') {
                    children.push_str(&element(k, &escape_text(s)));
                } else {
                    attr(attrs, k, s);
                }
            }
            Value::Array(items) => {
                let inner: String = items.iter().map(item_element).collect();
                children.push_str(&element(k, &inner));
            }
            Value::Object(map) => {
                let mut a = String::new();
                let mut c = String::new();
                fill(map, &mut a, &mut c);
                children.push_str(&open_close(k, &a, &c));
            }
        }
    }
}

/// One `<item>` of a list.
fn item_element(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut a = String::new();
            let mut c = String::new();
            fill(map, &mut a, &mut c);
            open_close("item", &a, &c)
        }
        Value::Array(items) => {
            let inner: String = items.iter().map(item_element).collect();
            element("item", &inner)
        }
        // `null` inside a list is the ONE place None is representable: an
        // element that says so, because a missing `<item>` would change the
        // list's length.
        Value::Null => "<item null=\"true\" />".to_string(),
        Value::Bool(b) => element("item", if *b { "true" } else { "false" }),
        Value::Number(n) => element("item", &number(n)),
        Value::String(s) => element("item", &escape_text(s)),
    }
}

fn element(name: &str, inner: &str) -> String {
    if inner.is_empty() {
        format!("<{name} />")
    } else {
        format!("<{name}>{inner}</{name}>")
    }
}

fn open_close(name: &str, attrs: &str, children: &str) -> String {
    if attrs.is_empty() && children.is_empty() {
        format!("<{name} />")
    } else if children.is_empty() {
        format!("<{name}{attrs} />")
    } else {
        format!("<{name}{attrs}>{children}</{name}>")
    }
}

fn attr(out: &mut String, k: &str, v: &str) {
    out.push(' ');
    out.push_str(k);
    out.push_str("=\"");
    out.push_str(&escape_attr(v));
    out.push('"');
}

/// A float the way python's `repr` writes it — `1e+16`, not `10000000000000000`,
/// and `3.0`, not `3`. An integer is its digits.
fn number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    let plain = format!("{f}");
    // Rust prints small-magnitude floats plainly and python switches to
    // exponent form at 1e16 and below 1e-4. Match the switch, and keep the
    // trailing `.0` python's repr always writes for a whole float.
    if f != 0.0 && (f.abs() >= 1e16 || f.abs() < 1e-4) {
        let e = format!("{f:e}");
        // Rust writes `1e16`; python writes `1e+16`.
        return match e.split_once('e') {
            Some((m, x)) if !x.starts_with('-') => format!("{m}e+{x}"),
            _ => e,
        };
    }
    if plain.contains('.') || plain.contains('e') || plain.contains("inf") || plain.contains("NaN")
    {
        plain
    } else {
        format!("{plain}.0")
    }
}

/// ElementTree's ATTRIBUTE escaping: `&`, `<`, `>`, `"`, and the three
/// whitespace characters that would not survive attribute-value normalization.
/// `'` is NOT escaped. Note `&#09;`, zero-padded, which is ET's spelling.
fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#09;"),
            _ => out.push(c),
        }
    }
    out
}

/// ElementTree's TEXT escaping: `&`, `<`, `>` only. Quotes, tabs and newlines
/// go through verbatim — which is the whole point of storing a document here.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

// ── read ────────────────────────────────────────────────────────────────────

/// Every completed `<row>` in the text, in order.
///
/// The document's root is open-ended: EOF without `</messages>` is normal. A row
/// that OPENED and never closed is a CUT FILE, said loudly — the count of `<row`
/// openers is exact because the writer escapes `<` inside every stored body, so
/// the sequence can never appear in content.
pub fn parse_messages(text: &str) -> Result<Vec<Row>, String> {
    let opened = text.matches("<row ").count() + text.matches("<row>").count();
    let mut rows = Vec::new();
    let mut i = 0usize;
    while let Some(at) = text[i..].find("<row").map(|p| p + i) {
        // `<row` must be followed by a space or `>` — `<rowdy>` is not a row.
        let after = at + 4;
        match text[after..].chars().next() {
            Some(c) if c == ' ' || c == '>' || c == '/' => {}
            _ => {
                i = after;
                continue;
            }
        }
        let Some((elem, end)) = parse_element(text, at) else {
            break;
        };
        rows.push(elem.1);
        i = end;
    }
    if opened != rows.len() {
        let last = rows
            .last()
            .and_then(|r| r.get("id"))
            .map_or_else(|| "none".to_string(), ToString::to_string);
        return Err(format!(
            "messages log: {} row(s) opened and never closed — the last append \
             was cut (a crash mid-write). Truncate the file at the last complete \
             </row> (row {last}) — nothing is repaired automatically",
            opened - rows.len()
        ));
    }
    Ok(rows)
}

/// One element starting at `at`: `((name, fields), index just past its close)`.
fn parse_element(text: &str, at: usize) -> Option<((String, Row), usize)> {
    let rest = &text[at + 1..];
    let head_end = rest.find('>')? + at + 1;
    let head = &text[at + 1..head_end];
    let self_closing = head.ends_with('/');
    let head = head.trim_end_matches('/');
    let name_end = head.find(char::is_whitespace).unwrap_or(head.len());
    let name = head[..name_end].to_string();
    let mut out: Row = Map::new();
    for (k, v) in parse_attrs(&head[name_end..]) {
        out.insert(k, coerce_scalar(&v));
    }
    if self_closing {
        return Some(((name, out), head_end + 1));
    }
    let closer = format!("</{name}>");
    let mut i = head_end + 1;
    let mut text_run = String::new();
    loop {
        let tag = text[i..].find('<').map(|p| p + i)?;
        text_run.push_str(&text[i..tag]);
        if text[tag..].starts_with(&closer) {
            let body = unescape(&text_run);
            // A LEAF holds text; a BRANCH holds children. The writer never
            // mixes them, so whitespace between child elements is layout and
            // anything else is a leaf's value.
            let keep = if out.is_empty() {
                !body.is_empty()
            } else {
                !body.trim().is_empty()
            };
            if keep {
                out.insert("#text".to_string(), Value::String(body));
            }
            return Some(((name, out), tag + closer.len()));
        }
        let ((child_name, child), end) = parse_element(text, tag)?;
        merge_child(&mut out, &child_name, child);
        text_run.clear();
        i = end;
    }
}

/// One child, folded into its parent by the same rules the writer used: a run of
/// `<item>`s is a list, anything else is a field.
fn merge_child(out: &mut Row, name: &str, child: Row) {
    let value = value_of(name, child);
    if name == "item" {
        match out
            .entry("#items".to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            Value::Array(a) => a.push(value),
            _ => unreachable!("#items is always an array"),
        }
    } else {
        out.insert(name.to_string(), value);
    }
}

/// A parsed element as the value its parent should hold.
fn value_of(name: &str, mut child: Row) -> Value {
    if child.get("null") == Some(&Value::String("true".to_string()))
        || child.get("null") == Some(&Value::Bool(true))
    {
        return Value::Null;
    }
    if let Some(Value::Array(items)) = child.remove("#items") {
        return Value::Array(items);
    }
    match child.remove("#text") {
        Some(Value::String(t)) if child.is_empty() => {
            // A TEXT FIELD stays text; anything else is typed by the one scalar
            // rule — the same rule the CLI and the call grammar use.
            if TEXT_KEYS.contains(&name) || t.contains('\n') {
                Value::String(t)
            } else {
                coerce_scalar(&t)
            }
        }
        Some(t) => {
            child.insert("#text".to_string(), t);
            Value::Object(child)
        }
        None => {
            if child.is_empty() {
                // An empty element: `""` for a text field, and the one scalar
                // rule's answer for anything else (also `""`).
                Value::String(String::new())
            } else {
                Value::Object(child)
            }
        }
    }
}

fn parse_attrs(head: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b: Vec<char> = head.chars().collect();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i] != '=' && !b[i].is_whitespace() {
            i += 1;
        }
        if start == i || i >= b.len() {
            break;
        }
        let key: String = b[start..i].iter().collect();
        while i < b.len() && (b[i] == '=' || b[i].is_whitespace()) {
            i += 1;
        }
        if i >= b.len() || b[i] != '"' {
            break;
        }
        i += 1;
        let vstart = i;
        while i < b.len() && b[i] != '"' {
            i += 1;
        }
        let raw: String = b[vstart..i].iter().collect();
        i += 1;
        out.push((key, unescape(&raw)));
    }
    out
}

/// The entities the writer emits, after the newline normalization every
/// conforming XML reader applies (XML 1.0 §2.11).
///
/// ORDER IS LOAD-BEARING and this is the whole subtlety of the format:
/// normalization runs on the RAW text and unescaping after it, so a LITERAL
/// `\r\n` in element text becomes `\n` (lossy, and python is lossy the same way
/// — ElementTree writes that `\r` unescaped) while an explicitly escaped
/// `&#13;` in an attribute survives as a carriage return. Reversing the two
/// would destroy exactly the character the escape exists to protect. Verified
/// against `xml.etree.ElementTree` 2026-09-10: `a&#13;b` reads back `a\rb`,
/// `x\r\ny` reads back `x\ny`.
fn unescape(s: &str) -> String {
    let s = normalize_newlines(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            let rest: String = chars.clone().take(6).collect();
            let (entity, len) = if rest.starts_with("amp;") {
                ('&', 4)
            } else if rest.starts_with("lt;") {
                ('<', 3)
            } else if rest.starts_with("gt;") {
                ('>', 3)
            } else if rest.starts_with("quot;") {
                ('"', 5)
            } else if rest.starts_with("apos;") {
                ('\'', 5)
            } else if rest.starts_with("#10;") {
                ('\n', 4)
            } else if rest.starts_with("#13;") {
                ('\r', 4)
            } else if rest.starts_with("#09;") {
                ('\t', 4)
            } else if rest.starts_with("#9;") {
                ('\t', 3)
            } else {
                (c, 0)
            };
            if len > 0 {
                for _ in 0..len {
                    chars.next();
                }
                out.push(entity);
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
#[path = "messages_xml/tests.rs"]
mod tests;
