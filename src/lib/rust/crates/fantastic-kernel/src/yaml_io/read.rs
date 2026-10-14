//! READ — python's `yaml_io.load` (`yaml.load(text, Loader=yaml.CSafeLoader)`) and
//! `yaml.load_all(text, Loader=yaml.CSafeLoader)`: PyYAML's YAML 1.1, not 1.2.
//!
//! ONE READER for everything the kernel reads — kernel state (records,
//! `lock.yaml`, the scheduler's sidecars, the credential store) and the
//! static_loader's manifests. The document is parsed by libyaml (the parser
//! PyYAML's C loader drives) at the EVENT level, and PyYAML's `Composer`,
//! `Resolver` and `SafeConstructor` are applied to those events here:
//!
//!   * a scalar with no tag (or the non-specific `!`) is resolved by PyYAML's
//!     implicit resolvers, in registration order, when libyaml marks it
//!     `plain_implicit` (a plain scalar, or any scalar tagged `!`); anything else
//!     is a string. So `yes`/`no`/`on`/`off` are BOOLS, `0644` is octal 420, `08`
//!     and `0o17` and `1e3` are strings, `1:30` is 90, `1_000` is 1000, `0b101`
//!     is 5;
//!   * an explicit tag picks its constructor (`!!str 1` is `"1"`, `!!int '7'` is
//!     7); a tag no `SafeConstructor` knows is the python error;
//!   * anchors and aliases resolve (an alias re-constructs the anchored node);
//!     `<<` merge keys splice like PyYAML's `flatten_mapping`;
//!   * a non-string mapping KEY is written as python prints the dict key
//!     (`yes: 1` → `"True"`), and keys python's dict holds as EQUAL (`1`, `1.0`,
//!     `true`) collapse onto the first one, the last value winning.
//!
//! Values python holds and JSON does not:
//!   * a timestamp is python's `str()` of the `date`/`datetime` it builds —
//!     what python's wire codec (`json.dumps(default=str)`) and its re-dump
//!     (`yaml_io.plain`) both make of it;
//!   * a non-finite float is its python repr (`inf`, `-inf`, `nan`);
//!   * an int of any width keeps every digit (`arbitrary_precision`);
//!   * `!!binary` (bytes), `!!set` (a set) and a RECURSIVE alias have no JSON
//!     form at all and fail the load, naming the node.

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::sync::OnceLock;

use fantastic_bundle::{py_float_repr, py_repr, py_strip};
use regex::Regex;
use serde_json::{Map, Number, Value};
use unsafe_libyaml as unsafe_sys;

const TAG_PREFIX: &str = "tag:yaml.org,2002:";

/// One YAML document back (python `yaml_io.load`): an empty stream is `null`, a
/// stream of several documents is an error.
///
/// # Errors
/// The parse/compose/construct failure, as one line of text.
pub fn load(text: &str) -> Result<Value, String> {
    let mut c = Composer::new(text)?;
    c.skip_stream_start()?;
    let node = if matches!(c.peek()?, Ev::StreamEnd) {
        None
    } else {
        Some(c.compose_document()?)
    };
    if !matches!(c.next()?, Ev::StreamEnd) {
        return Err("expected a single document in the stream".to_string());
    }
    match node {
        None => Ok(Value::Null),
        Some(n) => to_json(construct(&n)?),
    }
}

/// Every document of a stream, in order (python `yaml.load_all`). An empty
/// stream is no documents; an empty document (`---` alone) is `null`.
///
/// # Errors
/// The first parse/compose/construct failure, as one line of text.
pub fn load_all(text: &str) -> Result<Vec<Value>, String> {
    let mut c = Composer::new(text)?;
    c.skip_stream_start()?;
    let mut out = Vec::new();
    while !matches!(c.peek()?, Ev::StreamEnd) {
        let node = c.compose_document()?;
        out.push(to_json(construct(&node)?)?);
    }
    Ok(out)
}

// ── the event stream (libyaml) ──────────────────────────────────────

