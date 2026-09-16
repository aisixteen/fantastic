//! `yaml.safe_load` — PyYAML's semantics, not YAML 1.2's.
//!
//! The manifest is read by python with `yaml.safe_load`, whose `SafeLoader`
//! implements **YAML 1.1**: `yes`/`no`/`on`/`off` are BOOLS, `0o17`-less octal
//! (`0644`) and sexagesimal (`1:30`) are ints, `_` separates digits, and `<<`
//! merge keys splice one mapping into another. A YAML 1.2 parser reads the same
//! manifest differently — `readonly: yes` becomes the STRING `"yes"`, which is
//! truthy where python has a bool — so the roster a runtime hydrates would
//! depend on which runtime read it.
//!
//! So the document is parsed with libyaml (the same parser PyYAML's C loader
//! uses) at the EVENT level, and PyYAML's resolver + `SafeConstructor` are
//! applied to those events here: a plain scalar is resolved by PyYAML's implicit
//! regexes in PyYAML's registration order, a quoted one is always a string.
//!
//! Two shapes python holds and JSON does not, both kept as the text they were
//! written as (the same decision `coerce_scalar` records for the model path): a
//! non-finite float (`.inf`, `.nan`) and a timestamp.

use std::collections::HashMap;
use std::mem::MaybeUninit;

use fantastic_bundle::{py_float_repr, py_str};
use regex::Regex;
use serde_json::{Map, Value};
use unsafe_libyaml as unsafe_sys;

/// python `yaml.safe_load(text)`: the stream's single document, or `None`.
///
/// # Errors
/// The parse/compose/construct failure, as one line of text.
pub fn safe_load(text: &str) -> Result<Value, String> {
    let events = parse_events(text)?;
    let mut composer = Composer {
        events: &events,
        at: 0,
        anchors: HashMap::new(),
    };
    composer.single_document()
}

// ── the event stream ────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Ev {
    DocStart,
    DocEnd,
    StreamEnd,
    Alias(String),
    Scalar {
        anchor: Option<String>,
        tag: Option<String>,
        value: String,
        plain: bool,
    },
    SeqStart {
        anchor: Option<String>,
        tag: Option<String>,
    },
    SeqEnd,
    MapStart {
        anchor: Option<String>,
        tag: Option<String>,
    },
    MapEnd,
}

/// A libyaml parser that always frees itself, so an early return cannot leak it.
struct Parser {
    inner: Box<MaybeUninit<unsafe_sys::yaml_parser_t>>,
}

impl Drop for Parser {
    fn drop(&mut self) {
        // SAFETY: the parser was initialized in `parse_events` before this type
        // was constructed, and is deleted exactly once, here.
        unsafe { unsafe_sys::yaml_parser_delete(self.inner.as_mut_ptr()) }
    }
}

fn parse_events(text: &str) -> Result<Vec<Ev>, String> {
    let mut parser = Box::new(MaybeUninit::<unsafe_sys::yaml_parser_t>::uninit());
    // SAFETY: `parser` is a fresh, properly aligned allocation of the right size.
    if unsafe { unsafe_sys::yaml_parser_initialize(parser.as_mut_ptr()) }.fail {
        return Err("failed to initialise the YAML parser".to_string());
    }
    let mut parser = Parser { inner: parser };
    // SAFETY: the parser is initialized and `text` outlives every parse call
    // below (the events copy every string out before the parser is dropped).
    unsafe {
        unsafe_sys::yaml_parser_set_input_string(
            parser.inner.as_mut_ptr(),
            text.as_ptr(),
            text.len() as u64,
        );
    }
    let mut out = Vec::new();
    loop {
        let mut event = MaybeUninit::<unsafe_sys::yaml_event_t>::uninit();
        // SAFETY: the parser is initialized; `event` is a valid out-parameter.
        if unsafe { unsafe_sys::yaml_parser_parse(parser.inner.as_mut_ptr(), event.as_mut_ptr()) }
            .fail
        {
            // SAFETY: after a failed parse the parser holds its problem text;
            // it is a NUL-terminated static string owned by the parser.
            let problem = unsafe { problem_text(parser.inner.as_ptr()) };
            return Err(problem);
        }
        // SAFETY: a successful parse initialized the event.
        let ev = unsafe { read_event(event.as_ptr()) };
        // SAFETY: the event is initialized and every string has been copied out.
        unsafe { unsafe_sys::yaml_event_delete(event.as_mut_ptr()) };
        let done = matches!(ev, Some(Ev::StreamEnd));
        if let Some(ev) = ev {
            out.push(ev);
        }
        if done {
            return Ok(out);
        }
    }
}

