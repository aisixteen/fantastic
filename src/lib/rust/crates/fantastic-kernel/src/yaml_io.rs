//! The on-disk form of kernel-written state: YAML, block style, one emitter config.
//!
//! A rule-for-rule port of python's
//! `bundled_agents/loader/kernel_state/src/kernel_state/yaml_io.py`, which is
//! canonical — and BYTE-FOR-BYTE with it (the acceptance harness diffs files). The
//! python module's docstring is the FORMAT CONTRACT; the rules are restated here
//! where the code implements them, and pinned by the python-written fixture
//! `kernel_state/tests/fixtures/agent.yaml` (see `tests.rs`).
//!
//! Everything the kernel writes under `.fantastic` goes through [`dump`]: the
//! records (`agent.yaml`), `lock.yaml`, the scheduler's sidecars
//! (`schedules.yaml`, and `schedule_events.yaml` — a YAML STREAM, one `---`
//! document per event, via [`row_to_doc`]).
//!
//! There is no message tape: the kernel keeps no conversation, and its execution
//! rows live in its memory store (see `Kernel::flush_executions`).
//!
//! WHY A HAND-WRITTEN EMITTER. Python writes through libyaml (`CSafeDumper`); the
//! bytes are the contract, so the writer here re-implements the parts of libyaml's
//! emitter the format uses — block style, indent 2, indentless lists under a key,
//! no wrapping (`width=-1`), `allow_unicode`, `{}`/`[]` for empty collections,
//! libyaml's scalar ANALYSIS and STYLE SELECTION, and PyYAML's YAML 1.1 implicit
//! resolver (which decides when a string must be quoted). No yaml crate emits
//! libyaml's bytes over `serde_json::Value` with key order kept.
//!
//! READING goes through `yaml-rust2`'s event parser (pure rust, streams a `char`
//! iterator), with ONE resolver of ours for plain scalars — YAML 1.2 core, the
//! schema python's writer quotes against (`0o17`, `1e3`, `08` are quoted there
//! exactly because a 1.2 loader would read a number). yaml-rust2's own resolver
//! is NOT used: it reads `inf`/`nan`/`infinity` as floats (rust's `f64` parse),
//! which python writes as plain strings.

use std::sync::OnceLock;

use serde_json::{Map, Number, Value};
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};


/// What starts every stream document.
pub const SEPARATOR: &str = "---\n";

// ── write ──────────────────────────────────────────────────────────────────────

/// One YAML document (no `---`, no `...` after a collection) by the rules above.
#[must_use]
pub fn dump(v: &Value) -> String {
    let mut e = Emitter { out: String::new() };
    match v {
        Value::Object(m) if !m.is_empty() => e.block_mapping(m, 0, false),
        Value::Array(a) if !a.is_empty() => e.block_sequence(a, 0, false),
        Value::Object(_) => e.out.push_str("{}"),
        Value::Array(_) => e.out.push_str("[]"),
        scalar => {
            // A ROOT scalar (never a record or a row). libyaml marks a plain root
            // scalar open-ended and closes it with `...`.
            let (text, plain) = e.scalar_text(scalar, false, 2);
            e.out.push_str(&text);
            if plain {
                e.out.push_str("\n...");
            }
        }
    }
    e.line_start();
    e.out
}

/// One entry of a YAML stream as one document: `---\n` + the block mapping.
#[must_use]
pub fn row_to_doc(row: &Map<String, Value>) -> String {
    let mut s = String::from(SEPARATOR);
    s.push_str(&dump(&Value::Object(row.clone())));
    s
}

/// The string STYLE a scalar is written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    Plain,
    Single,
    Double,
    Literal,
}

struct Emitter {
    out: String,
}