#[derive(Clone, Debug)]
enum Ev {
    StreamStart,
    StreamEnd,
    DocStart,
    DocEnd,
    Alias(String),
    Scalar {
        anchor: Option<String>,
        tag: Option<String>,
        value: String,
        /// libyaml's `plain_implicit`: a plain scalar with no tag, or ANY scalar
        /// tagged with the non-specific `!` — the ones the resolver reads.
        implicit: bool,
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

/// A libyaml parser over borrowed text that always frees itself.
struct Parser<'a> {
    inner: Box<MaybeUninit<unsafe_sys::yaml_parser_t>>,
    done: bool,
    _text: PhantomData<&'a str>,
}

impl Drop for Parser<'_> {
    fn drop(&mut self) {
        // SAFETY: the parser was initialized in `Parser::new` before this value
        // existed, and is deleted exactly once, here.
        unsafe { unsafe_sys::yaml_parser_delete(self.inner.as_mut_ptr()) }
    }
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Result<Self, String> {
        let mut inner = Box::new(MaybeUninit::<unsafe_sys::yaml_parser_t>::uninit());
        // SAFETY: `inner` is a fresh, properly aligned allocation of the right size.
        if unsafe { unsafe_sys::yaml_parser_initialize(inner.as_mut_ptr()) }.fail {
            return Err("failed to initialise the YAML parser".to_string());
        }
        let mut p = Parser {
            inner,
            done: false,
            _text: PhantomData,
        };
        // SAFETY: the parser is initialized and `text` outlives it (`'a`).
        unsafe {
            unsafe_sys::yaml_parser_set_input_string(
                p.inner.as_mut_ptr(),
                text.as_ptr(),
                text.len() as u64,
            );
        }
        Ok(p)
    }