/// # Safety
/// `event` points at an initialized, not-yet-deleted libyaml event.
unsafe fn read_event(event: *const unsafe_sys::yaml_event_t) -> Option<Ev> {
    let string = |p: *const u8, len: u64| -> String {
        if p.is_null() {
            return String::new();
        }
        let bytes = std::slice::from_raw_parts(p, usize::try_from(len).unwrap_or(0));
        String::from_utf8_lossy(bytes).into_owned()
    };
    let cstring = |p: *const u8| -> Option<String> {
        if p.is_null() {
            return None;
        }
        let mut len = 0u64;
        while *p.offset(isize::try_from(len).unwrap_or(0)) != 0 {
            len += 1;
        }
        Some(string(p, len))
    };
    match (*event).type_ {
        unsafe_sys::YAML_DOCUMENT_START_EVENT => Some(Ev::DocStart),
        unsafe_sys::YAML_DOCUMENT_END_EVENT => Some(Ev::DocEnd),
        unsafe_sys::YAML_STREAM_END_EVENT => Some(Ev::StreamEnd),
        unsafe_sys::YAML_ALIAS_EVENT => Some(Ev::Alias(
            cstring((*event).data.alias.anchor).unwrap_or_default(),
        )),
        unsafe_sys::YAML_SCALAR_EVENT => {
            let s = (*event).data.scalar;
            Some(Ev::Scalar {
                anchor: cstring(s.anchor),
                tag: cstring(s.tag),
                value: string(s.value, s.length),
                plain: s.style == unsafe_sys::YAML_PLAIN_SCALAR_STYLE,
            })
        }
        unsafe_sys::YAML_SEQUENCE_START_EVENT => Some(Ev::SeqStart {
            anchor: cstring((*event).data.sequence_start.anchor),
            tag: cstring((*event).data.sequence_start.tag),
        }),
        unsafe_sys::YAML_SEQUENCE_END_EVENT => Some(Ev::SeqEnd),
        unsafe_sys::YAML_MAPPING_START_EVENT => Some(Ev::MapStart {
            anchor: cstring((*event).data.mapping_start.anchor),
            tag: cstring((*event).data.mapping_start.tag),
        }),
        unsafe_sys::YAML_MAPPING_END_EVENT => Some(Ev::MapEnd),
        _ => None,
    }
}

/// # Safety
/// `parser` points at an initialized parser that has just failed.
unsafe fn problem_text(parser: *const unsafe_sys::yaml_parser_t) -> String {
    // The public error fields sit behind libyaml's `Deref` prefix struct, so the
    // read goes through an explicit reference to the (initialized) parser.
    let parser: &unsafe_sys::yaml_parser_t = &*parser;
    let mark = parser.problem_mark;
    let problem = if parser.problem.is_null() {
        "parse error".to_string()
    } else {
        std::ffi::CStr::from_ptr(parser.problem)
            .to_string_lossy()
            .into_owned()
    };
    // PyYAML's pure-python scanner writes a multi-line `MarkedYAMLError`; this is
    // libyaml's one-line problem plus the same position (a §5 residual, D4).
    format!(
        "{problem} at line {}, column {}",
        mark.line + 1,
        mark.column + 1
    )
}

// ── compose + construct (PyYAML's Composer + SafeConstructor) ────────

#[derive(Clone, Debug)]
enum Node {
    Scalar {
        tag: String,
        value: String,
    },
    Seq {
        tag: String,
        items: Vec<Node>,
    },
    Map {
        tag: String,
        pairs: Vec<(Node, Node)>,
    },
}

const TAG_PREFIX: &str = "tag:yaml.org,2002:";

struct Composer<'a> {
    events: &'a [Ev],
    at: usize,
    anchors: HashMap<String, Node>,
}