impl Emitter {
    /// libyaml's `write_indent` reduced to what this format reaches: a break
    /// unless the output already sits at a line start.
    fn line_start(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn pad(&mut self, indent: usize) {
        for _ in 0..indent {
            self.out.push(' ');
        }
    }

    /// A non-empty block mapping whose keys sit at `indent`. `inline` = the first
    /// key continues the current line (after `- ` or `? `/`: `).
    fn block_mapping(&mut self, m: &Map<String, Value>, indent: usize, inline: bool) {
        for (i, (k, v)) in m.iter().enumerate() {
            if !(inline && i == 0) {
                self.line_start();
                self.pad(indent);
            }
            let key = Value::String(k.clone());
            // libyaml `check_simple_key`: a single-line scalar of ≤128 bytes.
            if !k.contains(is_break) && k.len() <= 128 {
                let (text, _) = self.scalar_text(&key, true, indent + 2);
                self.out.push_str(&text);
                self.out.push(':');
                self.value_after_indicator(v, indent, true);
            } else {
                self.out.push_str("? ");
                let (text, _) = self.scalar_text(&key, false, indent + 2);
                self.out.push_str(&text);
                self.line_start();
                self.pad(indent);
                self.out.push(':');
                self.value_after_indicator(v, indent, false);
            }
        }
    }

    /// The value after a `key:`. `simple` = the `:` closed a simple key (the value's
    /// block list is then INDENTLESS and a nested map starts on the next line);
    /// after a complex key's `:` both continue inline, indented by 2.
    fn value_after_indicator(&mut self, v: &Value, indent: usize, simple: bool) {
        match v {
            Value::Object(m) if !m.is_empty() => {
                if simple {
                    self.block_mapping(m, indent + 2, false);
                } else {
                    self.out.push(' ');
                    self.block_mapping(m, indent + 2, true);
                }
            }
            Value::Array(a) if !a.is_empty() => {
                if simple {
                    self.block_sequence(a, indent, false);
                } else {
                    self.out.push(' ');
                    self.block_sequence(a, indent + 2, true);
                }
            }
            Value::Object(_) => self.out.push_str(" {}"),
            Value::Array(_) => self.out.push_str(" []"),
            scalar => {
                let (text, _) = self.scalar_text(scalar, false, indent + 2);
                self.out.push(' ');
                self.out.push_str(&text);
            }
        }
    }

    /// A non-empty block sequence whose `-` sits at `indent`.
    fn block_sequence(&mut self, a: &[Value], indent: usize, inline: bool) {
        for (i, v) in a.iter().enumerate() {
            if !(inline && i == 0) {
                self.line_start();
                self.pad(indent);
            }
            self.out.push_str("- ");
            match v {
                Value::Object(m) if !m.is_empty() => self.block_mapping(m, indent + 2, true),
                Value::Array(inner) if !inner.is_empty() => {
                    self.block_sequence(inner, indent + 2, true);
                }
                Value::Object(_) => self.out.push_str("{}"),
                Value::Array(_) => self.out.push_str("[]"),
                scalar => {
                    let (text, _) = self.scalar_text(scalar, false, indent + 2);
                    self.out.push_str(&text);
                }
            }
        }
    }

    /// A scalar's text (`(text, was_plain)`). `content_indent` is where a literal
    /// block's lines sit.
    fn scalar_text(&self, v: &Value, simple_key: bool, content_indent: usize) -> (String, bool) {
        match v {
            Value::Null => ("null".to_string(), true),
            Value::Bool(b) => ((if *b { "true" } else { "false" }).to_string(), true),
            Value::Number(n) => (number(n), true),
            Value::String(s) => {
                let style = choose_style(s, simple_key);
                let text = match style {
                    Style::Plain => s.clone(),
                    Style::Single => single_quoted(s, content_indent),
                    Style::Double => double_quoted(s),
                    Style::Literal => literal(s, content_indent),
                };
                (text, style == Style::Plain)
            }
            // Collections never reach here.
            _ => (String::new(), false),
        }
    }
}

/// An int as its digits; a float as PyYAML's `represent_float` — `repr(f).lower()`,
/// and `.0` put before an exponent that has no dot (`1e+16` → `1.0e+16`).
fn number(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let raw = n.as_str();
    if !raw.contains(['.', 'e', 'E']) {
        // An int wider than 64 bits: python `int`, its exact digits.
        return raw.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.is_nan() {
        return ".nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf" } else { "-.inf" }.to_string();
    }
    let v = fantastic_bundle::py_float_repr(f).to_lowercase();
    if !v.contains('.') && v.contains('e') {
        v.replacen('e', ".0e", 1)
    } else {
        v
    }
}

/// libyaml `IS_BREAK`.
fn is_break(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// libyaml `IS_BLANKZ` (the end of the value counts as blank — the C string's NUL).
fn is_blankz(c: Option<char>) -> bool {
    match c {
        None => true,
        Some(c) => c == ' ' || c == '\t' || c == '\0' || is_break(c),
    }
}

/// libyaml `IS_PRINTABLE`: `\n`, `0x20..=0x7E`, `0xA0..=0xD7FF`, `0xE000..=0xFFFD`
/// minus the BOM. Everything outside the BMP is NOT printable (so it is escaped).
fn is_printable(c: char) -> bool {
    let u = c as u32;
    u == 0x0A
        || (0x20..=0x7E).contains(&u)
        || (0xA0..=0xD7FF).contains(&u)
        || ((0xE000..=0xFFFD).contains(&u) && u != 0xFEFF)
}

/// libyaml `yaml_emitter_analyze_scalar`'s verdicts (the fields this format reads).
struct Analysis {
    empty: bool,
    multiline: bool,
    block_plain_allowed: bool,
    single_quoted_allowed: bool,
    block_allowed: bool,
}

fn analyze(s: &str) -> Analysis {
    if s.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            block_plain_allowed: true,
            single_quoted_allowed: true,
            block_allowed: false,
        };
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut block_ind, mut line_breaks, mut special) = (false, false, false);
    let (mut leading_space, mut leading_break, mut trailing_space, mut trailing_break) =
        (false, false, false, false);
    let (mut break_space, mut space_break) = (false, false);
    let (mut prev_space, mut prev_break) = (false, false);
    if s.starts_with("---") || s.starts_with("...") {
        block_ind = true;
    }
    let mut preceded_ws = true;
    let n = chars.len();
    for (i, &c) in chars.iter().enumerate() {
        let followed_ws = is_blankz(chars.get(i + 1).copied());
        if i == 0 {
            if matches!(
                c,
                '#' | ','
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '&'
                    | '*'
                    | '!'
                    | '|'
                    | '>'
                    | '\''
                    | '"'
                    | '%'
                    | '@'
                    | '`'
            ) {
                block_ind = true;
            }
            if (c == '?' || c == ':') && followed_ws {
                block_ind = true;
            }
            if c == '-' && followed_ws {
                block_ind = true;
            }
        } else {
            if c == ':' && followed_ws {
                block_ind = true;
            }
            if c == '#' && preceded_ws {
                block_ind = true;
            }
        }
        if !is_printable(c) {
            special = true;
        }
        if is_break(c) {
            line_breaks = true;
        }
        if c == ' ' {
            if i == 0 {
                leading_space = true;
            }
            if i + 1 == n {
                trailing_space = true;
            }
            if prev_break {
                break_space = true;
            }
            prev_space = true;
            prev_break = false;
        } else if is_break(c) {
            if i == 0 {
                leading_break = true;
            }
            if i + 1 == n {
                trailing_break = true;
            }
            if prev_space {
                space_break = true;
            }
            prev_space = false;
            prev_break = true;
        } else {
            prev_space = false;
            prev_break = false;
        }
        preceded_ws = is_blankz(Some(c));
    }
    let mut a = Analysis {
        empty: false,
        multiline: line_breaks,
        block_plain_allowed: true,
        single_quoted_allowed: true,
        block_allowed: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        a.block_plain_allowed = false;
    }
    if trailing_space {
        a.block_allowed = false;
    }
    if break_space {
        a.block_plain_allowed = false;
        a.single_quoted_allowed = false;
    }
    if space_break || special {
        a.block_plain_allowed = false;
        a.single_quoted_allowed = false;
        a.block_allowed = false;
    }
    if line_breaks || block_ind {
        a.block_plain_allowed = false;
    }
    a
}