    /// The next event libyaml produces (skipping the directive-free kinds this
    /// reader never sees). Past the stream end it keeps answering `StreamEnd`.
    fn next_event(&mut self) -> Result<Ev, String> {
        if self.done {
            return Ok(Ev::StreamEnd);
        }
        loop {
            let mut event = MaybeUninit::<unsafe_sys::yaml_event_t>::uninit();
            // SAFETY: the parser is initialized; `event` is a valid out-parameter.
            if unsafe { unsafe_sys::yaml_parser_parse(self.inner.as_mut_ptr(), event.as_mut_ptr()) }
                .fail
            {
                // SAFETY: after a failed parse the parser holds its problem.
                return Err(unsafe { problem_text(self.inner.as_ptr()) });
            }
            // SAFETY: a successful parse initialized the event.
            let ev = unsafe { read_event(event.as_ptr()) };
            // SAFETY: the event is initialized and every string was copied out.
            unsafe { unsafe_sys::yaml_event_delete(event.as_mut_ptr()) };
            if let Some(ev) = ev {
                if matches!(ev, Ev::StreamEnd) {
                    self.done = true;
                }
                return Ok(ev);
            }
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
        Some(
            std::ffi::CStr::from_ptr(p.cast())
                .to_string_lossy()
                .into_owned(),
        )
    };
    match (*event).type_ {
        unsafe_sys::YAML_STREAM_START_EVENT => Some(Ev::StreamStart),
        unsafe_sys::YAML_STREAM_END_EVENT => Some(Ev::StreamEnd),
        unsafe_sys::YAML_DOCUMENT_START_EVENT => Some(Ev::DocStart),
        unsafe_sys::YAML_DOCUMENT_END_EVENT => Some(Ev::DocEnd),
        unsafe_sys::YAML_ALIAS_EVENT => Some(Ev::Alias(
            cstring((*event).data.alias.anchor).unwrap_or_default(),
        )),
        unsafe_sys::YAML_SCALAR_EVENT => {
            let s = (*event).data.scalar;
            Some(Ev::Scalar {
                anchor: cstring(s.anchor),
                tag: cstring(s.tag),
                value: string(s.value, s.length),
                implicit: s.plain_implicit,
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
    let parser: &unsafe_sys::yaml_parser_t = &*parser;
    let mark = parser.problem_mark;
    let problem = if parser.problem.is_null() {
        "parse error".to_string()
    } else {
        std::ffi::CStr::from_ptr(parser.problem.cast())
            .to_string_lossy()
            .into_owned()
    };
    // PyYAML writes a multi-line `MarkedYAMLError`; this is libyaml's one-line
    // problem plus the same position.
    format!(
        "{problem} at line {}, column {}",
        mark.line + 1,
        mark.column + 1
    )
}

// ── compose (PyYAML's C composer) ───────────────────────────────────

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

impl Node {
    /// PyYAML's `node.id`, as its constructor errors name it.
    fn id(&self) -> &'static str {
        match self {
            Node::Scalar { .. } => "scalar",
            Node::Seq { .. } => "sequence",
            Node::Map { .. } => "mapping",
        }
    }

    fn tag(&self) -> &str {
        match self {
            Node::Scalar { tag, .. } | Node::Seq { tag, .. } | Node::Map { tag, .. } => tag,
        }
    }
}

struct Composer<'a> {
    parser: Parser<'a>,
    peeked: Option<Ev>,
    /// Every anchor of the CURRENT document (PyYAML resets them per document).
    anchors: HashMap<String, Node>,
    /// Anchors of collections still being composed: an alias to one of them is
    /// a recursive structure.
    open: HashSet<String>,
}

impl<'a> Composer<'a> {
    fn new(text: &'a str) -> Result<Self, String> {
        Ok(Composer {
            parser: Parser::new(text)?,
            peeked: None,
            anchors: HashMap::new(),
            open: HashSet::new(),
        })
    }

    fn peek(&mut self) -> Result<&Ev, String> {
        if self.peeked.is_none() {
            self.peeked = Some(self.parser.next_event()?);
        }
        Ok(self.peeked.as_ref().expect("filled above"))
    }

    fn next(&mut self) -> Result<Ev, String> {
        match self.peeked.take() {
            Some(ev) => Ok(ev),
            None => self.parser.next_event(),
        }
    }

    fn skip_stream_start(&mut self) -> Result<(), String> {
        if matches!(self.peek()?, Ev::StreamStart) {
            self.next()?;
        }
        Ok(())
    }

    /// `DocumentStart`, the root node, `DocumentEnd`; anchors are per document.
    fn compose_document(&mut self) -> Result<Node, String> {
        match self.next()? {
            Ev::DocStart => {}
            other => return Err(format!("expected a document start, found {other:?}")),
        }
        let node = self.compose_node()?;
        match self.next()? {
            Ev::DocEnd => {}
            other => return Err(format!("expected a document end, found {other:?}")),
        }
        self.anchors.clear();
        self.open.clear();
        Ok(node)
    }

    /// PyYAML's composer refuses an anchor already defined in the document.
    fn claim_anchor(&self, anchor: Option<&String>) -> Result<(), String> {
        match anchor {
            Some(a) if self.anchors.contains_key(a) || self.open.contains(a) => {
                Err(format!("found duplicate anchor {}", py_repr(a)))
            }
            _ => Ok(()),
        }
    }

    fn compose_node(&mut self) -> Result<Node, String> {
        match self.next()? {
            Ev::Alias(a) => {
                if self.open.contains(&a) {
                    return Err(format!(
                        "found a recursive alias {} — a structure that contains itself \
                         has no JSON form",
                        py_repr(&a)
                    ));
                }
                self.anchors
                    .get(&a)
                    .cloned()
                    .ok_or_else(|| format!("found undefined alias {}", py_repr(&a)))
            }
            Ev::Scalar {
                anchor,
                tag,
                value,
                implicit,
            } => {
                self.claim_anchor(anchor.as_ref())?;
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolve_scalar(&value, implicit),
                };
                let node = Node::Scalar { tag, value };
                if let Some(a) = anchor {
                    self.anchors.insert(a, node.clone());
                }
                Ok(node)
            }
            Ev::SeqStart { anchor, tag } => {
                self.claim_anchor(anchor.as_ref())?;
                if let Some(a) = &anchor {
                    self.open.insert(a.clone());
                }
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => format!("{TAG_PREFIX}seq"),
                };
                let mut items = Vec::new();
                while !matches!(self.peek()?, Ev::SeqEnd) {
                    items.push(self.compose_node()?);
                }
                self.next()?;
                let node = Node::Seq { tag, items };
                if let Some(a) = anchor {
                    self.open.remove(&a);
                    self.anchors.insert(a, node.clone());
                }
                Ok(node)
            }
            Ev::MapStart { anchor, tag } => {
                self.claim_anchor(anchor.as_ref())?;
                if let Some(a) = &anchor {
                    self.open.insert(a.clone());
                }
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => format!("{TAG_PREFIX}map"),
                };
                let mut pairs = Vec::new();
                while !matches!(self.peek()?, Ev::MapEnd) {
                    let k = self.compose_node()?;
                    let v = self.compose_node()?;
                    pairs.push((k, v));
                }
                self.next()?;
                let node = Node::Map { tag, pairs };
                if let Some(a) = anchor {
                    self.open.remove(&a);
                    self.anchors.insert(a, node.clone());
                }
                Ok(node)
            }
            other => Err(format!("unexpected YAML event {other:?}")),
        }
    }
}

// ── resolve (PyYAML's Resolver, YAML 1.1) ───────────────────────────