impl Composer<'_> {
    fn next(&mut self) -> Option<&Ev> {
        let ev = self.events.get(self.at);
        if ev.is_some() {
            self.at += 1;
        }
        ev
    }

    /// python `get_single_data`: one document, or `None` for an empty stream; a
    /// second document is an error.
    fn single_document(&mut self) -> Result<Value, String> {
        let mut data = Value::Null;
        let mut seen = false;
        while let Some(ev) = self.next().cloned() {
            match ev {
                Ev::DocStart => {
                    if seen {
                        return Err("expected a single document in the stream".to_string());
                    }
                    let node = self.compose_node()?;
                    data = construct(&node)?;
                    seen = true;
                }
                Ev::StreamEnd => break,
                _ => {}
            }
        }
        Ok(data)
    }

    fn compose_node(&mut self) -> Result<Node, String> {
        let Some(ev) = self.next().cloned() else {
            return Err("unexpected end of the YAML event stream".to_string());
        };
        match ev {
            Ev::Alias(anchor) => self
                .anchors
                .get(&anchor)
                .cloned()
                .ok_or_else(|| format!("found undefined alias '{anchor}'")),
            Ev::Scalar {
                anchor,
                tag,
                value,
                plain,
            } => {
                let tag = resolved_tag(tag.as_deref(), &value, plain);
                let node = Node::Scalar { tag, value };
                self.anchor(anchor, &node);
                Ok(node)
            }
            Ev::SeqStart { anchor, tag } => {
                let tag = tag
                    .filter(|t| t != "!")
                    .unwrap_or_else(|| format!("{TAG_PREFIX}seq"));
                let mut items = Vec::new();
                loop {
                    match self.events.get(self.at) {
                        Some(Ev::SeqEnd) => {
                            self.at += 1;
                            break;
                        }
                        None => return Err("unexpected end of the YAML event stream".to_string()),
                        _ => items.push(self.compose_node()?),
                    }
                }
                let node = Node::Seq { tag, items };
                self.anchor(anchor, &node);
                Ok(node)
            }
            Ev::MapStart { anchor, tag } => {
                let tag = tag
                    .filter(|t| t != "!")
                    .unwrap_or_else(|| format!("{TAG_PREFIX}map"));
                let mut pairs = Vec::new();
                loop {
                    match self.events.get(self.at) {
                        Some(Ev::MapEnd) => {
                            self.at += 1;
                            break;
                        }
                        None => return Err("unexpected end of the YAML event stream".to_string()),
                        _ => {
                            let k = self.compose_node()?;
                            let v = self.compose_node()?;
                            pairs.push((k, v));
                        }
                    }
                }
                let node = Node::Map { tag, pairs };
                self.anchor(anchor, &node);
                Ok(node)
            }
            other => Err(format!("unexpected YAML event {other:?}")),
        }
    }

    /// An anchored node is remembered whole: PyYAML's composer stores the NODE,
    /// so an alias re-constructs the same structure.
    fn anchor(&mut self, anchor: Option<String>, node: &Node) {
        if let Some(a) = anchor {
            self.anchors.insert(a, node.clone());
        }
    }
}

/// PyYAML's `Resolver.resolve` for a scalar: an explicit tag wins; a PLAIN
/// scalar is matched against the implicit resolvers in registration order;
/// anything else is a string.
fn resolved_tag(tag: Option<&str>, value: &str, plain: bool) -> String {
    if let Some(t) = tag {
        if t != "!" {
            return t.to_string();
        }
    }
    if plain {
        for (name, re) in implicit_resolvers() {
            if re.is_match(value) {
                return format!("{TAG_PREFIX}{name}");
            }
        }
    }
    format!("{TAG_PREFIX}str")
}