/// The ONE style choice python makes (`_represent_str`), then libyaml's
/// `select_scalar_style` downgrades.
fn choose_style(s: &str, simple_key: bool) -> Style {
    let requested = if s.contains('\n') {
        if s == "\n" || s.ends_with("\n\n") {
            Some(Style::Double)
        } else {
            Some(Style::Literal)
        }
    } else if core12_number().is_match(s) {
        Some(Style::Single)
    } else {
        None
    };
    let a = analyze(s);
    let mut style = match requested {
        None => {
            let plain_implicit = resolves_as_str_11(s);
            if !a.block_plain_allowed
                || (a.empty && simple_key)
                || !plain_implicit
                || (simple_key && (a.empty || a.multiline))
            {
                Style::Single
            } else {
                Style::Plain
            }
        }
        Some(st) => st,
    };
    if style == Style::Single && !a.single_quoted_allowed {
        style = Style::Double;
    }
    if style == Style::Literal && (!a.block_allowed || simple_key) {
        style = Style::Double;
    }
    style
}

fn re(cell: &'static OnceLock<regex::Regex>, pat: &str) -> &'static regex::Regex {
    cell.get_or_init(|| regex::Regex::new(pat).expect("static pattern"))
}

/// Python's own rule: a string the YAML 1.2 CORE schema would read as a number.
fn core12_number() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    re(
        &R,
        r"\A(?:[-+]?0o[0-7]+|[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?)\z",
    )
}