/// `Resolver.resolve` for a scalar: when libyaml marks it implicit, the first
/// implicit resolver (in `resolver.py`'s registration order) whose pattern
/// matches names the tag; otherwise it is a string.
fn resolve_scalar(value: &str, implicit: bool) -> String {
    if implicit {
        for (name, re) in implicit_resolvers() {
            if re.is_match(value) {
                return format!("{TAG_PREFIX}{name}");
            }
        }
    }
    format!("{TAG_PREFIX}str")
}

/// PyYAML's implicit resolvers, verbatim and in registration order. Python's
/// `^…$` is `\A…\n?\z` here (python's `$` also matches before a final newline).
/// The first-character index PyYAML keeps only FILTERS: every pattern can only
/// match a value starting with one of its own registered characters.
fn implicit_resolvers() -> &'static [(&'static str, Regex)] {
    static R: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    R.get_or_init(|| {
        let re = |p: &str| Regex::new(&format!(r"\A(?:{p})\n?\z")).expect("PyYAML pattern");
        vec![
            (
                "bool",
                re("yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF"),
            ),
            (
                "float",
                re(concat!(
                    r"[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?",
                    r"|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
                    r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*",
                    r"|[-+]?\.(?:inf|Inf|INF)",
                    r"|\.(?:nan|NaN|NAN)"
                )),
            ),
            (
                "int",
                re(concat!(
                    r"[-+]?0b[0-1_]+",
                    r"|[-+]?0[0-7_]+",
                    r"|[-+]?(?:0|[1-9][0-9_]*)",
                    r"|[-+]?0x[0-9a-fA-F_]+",
                    r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+"
                )),
            ),
            ("merge", re("<<")),
            ("null", re("~|null|Null|NULL|")),
            (
                "timestamp",
                re(concat!(
                    r"[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]",
                    r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?",
                    r"(?:[Tt]|[ \t]+)[0-9][0-9]?",
                    r":[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?",
                    r"(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?"
                )),
            ),
            ("value", re("=")),
            // "only for documentation purposes" in PyYAML — a plain scalar cannot
            // start with these, but a `!`-tagged quoted one can.
            ("yaml", re(r"!|&|\*")),
        ]
    })
}

// ── construct (PyYAML's SafeConstructor) ────────────────────────────

/// A constructed python value, before it becomes JSON: the KEY of a mapping
/// needs python's types (a dict key `True` equals `1`; a date prints as text).
#[derive(Clone, Debug)]
enum Py {
    None,
    Bool(bool),
    /// An int of any width, as canonical decimal digits.
    Int(String),
    Float(f64),
    Str(String),
    /// `str()` of a `datetime.date`.
    Date(String),
    /// `str()` of a `datetime.datetime`, and — when it carries a tzinfo — its
    /// instant in UTC (aware datetimes compare by instant).
    DateTime {
        text: String,
        utc: Option<String>,
    },
    List(Vec<Py>),
    Dict(Vec<(Py, Py)>),
}

fn construct(node: &Node) -> Result<Py, String> {
    let tag = node.tag();
    let Some(kind) = tag.strip_prefix(TAG_PREFIX) else {
        return Err(undefined(tag));
    };
    match kind {
        "null" | "bool" | "int" | "float" | "str" | "timestamp" | "binary" => {
            let Node::Scalar { value, .. } = node else {
                return Err(format!("expected a scalar node, but found {}", node.id()));
            };
            match kind {
                "null" => Ok(Py::None),
                "bool" => construct_bool(value),
                "int" => yaml_int(value).map(|n| Py::Int(n.to_decimal())),
                "float" => yaml_float(value).map(Py::Float),
                "str" => Ok(Py::Str(value.clone())),
                "timestamp" => timestamp(value),
                _ => Err("a !!binary node has no JSON form (python builds bytes)".to_string()),
            }
        }
        "seq" => match node {
            Node::Seq { items, .. } => items
                .iter()
                .map(construct)
                .collect::<Result<_, _>>()
                .map(Py::List),
            other => Err(format!(
                "expected a sequence node, but found {}",
                other.id()
            )),
        },
        "map" => match node {
            Node::Map { pairs, .. } => construct_mapping(pairs),
            other => Err(format!("expected a mapping node, but found {}", other.id())),
        },
        "omap" | "pairs" => construct_pairs(node, kind),
        "set" => Err("a !!set node has no JSON form (python builds a set)".to_string()),
        _ => Err(undefined(tag)),
    }
}

fn undefined(tag: &str) -> String {
    format!(
        "could not determine a constructor for the tag {}",
        py_repr(tag)
    )
}

