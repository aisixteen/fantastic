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
//! own attributes/children; a key that is not an XML name is
//! `<entry key="…">value</entry>`; a raw `bytes` value is `<key len="N"/>` — the ONE
//! reduction the tape makes (base64 is banned, and a length is the honest record).
//! `None` is omitted (absent on read = None for every reader that asks). A truncated
//! last row (a crash mid-append) is a PARSE ERROR naming the last good row — fail
//! loud, no repair.

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

/// A MAP KEY IS NOT ALWAYS A TAG. Rows carry caller data now (a call's payload, a
/// settle's result), and those maps are keyed by whatever the caller wrote —
/// `callable_by: {"$parent": "*"}`, a grant map keyed by agent id. Keys that are not
/// XML names go through `<entry key="…">value</entry>` instead of becoming an
/// attribute or an element; everything else is unchanged.
const ENTRY: &str = "entry";

/// Python `_XML_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_.\-]*")`, fullmatch — the
/// subset of XML names the writer will emit as a tag or an attribute.
fn is_xml_name(k: &str) -> bool {
    let mut cs = k.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

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
        if !is_xml_name(k) {
            // `key` FIRST, then the value's own attributes — python sets it
            // before `_fill_value` runs, and attribute order is byte parity.
            let mut a = String::new();
            let mut c = String::new();
            attr(&mut a, "key", k);
            fill_value(v, &mut a, &mut c);
            children.push_str(&open_close(ENTRY, &a, &c));
            continue;
        }
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
    let mut a = String::new();
    let mut c = String::new();
    fill_value(v, &mut a, &mut c);
    open_close("item", &a, &c)
}

/// A VALUE onto an element that already exists (an `<item>`, an `<entry>`) —
/// python `_fill_value`. `children` doubles as the element's text: the writer never
/// mixes text and children, so one buffer is the whole body.
///
/// `null` is the ONE place None is representable — an element that SAYS so, because
/// a missing `<item>` would change a list's length.
fn fill_value(v: &Value, attrs: &mut String, children: &mut String) {
    match v {
        Value::Object(map) => fill(map, attrs, children),
        Value::Array(items) => {
            for it in items {
                children.push_str(&item_element(it));
            }
        }
        Value::Null => attr(attrs, "null", "true"),
        Value::Bool(b) => children.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => children.push_str(&number(n)),
        Value::String(s) => children.push_str(&escape_text(s)),
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

/// `str(v)` for an int, `repr(v)` for a float (`_fill`: `repr(v) if
/// isinstance(v, float) else str(v)`) — C51: reuses [`fantastic_bundle::
/// py_float_repr`], which already writes the two-digit exponent python's
/// `repr` does (`1e-05`, not rust `{:e}`'s `1e-5`); this file's own hand-rolled
/// copy only patched the SIGN (`1e+16`) and missed the digit count.
fn number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    // Arbitrary precision (C74): an integer literal wider than i64/u64 is
    // still an int — python `str(v)`, its exact digits — never routed through
    // `f64`, which would lose precision AND misclassify it as a float.
    let raw = n.as_str();
    if !raw.contains(['.', 'e', 'E']) {
        return raw.to_string();
    }
    fantastic_bundle::py_float_repr(n.as_f64().unwrap_or(0.0))
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
        // AN `<entry>`'s KEY IS A MAP KEY, not a scalar field: it comes back as the
        // string the writer wrote (python `child.get("key")`), never coerced into an
        // int — `{"1": …}` is not `{1: …}`.
        if name == ENTRY && k == "key" {
            out.insert(k, Value::String(v));
        } else {
            out.insert(k, coerce_scalar(&v));
        }
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
            let bare =
                out.is_empty() || (name == ENTRY && out.len() == 1 && out.contains_key("key"));
            let keep = if bare {
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
fn merge_child(out: &mut Row, name: &str, mut child: Row) {
    // `<entry key="…">` IS ITS KEY. The key attribute comes off first, so the VALUE
    // reads by the ordinary rules (python `_without_key`).
    if name == ENTRY {
        if let Some(Value::String(key)) = child.shift_remove("key") {
            let value = value_of(name, child);
            out.insert(key, value);
            return;
        }
    }
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
    if let Some(Value::Array(items)) = child.shift_remove("#items") {
        return Value::Array(items);
    }
    match child.shift_remove("#text") {
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