/// PyYAML's YAML 1.1 implicit resolvers (`yaml/resolver.py`): does a PLAIN
/// scalar with this text read back as a string? (bool, float, int, merge, null,
/// timestamp, value, and the documentation-only `yaml` one for `!`/`&`/`*`.)
fn resolves_as_str_11(s: &str) -> bool {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    let r = re(
        &R,
        concat!(
            r"\A(?:",
            // bool
            r"yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF",
            // float
            r"|[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?",
            r"|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
            r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*",
            r"|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
            // int
            r"|[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+",
            r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+",
            // merge, null, value, yaml
            r"|<<|~|null|Null|NULL|=|!|&|\*",
            // timestamp
            r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]",
            r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?",
            r":[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?",
            r")\z"
        ),
    );
    !s.is_empty() && !r.is_match(s)
}

/// libyaml `write_single_quoted` (no wrapping): `'` doubled; a line break other
/// than `\n` (U+2028/2029 — a `\n` never reaches this style) is written raw and
/// the next line is indented.
fn single_quoted(s: &str, indent: usize) -> String {
    let mut out = String::from("'");
    let mut breaks = false;
    for c in s.chars() {
        if is_break(c) {
            if !breaks && c == '\n' {
                out.push('\n');
            }
            out.push(c);
            breaks = true;
        } else {
            if breaks {
                out.push_str(&" ".repeat(indent));
            }
            if c == '\'' {
                out.push_str("''");
            } else {
                out.push(c);
            }
            breaks = false;
        }
    }
    out.push('\'');
    out
}