/// PyYAML's implicit resolvers, verbatim, in the order `resolver.py` registers
/// them (the first-character index only filters, so order is what decides).
fn implicit_resolvers() -> &'static [(&'static str, Regex)] {
    static RESOLVERS: std::sync::OnceLock<Vec<(&'static str, Regex)>> = std::sync::OnceLock::new();
    RESOLVERS.get_or_init(|| {
        let re = |p: &str| Regex::new(p).expect("PyYAML's own pattern compiles");
        vec![
            (
                "bool",
                re(r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)$"),
            ),
            (
                "float",
                re(r"^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$"),
            ),
            (
                "int",
                re(r"^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$"),
            ),
            ("merge", re(r"^(?:<<)$")),
            ("null", re(r"^(?:~|null|Null|NULL|)$")),
            (
                "timestamp",
                re(r"^(?:[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?)$"),
            ),
        ]
    })
}

/// PyYAML's `SafeConstructor` over a composed node.
fn construct(node: &Node) -> Result<Value, String> {
    match node {
        Node::Scalar { tag, value } => construct_scalar(tag, value),
        Node::Seq { tag, items } => {
            if tag != &format!("{TAG_PREFIX}seq") {
                return Err(format!(
                    "could not determine a constructor for the tag '{tag}'"
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(construct(item)?);
            }
            Ok(Value::Array(out))
        }
        Node::Map { tag, pairs } => {
            if tag != &format!("{TAG_PREFIX}map") {
                return Err(format!(
                    "could not determine a constructor for the tag '{tag}'"
                ));
            }
            let flattened = flatten_mapping(pairs)?;
            let mut out = Map::new();
            for (k, v) in flattened {
                let key = construct(&k)?;
                // A python dict holds any hashable key; a JSON object holds
                // strings, so a non-string key is written as python prints it.
                out.insert(py_str(&key), construct(&v)?);
            }
            Ok(Value::Object(out))
        }
    }
}

/// PyYAML's `flatten_mapping`: a `<<` key splices its mapping (or its list of
/// mappings, last one first) IN FRONT of the mapping's own pairs, so an own key
/// overrides a merged one.
fn flatten_mapping(pairs: &[(Node, Node)]) -> Result<Vec<(Node, Node)>, String> {
    let mut merge: Vec<(Node, Node)> = Vec::new();
    let mut own: Vec<(Node, Node)> = Vec::new();
    for (k, v) in pairs {
        let is_merge =
            matches!(k, Node::Scalar { tag, .. } if tag == &format!("{TAG_PREFIX}merge"));
        if !is_merge {
            own.push((k.clone(), v.clone()));
            continue;
        }
        match v {
            Node::Map { pairs, .. } => merge.extend(flatten_mapping(pairs)?),
            Node::Seq { items, .. } => {
                let mut submerge: Vec<Vec<(Node, Node)>> = Vec::new();
                for sub in items {
                    let Node::Map { pairs, .. } = sub else {
                        return Err(
                            "while constructing a mapping: expected a mapping for merging"
                                .to_string(),
                        );
                    };
                    submerge.push(flatten_mapping(pairs)?);
                }
                submerge.reverse();
                for sub in submerge {
                    merge.extend(sub);
                }
            }
            _ => {
                return Err(
                    "while constructing a mapping: expected a mapping or list of mappings for \
                     merging"
                        .to_string(),
                )
            }
        }
    }
    merge.extend(own);
    Ok(merge)
}

fn construct_scalar(tag: &str, value: &str) -> Result<Value, String> {
    let Some(kind) = tag.strip_prefix(TAG_PREFIX) else {
        return Err(format!(
            "could not determine a constructor for the tag '{tag}'"
        ));
    };
    match kind {
        "str" => Ok(Value::String(value.to_string())),
        "null" => Ok(Value::Null),
        "bool" => Ok(Value::Bool(matches!(
            value.to_lowercase().as_str(),
            "yes" | "true" | "on"
        ))),
        "int" => Ok(construct_int(value)),
        "float" => Ok(construct_float(value)),
        // python builds a `datetime`; JSON has no such scalar, so the text
        // stands (residual, like a non-finite float).
        "timestamp" => Ok(Value::String(value.to_string())),
        _ => Err(format!(
            "could not determine a constructor for the tag '{tag}'"
        )),
    }
}

/// PyYAML `construct_yaml_int`: `_` separators, `0b`/`0x`/leading-`0` octal and
/// sexagesimal. An integer too wide for `i64` keeps its digits as text (D13).
fn construct_int(raw: &str) -> Value {
    let cleaned = raw.replace('_', "");
    let (sign, digits) = match cleaned.strip_prefix('-') {
        Some(rest) => (-1i128, rest.to_string()),
        None => (
            1i128,
            cleaned.strip_prefix('+').unwrap_or(&cleaned).to_string(),
        ),
    };
    let parsed: Option<i128> = if digits == "0" {
        Some(0)
    } else if let Some(bits) = digits.strip_prefix("0b") {
        i128::from_str_radix(bits, 2).ok()
    } else if let Some(hex) = digits.strip_prefix("0x") {
        i128::from_str_radix(hex, 16).ok()
    } else if digits.starts_with('0') {
        i128::from_str_radix(&digits, 8).ok()
    } else if digits.contains(':') {
        let mut total: i128 = 0;
        let mut ok = true;
        for part in digits.split(':') {
            match part.parse::<i128>() {
                Ok(n) => total = total * 60 + n,
                Err(_) => ok = false,
            }
        }
        ok.then_some(total)
    } else {
        digits.parse::<i128>().ok()
    };
    match parsed.map(|n| sign * n).and_then(|n| i64::try_from(n).ok()) {
        Some(n) => Value::Number(n.into()),
        None => Value::String(raw.to_string()),
    }
}

/// PyYAML `construct_yaml_float`: `_` separators, `.inf`/`.nan` and
/// sexagesimal. A non-finite float keeps its python repr as text (D13).
fn construct_float(raw: &str) -> Value {
    let cleaned = raw.replace('_', "").to_lowercase();
    let (sign, rest) = match cleaned.strip_prefix('-') {
        Some(rest) => (-1.0f64, rest.to_string()),
        None => (
            1.0f64,
            cleaned.strip_prefix('+').unwrap_or(&cleaned).to_string(),
        ),
    };
    let value = if rest == ".inf" {
        sign * f64::INFINITY
    } else if rest == ".nan" {
        f64::NAN
    } else if rest.contains(':') {
        let mut total = 0.0f64;
        for part in rest.split(':') {
            match part.parse::<f64>() {
                Ok(n) => total = total * 60.0 + n,
                Err(_) => return Value::String(raw.to_string()),
            }
        }
        sign * total
    } else {
        match rest.parse::<f64>() {
            Ok(n) => sign * n,
            Err(_) => return Value::String(raw.to_string()),
        }
    };
    match serde_json::Number::from_f64(value) {
        Some(n) => Value::Number(n),
        None => Value::String(py_float_repr(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::safe_load;
    use serde_json::json;

    #[test]
    fn plain_scalars_follow_yaml_1_1_as_pyyaml_reads_them() {
        let v = safe_load("flag: yes\nq: 'yes'\nlamp: off\nn: no\n").unwrap();
        assert_eq!(
            v,
            json!({"flag": true, "q": "yes", "lamp": false, "n": false})
        );
        // A plain KEY is resolved too: python's dict key here is the bool False,
        // which a JSON object spells as python prints it.
        assert_eq!(safe_load("off: 1\n").unwrap(), json!({"False": 1}));
        let v =
            safe_load("a: 0644\nb: 1_000\nc: 0x1f\nd: 1:30\ne: 3.5\nf: ~\ng:\nh: NULL\n").unwrap();
        assert_eq!(
            v,
            json!({"a": 420, "b": 1000, "c": 31, "d": 90, "e": 3.5, "f": null, "g": null, "h": null})
        );
        // A quoted scalar is a string whatever it spells.
        assert_eq!(
            safe_load("a: '1'\nb: \"~\"\nc: 'off'\n").unwrap(),
            json!({"a": "1", "b": "~", "c": "off"})
        );
    }

    #[test]
    fn merge_keys_splice_like_pyyaml() {
        let doc = "\
base: &b
  id: a
  mode: mem
one:
  <<: *b
  mode: disk
two:
  <<: [*b, {extra: 1}]
";
        let v = safe_load(doc).unwrap();
        assert_eq!(v["one"], json!({"id": "a", "mode": "disk"}));
        assert_eq!(v["two"], json!({"extra": 1, "id": "a", "mode": "mem"}));
    }

    #[test]
    fn an_empty_or_falsy_document_loads_as_python_does() {
        assert_eq!(safe_load("").unwrap(), json!(null));
        assert_eq!(safe_load("~").unwrap(), json!(null));
        assert_eq!(safe_load("0").unwrap(), json!(0));
        assert_eq!(safe_load("[]").unwrap(), json!([]));
        assert_eq!(safe_load("{}").unwrap(), json!({}));
        assert_eq!(safe_load("[1]").unwrap(), json!([1]));
    }

    #[test]
    fn a_broken_document_is_an_error_not_a_panic() {
        let e = safe_load("a: [1\n").unwrap_err();
        assert!(!e.is_empty(), "{e}");
        assert!(safe_load("---\na: 1\n---\nb: 2\n").is_err());
    }
}