/// `construct_yaml_bool`: `bool_values[value.lower()]`, a `KeyError` otherwise.
fn construct_bool(value: &str) -> Result<Py, String> {
    match value.to_lowercase().as_str() {
        "yes" | "true" | "on" => Ok(Py::Bool(true)),
        "no" | "false" | "off" => Ok(Py::Bool(false)),
        other => Err(format!("not a bool: KeyError({})", py_repr(other))),
    }
}

/// `construct_yaml_omap` / `construct_yaml_pairs`: a sequence of one-pair
/// mappings → a list of `(key, value)` tuples (a JSON pair array).
fn construct_pairs(node: &Node, kind: &str) -> Result<Py, String> {
    let context = if kind == "omap" {
        "while constructing an ordered map"
    } else {
        "while constructing pairs"
    };
    let Node::Seq { items, .. } = node else {
        return Err(format!(
            "{context}: expected a sequence, but found {}",
            node.id()
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for sub in items {
        let Node::Map { pairs, .. } = sub else {
            return Err(format!(
                "{context}: expected a mapping of length 1, but found {}",
                sub.id()
            ));
        };
        if pairs.len() != 1 {
            return Err(format!(
                "{context}: expected a single mapping item, but found {} items",
                pairs.len()
            ));
        }
        let (k, v) = &pairs[0];
        out.push(Py::List(vec![construct(k)?, construct(v)?]));
    }
    Ok(Py::List(out))
}

/// `SafeConstructor.construct_mapping`: flatten the merges, then build a python
/// dict — an unhashable key refuses, and a key EQUAL to an earlier one keeps the
/// earlier key with the later value (dict assignment).
fn construct_mapping(pairs: &[(Node, Node)]) -> Result<Py, String> {
    let flat = flatten_mapping(pairs)?;
    let mut out: Vec<(KeyClass, Py, Py)> = Vec::with_capacity(flat.len());
    for (k, v) in &flat {
        let key = construct(k)?;
        let Some(class) = key_class(&key) else {
            return Err("while constructing a mapping: found unhashable key".to_string());
        };
        let value = construct(v)?;
        match out.iter_mut().find(|(c, _, _)| *c == class) {
            Some(slot) => slot.2 = value,
            None => out.push((class, key, value)),
        }
    }
    Ok(Py::Dict(out.into_iter().map(|(_, k, v)| (k, v)).collect()))
}

/// PyYAML's `flatten_mapping`: a `<<` key splices its mapping (or its list of
/// mappings, the LAST one first) in front of the mapping's own pairs, so an own
/// key overrides a merged one; a `=` KEY is a plain string key.
fn flatten_mapping(pairs: &[(Node, Node)]) -> Result<Vec<(Node, Node)>, String> {
    let merge_tag = format!("{TAG_PREFIX}merge");
    let value_tag = format!("{TAG_PREFIX}value");
    let mut merge: Vec<(Node, Node)> = Vec::new();
    let mut own: Vec<(Node, Node)> = Vec::new();
    for (k, v) in pairs {
        if k.tag() == merge_tag {
            match v {
                Node::Map { pairs, .. } => merge.extend(flatten_mapping(pairs)?),
                Node::Seq { items, .. } => {
                    let mut submerge: Vec<Vec<(Node, Node)>> = Vec::new();
                    for sub in items {
                        let Node::Map { pairs, .. } = sub else {
                            return Err(format!(
                                "while constructing a mapping: expected a mapping for \
                                 merging, but found {}",
                                sub.id()
                            ));
                        };
                        submerge.push(flatten_mapping(pairs)?);
                    }
                    for sub in submerge.into_iter().rev() {
                        merge.extend(sub);
                    }
                }
                Node::Scalar { .. } => {
                    return Err(format!(
                        "while constructing a mapping: expected a mapping or list of \
                         mappings for merging, but found {}",
                        v.id()
                    ))
                }
            }
        } else if k.tag() == value_tag {
            let Node::Scalar { value, .. } = k else {
                unreachable!("a resolved `value` tag is only ever on a scalar");
            };
            own.push((
                Node::Scalar {
                    tag: format!("{TAG_PREFIX}str"),
                    value: value.clone(),
                },
                v.clone(),
            ));
        } else {
            own.push((k.clone(), v.clone()));
        }
    }
    merge.extend(own);
    Ok(merge)
}

/// Which python dict keys are EQUAL: `True == 1 == 1.0`, `0 == -0.0`, a str by
/// its text, aware datetimes by their instant. `None` = unhashable (a list or a
/// dict as a key).
#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyClass {
    Num(String),
    Str(String),
    None,
    Date(String),
    NaiveDateTime(String),
    AwareDateTime(String),
}

fn key_class(key: &Py) -> Option<KeyClass> {
    Some(match key {
        Py::None => KeyClass::None,
        Py::Bool(b) => KeyClass::Num(if *b { "1" } else { "0" }.to_string()),
        Py::Int(d) => KeyClass::Num(d.clone()),
        Py::Float(f) => {
            if f.is_nan() {
                KeyClass::Num("nan".to_string())
            } else if f.is_finite() && f.fract() == 0.0 {
                // an integral float equals the int with its value
                if *f == 0.0 {
                    KeyClass::Num("0".to_string())
                } else {
                    KeyClass::Num(format!("{f:.0}"))
                }
            } else {
                KeyClass::Num(format!("f{}", py_float_repr(*f)))
            }
        }
        Py::Str(s) => KeyClass::Str(s.clone()),
        Py::Date(s) => KeyClass::Date(s.clone()),
        Py::DateTime { text, utc: None } => KeyClass::NaiveDateTime(text.clone()),
        Py::DateTime { utc: Some(u), .. } => KeyClass::AwareDateTime(u.clone()),
        Py::List(_) | Py::Dict(_) => return None,
    })
}

/// `str(key)` — how python prints a dict key that JSON must hold as text.
fn py_key_text(key: &Py) -> String {
    match key {
        Py::None => "None".to_string(),
        Py::Bool(true) => "True".to_string(),
        Py::Bool(false) => "False".to_string(),
        Py::Int(d) => d.clone(),
        Py::Float(f) => py_float_repr(*f),
        Py::Str(s) | Py::Date(s) | Py::DateTime { text: s, .. } => s.clone(),
        Py::List(_) | Py::Dict(_) => unreachable!("an unhashable key refused above"),
    }
}

fn to_json(v: Py) -> Result<Value, String> {
    Ok(match v {
        Py::None => Value::Null,
        Py::Bool(b) => Value::Bool(b),
        Py::Int(d) => Value::Number(d.parse::<Number>().map_err(|e| format!("int {d}: {e}"))?),
        Py::Float(f) => match Number::from_f64(f) {
            Some(n) => Value::Number(n),
            None => Value::String(py_float_repr(f)),
        },
        Py::Str(s) | Py::Date(s) | Py::DateTime { text: s, .. } => Value::String(s),
        Py::List(items) => Value::Array(items.into_iter().map(to_json).collect::<Result<_, _>>()?),
        Py::Dict(pairs) => {
            let mut m = Map::new();
            for (k, v) in pairs {
                m.insert(py_key_text(&k), to_json(v)?);
            }
            Value::Object(m)
        }
    })
}

// ── the scalar constructors ─────────────────────────────────────────

/// A python int (unbounded): sign + base-10⁹ little-endian limbs.
#[derive(Clone, Debug, Default)]
struct BigInt {
    neg: bool,
    mag: Vec<u32>,
}

const LIMB: u64 = 1_000_000_000;

impl BigInt {
    fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    /// `self = self * m + a` on the magnitude.
    fn mul_add(&mut self, m: u32, a: u32) {
        let mut carry = u64::from(a);
        for limb in &mut self.mag {
            let x = u64::from(*limb) * u64::from(m) + carry;
            *limb = (x % LIMB) as u32;
            carry = x / LIMB;
        }
        while carry > 0 {
            self.mag.push((carry % LIMB) as u32);
            carry /= LIMB;
        }
        self.trim();
    }

    fn trim(&mut self) {
        while self.mag.last() == Some(&0) {
            self.mag.pop();
        }
        if self.mag.is_empty() {
            self.neg = false;
        }
    }

    fn cmp_mag(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
        a.len()
            .cmp(&b.len())
            .then_with(|| a.iter().rev().cmp(b.iter().rev()))
    }

    /// Signed addition.
    fn add(&self, other: &BigInt) -> BigInt {
        let (big, small, neg) = if self.neg == other.neg {
            let mut out = Vec::with_capacity(self.mag.len().max(other.mag.len()) + 1);
            let mut carry = 0u64;
            for i in 0..self.mag.len().max(other.mag.len()) {
                let x = u64::from(*self.mag.get(i).unwrap_or(&0))
                    + u64::from(*other.mag.get(i).unwrap_or(&0))
                    + carry;
                out.push((x % LIMB) as u32);
                carry = x / LIMB;
            }
            if carry > 0 {
                out.push(carry as u32);
            }
            let mut r = BigInt {
                neg: self.neg,
                mag: out,
            };
            r.trim();
            return r;
        } else if Self::cmp_mag(&self.mag, &other.mag) != std::cmp::Ordering::Less {
            (&self.mag, &other.mag, self.neg)
        } else {
            (&other.mag, &self.mag, other.neg)
        };
        let mut out = Vec::with_capacity(big.len());
        let mut borrow = 0i64;
        for (i, &b) in big.iter().enumerate() {
            let mut x = i64::from(b) - i64::from(*small.get(i).unwrap_or(&0)) - borrow;
            borrow = 0;
            if x < 0 {
                x += LIMB as i64;
                borrow = 1;
            }
            out.push(x as u32);
        }
        let mut r = BigInt { neg, mag: out };
        r.trim();
        r
    }

    fn to_decimal(&self) -> String {
        let Some((last, rest)) = self.mag.split_last() else {
            return "0".to_string();
        };
        let mut s = String::new();
        if self.neg {
            s.push('-');
        }
        s.push_str(&last.to_string());
        for limb in rest.iter().rev() {
            s.push_str(&format!("{limb:09}"));
        }
        s
    }
}

/// python `int(s, base)` for the bases PyYAML uses: surrounding whitespace
/// stripped, an optional sign, the base's own prefix allowed (`int('0o17', 8)`
/// is 15), then at least one digit of that base.
fn int_radix(s: &str, base: u32) -> Result<BigInt, String> {
    let bad = || format!("invalid literal for int() with base {base}: {}", py_repr(s));
    let t = py_strip(s);
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let prefix = match base {
        2 => Some(["0b", "0B"]),
        8 => Some(["0o", "0O"]),
        16 => Some(["0x", "0X"]),
        _ => None,
    };
    let digits = prefix
        .and_then(|ps| ps.iter().find_map(|p| t.strip_prefix(p)))
        .unwrap_or(t);
    if digits.is_empty() {
        return Err(bad());
    }
    let mut n = BigInt::default();
    for c in digits.chars() {
        let d = c.to_digit(base).ok_or_else(bad)?;
        n.mul_add(base, d);
    }
    n.neg = neg;
    n.trim();
    Ok(n)
}

/// `construct_yaml_int`: `_` dropped, a sign, then `0`, `0b…`, `0x…`, a leading
/// `0` (octal), sexagesimal `a:b:c`, or decimal.
fn yaml_int(raw: &str) -> Result<BigInt, String> {
    let v = raw.replace('_', "");
    let neg = v.starts_with('-');
    let v = v.strip_prefix(['+', '-']).unwrap_or(&v).to_string();
    if v.is_empty() {
        return Err(format!("not an int: {}", py_repr(raw)));
    }
    let mut n = if v == "0" {
        BigInt::default()
    } else if let Some(bits) = v.strip_prefix("0b") {
        int_radix(bits, 2)?
    } else if let Some(hex) = v.strip_prefix("0x") {
        int_radix(hex, 16)?
    } else if v.starts_with('0') {
        int_radix(&v, 8)?
    } else if v.contains(':') {
        // python accumulates the parts reversed with a growing base; on exact
        // ints that is Horner's rule over the parts in order.
        let mut total = BigInt::default();
        for part in v.split(':') {
            let p = int_radix(part, 10)?;
            // `mul_add` scales the magnitude; the sign stays.
            total.mul_add(60, 0);
            total = total.add(&p);
        }
        total
    } else {
        int_radix(&v, 10)?
    };
    if neg && !n.is_zero() {
        n.neg = !n.neg;
    }
    Ok(n)
}

/// python `float(s)`: surrounding whitespace stripped; `inf`/`infinity`/`nan`
/// in any case, with a sign.
fn float_text(s: &str) -> Result<f64, String> {
    py_strip(s)
        .parse::<f64>()
        .map_err(|_| format!("could not convert string to float: {}", py_repr(s)))
}

/// `construct_yaml_float`: `_` dropped and lowercased, a sign, then `.inf`,
/// `.nan`, sexagesimal (python's own reversed accumulation, so the float
/// rounding is the same), or `float()`.
fn yaml_float(raw: &str) -> Result<f64, String> {
    let v = raw.replace('_', "").to_lowercase();
    if v.is_empty() {
        return Err(format!("not a float: {}", py_repr(raw)));
    }
    let sign = if v.starts_with('-') { -1.0 } else { 1.0 };
    let v = v.strip_prefix(['+', '-']).unwrap_or(&v);
    if v == ".inf" {
        return Ok(sign * f64::INFINITY);
    }
    if v == ".nan" {
        return Ok(f64::NAN);
    }
    if v.contains(':') {
        let mut digits = Vec::new();
        for part in v.split(':') {
            digits.push(float_text(part)?);
        }
        let mut base = 1.0f64;
        let mut value = 0.0f64;
        for d in digits.iter().rev() {
            value += d * base;
            base *= 60.0;
        }
        return Ok(sign * value);
    }
    Ok(sign * float_text(v)?)
}

/// PyYAML's constructor `timestamp_regexp` (looser than the resolver's: a
/// one-digit month/day is accepted without a time).
fn timestamp_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"\A(?P<year>[0-9][0-9][0-9][0-9])",
            r"-(?P<month>[0-9][0-9]?)",
            r"-(?P<day>[0-9][0-9]?)",
            r"(?:(?:[Tt]|[ \t]+)",
            r"(?P<hour>[0-9][0-9]?)",
            r":(?P<minute>[0-9][0-9])",
            r":(?P<second>[0-9][0-9])",
            r"(?:\.(?P<fraction>[0-9]*))?",
            r"(?:[ \t]*(?P<tz>Z|(?P<tz_sign>[-+])(?P<tz_hour>[0-9][0-9]?)",
            r"(?::(?P<tz_minute>[0-9][0-9]))?))?)?\n?\z"
        ))
        .expect("PyYAML timestamp pattern")
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Howard Hinnant's `days_from_civil` — days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `construct_yaml_timestamp`, then python's `str()` of the `date` or
/// `datetime` it builds — with python's own range checks, in its order (the
/// tzinfo is built before the datetime).
fn timestamp(value: &str) -> Result<Py, String> {
    let caps = timestamp_re()
        .captures(value)
        .ok_or_else(|| format!("not a timestamp: {}", py_repr(value)))?;
    let num = |name: &str| -> i64 {
        caps.name(name)
            .map_or(0, |m| m.as_str().parse::<i64>().unwrap_or(0))
    };
    let (year, month, day) = (num("year"), num("month"), num("day"));
    let check_date = || -> Result<(), String> {
        if !(1..=9999).contains(&year) {
            return Err(format!("year {year} is out of range"));
        }
        if !(1..=12).contains(&month) {
            return Err("month must be in 1..12".to_string());
        }
        if !(1..=days_in_month(year, month)).contains(&day) {
            return Err("day is out of range for month".to_string());
        }
        Ok(())
    };
    let date = format!("{year:04}-{month:02}-{day:02}");
    if caps.name("hour").is_none() {
        check_date()?;
        return Ok(Py::Date(date));
    }
    let (hour, minute, second) = (num("hour"), num("minute"), num("second"));
    let micros: i64 = match caps.name("fraction").map(|m| m.as_str()) {
        Some(f) if !f.is_empty() => {
            let mut f: String = f.chars().take(6).collect();
            while f.len() < 6 {
                f.push('0');
            }
            f.parse().unwrap_or(0)
        }
        _ => 0,
    };
    // tzinfo first: `datetime.timezone(delta)` refuses |delta| >= 24h.
    let offset_min: Option<i64> = if let Some(sign) = caps.name("tz_sign") {
        let m = num("tz_hour") * 60 + num("tz_minute");
        if m >= 24 * 60 {
            return Err(
                "offset must be a timedelta strictly between -timedelta(hours=24) and \
                 timedelta(hours=24)"
                    .to_string(),
            );
        }
        Some(if sign.as_str() == "-" { -m } else { m })
    } else if caps.name("tz").is_some() {
        Some(0)
    } else {
        None
    };
    check_date()?;
    if !(0..=23).contains(&hour) {
        return Err("hour must be in 0..23".to_string());
    }
    if !(0..=59).contains(&minute) {
        return Err("minute must be in 0..59".to_string());
    }
    if !(0..=59).contains(&second) {
        return Err("second must be in 0..59".to_string());
    }
    let mut text = format!("{date} {hour:02}:{minute:02}:{second:02}");
    if micros != 0 {
        text.push_str(&format!(".{micros:06}"));
    }
    let utc = offset_min.map(|off| {
        let sign = if off < 0 { '-' } else { '+' };
        let a = off.abs();
        text.push_str(&format!("{sign}{:02}:{:02}", a / 60, a % 60));
        let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - off * 60;
        format!("{secs}.{micros:06}")
    });
    Ok(Py::DateTime { text, utc })
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