/// libyaml `write_double_quoted` with `allow_unicode` and no wrapping.
fn double_quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        let u = c as u32;
        if !is_printable(c) || u == 0xFEFF || is_break(c) || c == '"' || c == '\\' {
            out.push('\\');
            match c {
                '\0' => out.push('0'),
                '\u{07}' => out.push('a'),
                '\u{08}' => out.push('b'),
                '\t' => out.push('t'),
                '\n' => out.push('n'),
                '\u{0B}' => out.push('v'),
                '\u{0C}' => out.push('f'),
                '\r' => out.push('r'),
                '\u{1B}' => out.push('e'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '\u{85}' => out.push('N'),
                '\u{A0}' => out.push('_'),
                '\u{2028}' => out.push('L'),
                '\u{2029}' => out.push('P'),
                _ if u <= 0xFF => out.push_str(&format!("x{u:02X}")),
                _ if u <= 0xFFFF => out.push_str(&format!("u{u:04X}")),
                _ => out.push_str(&format!("U{u:08X}")),
            }
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// libyaml `write_literal_scalar` + `write_block_scalar_hints`: `|`, an indent
/// hint when the text opens with a space or a break, `-` when it does not end in
/// a break (`+` is never reached: python requests `"` for a trailing blank line).
fn literal(s: &str, indent: usize) -> String {
    let mut out = String::from("|");
    let first = s.chars().next();
    if first.is_some_and(|c| c == ' ' || is_break(c)) {
        out.push('2');
    }
    let mut rev = s.chars().rev();
    match rev.next() {
        None => out.push('-'),
        Some(last) if !is_break(last) => out.push('-'),
        Some(_) => match rev.next() {
            None => out.push('+'),
            Some(prev) if is_break(prev) => out.push('+'),
            Some(_) => {}
        },
    }
    out.push('\n');
    let mut breaks = true;
    for c in s.chars() {
        if is_break(c) {
            out.push(c);
            breaks = true;
        } else {
            if breaks {
                out.push_str(&" ".repeat(indent));
            }
            out.push(c);
            breaks = false;
        }
    }
    // The literal's own final break is the line start the next node needs; a
    // `|-` block ends mid-line and the caller breaks.
    out
}

// ── read ───────────────────────────────────────────────────────────────────────

/// YAML 1.2 CORE resolution of a PLAIN scalar (see the module doc for why not
/// yaml-rust2's).
fn resolve_plain(s: &str) -> Result<Value, String> {
    static INT: OnceLock<regex::Regex> = OnceLock::new();
    static FLOAT: OnceLock<regex::Regex> = OnceLock::new();
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => {
            return Err(format!(
                "the float {s} has no JSON form — a record or row cannot hold it"
            ))
        }
        _ => {}
    }
    if let Some(oct) = s.strip_prefix("0o") {
        if !oct.is_empty() && oct.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return i64::from_str_radix(oct, 8)
                .map(Value::from)
                .map_err(|e| format!("{s}: {e}"));
        }
    }
    if let Some(hex) = s.strip_prefix("0x") {
        if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return i64::from_str_radix(hex, 16)
                .map(Value::from)
                .map_err(|e| format!("{s}: {e}"));
        }
    }
    if re(&INT, r"\A[-+]?[0-9]+\z").is_match(s) {
        let digits = s.strip_prefix('+').unwrap_or(s);
        return digits
            .parse::<Number>()
            .map(Value::Number)
            .map_err(|e| format!("{s}: {e}"));
    }
    if re(
        &FLOAT,
        r"\A[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?\z",
    )
    .is_match(s)
    {
        let f: f64 = s.parse().map_err(|e| format!("{s}: {e}"))?;
        return Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| format!("the float {s} has no JSON form"));
    }
    Ok(Value::String(s.to_string()))
}

/// Builds values from parser events — one document at a time.
enum Frame {
    Seq(Vec<Value>),
    Map(Map<String, Value>, Option<String>),
}

/// A document's value, or an error with where it happened.
enum DocError {
    Scan(Marker, String),
    Shape(String),
}

/// Pull ONE document's value off the parser (after its `DocumentStart`), through
/// its `DocumentEnd`.
fn read_document<T: Iterator<Item = char>>(p: &mut Parser<T>) -> Result<Value, DocError> {
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        let (ev, mark) = p
            .next_token()
            .map_err(|e| DocError::Scan(*e.marker(), e.info().to_string()))?;
        let done: Option<Value> = match ev {
            Event::Scalar(v, style, _, tag) => {
                if tag.is_some() {
                    return Err(DocError::Shape(
                        "a tagged node — tags are not part of this format".to_string(),
                    ));
                }
                let key_slot = matches!(stack.last(), Some(Frame::Map(_, None)));
                if key_slot {
                    // A map KEY is its text (keys are strings, as on the JSON wire).
                    if let Some(Frame::Map(_, k)) = stack.last_mut() {
                        *k = Some(v);
                    }
                    None
                } else if style == TScalarStyle::Plain {
                    Some(resolve_plain(&v).map_err(DocError::Shape)?)
                } else {
                    Some(Value::String(v))
                }
            }
            Event::SequenceStart(_, tag) | Event::MappingStart(_, tag) if tag.is_some() => {
                return Err(DocError::Shape(
                    "a tagged node — tags are not part of this format".to_string(),
                ))
            }
            Event::SequenceStart(..) => {
                if matches!(stack.last(), Some(Frame::Map(_, None))) {
                    return Err(DocError::Shape("a non-scalar map key".to_string()));
                }
                stack.push(Frame::Seq(Vec::new()));
                None
            }
            Event::MappingStart(..) => {
                if matches!(stack.last(), Some(Frame::Map(_, None))) {
                    return Err(DocError::Shape("a non-scalar map key".to_string()));
                }
                stack.push(Frame::Map(Map::new(), None));
                None
            }
            Event::SequenceEnd => match stack.pop() {
                Some(Frame::Seq(items)) => Some(Value::Array(items)),
                _ => return Err(DocError::Shape("unbalanced sequence".to_string())),
            },
            Event::MappingEnd => match stack.pop() {
                Some(Frame::Map(m, _)) => Some(Value::Object(m)),
                _ => return Err(DocError::Shape("unbalanced mapping".to_string())),
            },
            Event::Alias(_) => {
                return Err(DocError::Shape(
                    "an alias — aliases are not part of this format".to_string(),
                ))
            }
            Event::DocumentEnd => {
                return Err(DocError::Scan(mark, "an empty document".to_string()));
            }
            other => {
                return Err(DocError::Scan(mark, format!("unexpected event {other:?}")));
            }
        };
        let Some(v) = done else { continue };
        match stack.last_mut() {
            None => {
                // The document's root is complete; its end must follow.
                let (ev, mark) = p
                    .next_token()
                    .map_err(|e| DocError::Scan(*e.marker(), e.info().to_string()))?;
                if ev != Event::DocumentEnd {
                    return Err(DocError::Scan(
                        mark,
                        "expected the document end".to_string(),
                    ));
                }
                return Ok(v);
            }
            Some(Frame::Seq(items)) => items.push(v),
            Some(Frame::Map(m, k)) => {
                let key = k.take().unwrap_or_default();
                m.insert(key, v);
            }
        }
    }
}

/// Every document of a stream, in order (python `yaml.load_all`).
pub fn load_all(text: &str) -> Result<Vec<Value>, String> {
    let mut p = Parser::new(text.chars());
    let mut out = Vec::new();
    loop {
        let (ev, mark) = p.next_token().map_err(|e| e.to_string())?;
        match ev {
            Event::StreamStart => {}
            Event::StreamEnd => return Ok(out),
            Event::DocumentStart => match read_document(&mut p) {
                Ok(v) => out.push(v),
                Err(DocError::Scan(m, info)) => {
                    return Err(format!(
                        "{info} at line {} column {}",
                        m.line(),
                        m.col() + 1
                    ))
                }
                Err(DocError::Shape(s)) => return Err(s),
            },
            other => {
                return Err(format!(
                    "unexpected event {other:?} at line {} column {}",
                    mark.line(),
                    mark.col() + 1
                ))
            }
        }
    }
}

/// One YAML document back (python `yaml.load`): an empty text is `null`, a
/// stream of several documents is an error.
pub fn load(text: &str) -> Result<Value, String> {
    let mut docs = load_all(text)?;
    match docs.len() {
        0 => Ok(Value::Null),
        1 => Ok(docs.remove(0)),
        _ => Err("expected a single document in the stream".to_string()),
    }
}

#[cfg(test)]
#[path = "yaml_io/tests.rs"]
mod tests;
