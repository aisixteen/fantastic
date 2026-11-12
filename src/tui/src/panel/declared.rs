//! THE DECLARED PANEL: an agent whose record carries `view.tui` gets this one
//! panel, drawn from DATA (the bundle's `bundle.yaml`, `record.view.tui`), with
//! no Rust written for the agent. The declaration is a list of sections:
//!
//! ```yaml
//! tui:
//!   - {title: Status, call: {verb: status}, show: kv, fields: [state, {path: load.1m, label: load}]}
//!   - {title: Jobs, call: {verb: list, args: {limit: 20}}, show: table, every_s: 5}
//! ```
//!
//! Each section renders the reply of ONE verb call. Reads only: the panel sends
//! the declared verbs (on select, on `r`, and every `every_s` while it is the
//! selected panel) and `reflect`, nothing else. A refusal shows as the kernel's
//! `{error, reason}` in the section that asked.

use std::collections::VecDeque;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::generic::GenericPanel;
use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

/// At most this many body lines drawn per section.
pub const MAX_LINES: usize = 200;

/// How a section draws its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Show {
    /// Key and value per line.
    Kv,
    /// Rows of the reply's list.
    Table,
    /// One line per item.
    List,
    /// A growing log.
    Log,
    /// The reply as text.
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub path: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub title: String,
    pub verb: String,
    pub args: Option<Map<String, Value>>,
    pub show: Show,
    pub fields: Vec<Field>,
    pub every_s: Option<f64>,
}

/// The verb a section calls, and its arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CallDecl {
    /// The verb to call.
    #[schemars(length(min = 1))]
    verb: String,
    /// Its arguments.
    args: Option<Map<String, Value>>,
}

/// One reply field a section shows.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum FieldDecl {
    /// The dotted path, labelled by itself.
    Path(#[schemars(length(min = 1))] String),
    /// The dotted path and its label.
    Labelled(LabelledDecl),
}

/// A field with its own label.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LabelledDecl {
    /// The dotted path in the reply.
    #[schemars(length(min = 1))]
    path: String,
    /// The label; absent, the path.
    label: Option<String>,
}

/// One section of `view.tui`: the reply of one verb call, drawn one way.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SectionDecl {
    /// The section's heading.
    #[schemars(length(min = 1), regex(pattern = "\\S"))]
    title: String,
    /// The call whose reply the section draws.
    call: CallDecl,
    /// How the reply is drawn.
    show: Show,
    /// The reply fields to show, in order.
    fields: Option<Vec<FieldDecl>>,
    /// Ask again every this many seconds while the panel is selected.
    #[schemars(range(min = 1, max = 3600))]
    every_s: Option<f64>,
}

/// The `view.tui` declaration: the list of sections (at least one, at most 16).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TuiDecl {
    /// The sections, in order.
    #[schemars(length(min = 1, max = 16))]
    tui: Vec<Value>,
}

static TUI: fantastic_kernel::RecordDecl = fantastic_kernel::RecordDecl::of::<TuiDecl>(false);
static SECTION: fantastic_kernel::RecordDecl = fantastic_kernel::RecordDecl::of::<SectionDecl>(false);

/// The sections `view.tui` declares, or the refusal: the shared engine checks
/// the declaration against the schemas above, then the checked value is typed.
pub fn parse(view: &Value) -> Result<Vec<Section>, Value> {
    // the declaration is checked as it stands: a view with no `tui` is refused by the
    // engine as missing it, not read as an empty value
    let mut whole = serde_json::Map::new();
    if let Some(tui) = view.get("tui") {
        whole.insert("tui".to_string(), tui.clone());
    }
    parse_whole(&Value::Object(whole))
}

/// Does `view` declare a TUI view that parses?
pub fn declared(view: &Value) -> bool {
    parse(view).is_ok()
}

pub fn parse_tui(tui: &Value) -> Result<Vec<Section>, Value> {
    parse_whole(&json!({"tui": tui}))
}

/// The checked declaration `{tui: [...]}`, typed.
fn parse_whole(whole: &Value) -> Result<Vec<Section>, Value> {
    fantastic_kernel::verbs::validate("view", TUI.schema(), whole)?;
    let tui = &whole["tui"];
    let list = tui.as_array().expect("the declaration's schema made it a list");
    list.iter().enumerate().map(|(i, s)| section(i, s)).collect()
}

fn section(i: usize, s: &Value) -> Result<Section, Value> {
    let at = format!("view.tui[{i}]");
    fantastic_kernel::verbs::validate(&at, SECTION.schema(), s)?;
    let d: SectionDecl = serde_json::from_value(s.clone())
        .map_err(|e| fantastic_kernel::verbs::unparsed(&at, &e.to_string()))?;
    let fields = d
        .fields
        .unwrap_or_default()
        .into_iter()
        .map(|f| match f {
            FieldDecl::Path(p) => Field { label: p.clone(), path: p },
            FieldDecl::Labelled(l) => Field { label: l.label.unwrap_or_else(|| l.path.clone()), path: l.path },
        })
        .collect();
    Ok(Section { title: d.title, verb: d.call.verb, args: d.call.args, show: d.show, fields, every_s: d.every_s })
}

/// The value at a dotted path; a numeric segment indexes an array.
pub fn pick<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match cur {
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            Value::Object(o) => o.get(seg)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// What a section shows now.
#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    Pending,
    Reply(Value),
    Refused {
        error: String,
        reason: Option<String>,
    },
}

fn val(v: Option<&Value>) -> String {
    match v {
        None => "-".into(),
        Some(v) => ro::brief(v, 120),
    }
}

/// The rows a table / list / log draws: the reply if an array, else its first
/// array-valued key.
fn items(reply: &Value) -> Vec<&Value> {
    match reply {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o
            .values()
            .find_map(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn is_scalar(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::Object(_))
}

/// One section's body lines (its heading is the caller's).
pub fn render_section(sec: &Section, slot: &Slot) -> Vec<Line<'static>> {
    let reply = match slot {
        Slot::Pending => return vec![ro::dim("    loading...".into())],
        Slot::Refused { error, reason } => {
            let s = match reason {
                Some(r) => format!("    refused: {error} ({r})"),
                None => format!("    refused: {error}"),
            };
            return vec![ro::bad(s)];
        }
        Slot::Reply(r) => r,
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    match sec.show {
        Show::Kv => {
            if sec.fields.is_empty() {
                if let Some(o) = reply.as_object() {
                    for (k, v) in o.iter().filter(|(_, v)| is_scalar(v)) {
                        out.push(ro::plain(format!("    {k}  {}", val(Some(v)))));
                    }
                } else {
                    out.push(ro::plain(format!("    {}", val(Some(reply)))));
                }
            } else {
                for f in &sec.fields {
                    out.push(ro::plain(format!(
                        "    {}  {}",
                        f.label,
                        val(pick(reply, &f.path))
                    )));
                }
            }
        }
        Show::Table => {
            let rows = items(reply);
            let cols: Vec<Field> = if !sec.fields.is_empty() {
                sec.fields.clone()
            } else if let Some(o) = rows.first().and_then(|r| r.as_object()) {
                o.keys()
                    .map(|k| Field {
                        path: k.clone(),
                        label: k.clone(),
                    })
                    .collect()
            } else {
                vec![Field {
                    path: String::new(),
                    label: "value".into(),
                }]
            };
            let cell = |r: &Value, c: &Field| -> String {
                if c.path.is_empty() {
                    val(Some(r))
                } else {
                    val(pick(r, &c.path))
                }
            };
            let body: Vec<Vec<String>> = rows
                .iter()
                .map(|r| cols.iter().map(|c| cell(r, c)).collect())
                .collect();
            let width: Vec<usize> = cols
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    body.iter()
                        .map(|r| r[i].chars().count())
                        .chain([c.label.chars().count()])
                        .max()
                        .unwrap_or(0)
                })
                .collect();
            let line = |cells: Vec<&str>| -> String {
                let mut s = String::from("    ");
                for (i, c) in cells.iter().enumerate() {
                    s.push_str(&format!("{c:<w$}  ", w = width[i]));
                }
                s.trim_end().to_string()
            };
            out.push(ro::dim(line(
                cols.iter().map(|c| c.label.as_str()).collect(),
            )));
            if body.is_empty() {
                out.push(ro::dim("    (no rows)".into()));
            }
            for r in &body {
                out.push(ro::plain(line(r.iter().map(String::as_str).collect())));
            }
        }
        Show::List | Show::Log => {
            let rows = items(reply);
            if rows.is_empty() {
                out.push(ro::dim("    (empty)".into()));
            }
            for r in rows {
                let s = match r {
                    Value::String(s) => s.clone(),
                    Value::Object(_) => match sec.fields.first() {
                        Some(f) => val(pick(r, &f.path)),
                        None => ro::brief(r, 120),
                    },
                    other => ro::brief(other, 120),
                };
                out.push(ro::plain(format!("    {}", s.replace('\n', " "))));
            }
        }
        Show::Text => {
            let text = if let Value::String(s) = reply {
                s.clone()
            } else if let Some(f) = sec.fields.first() {
                val(pick(reply, &f.path))
            } else if let Some(t) = reply.get("text").and_then(Value::as_str) {
                t.to_string()
            } else {
                ro::brief(reply, 400)
            };
            for l in text.lines() {
                out.push(ro::plain(format!("    {l}")));
            }
            if out.is_empty() {
                out.push(ro::dim("    (empty)".into()));
            }
        }
    }
    if out.len() > MAX_LINES {
        let more = out.len() - MAX_LINES;
        out.truncate(MAX_LINES);
        out.push(ro::dim(format!("    … {more} more")));
    }
    out
}

pub struct DeclaredPanel {
    agent: AgentId,
    pub sections: Vec<Section>,
    pub slots: Vec<Slot>,
    /// Section indices asked and not yet answered, in send order: replies come
    /// back in that order, so the front one is whose answer arrived.
    pub inflight: VecDeque<usize>,
    last_ask: Vec<f64>,
    generic: GenericPanel,
    pub show_generic: bool,
    scroll: u16,
    /// The scroll stays on the newest line until the human scrolls up.
    pinned: bool,
    bottom: u16,
    pub decl_error: Option<String>,
}

impl DeclaredPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            generic: GenericPanel::new(agent.clone()),
            agent,
            sections: Vec::new(),
            slots: Vec::new(),
            inflight: VecDeque::new(),
            last_ask: Vec::new(),
            show_generic: false,
            scroll: 0,
            pinned: true,
            bottom: 0,
            decl_error: None,
        }
    }

    /// Take the record's `view`: a good declaration replaces the sections (the
    /// slots of an unchanged index stay), a bad one is kept as the refusal.
    pub fn set_decl(&mut self, view: &Value) {
        match parse(view) {
            Ok(secs) => {
                self.decl_error = None;
                let old = std::mem::take(&mut self.slots);
                let old_secs = std::mem::take(&mut self.sections);
                self.slots = secs
                    .iter()
                    .enumerate()
                    .map(|(i, s)| match (old.get(i), old_secs.get(i)) {
                        (Some(slot), Some(prev)) if prev == s => slot.clone(),
                        _ => Slot::Pending,
                    })
                    .collect();
                self.last_ask.resize(secs.len(), f64::NEG_INFINITY);
                self.last_ask.truncate(secs.len());
                self.inflight.retain(|i| *i < secs.len());
                self.sections = secs;
            }
            Err(r) => {
                let error = fantastic_kernel::error_text(&r).unwrap_or_default();
                self.decl_error = Some(format!("{error} ({})", r["reason"].as_str().unwrap_or("")));
            }
        }
    }

    fn ask(&mut self, i: usize, now: f64, out: &mut Vec<Cmd>) {
        let sec = &self.sections[i];
        let mut payload = sec.args.clone().unwrap_or_default();
        payload.insert("type".into(), json!(sec.verb));
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: Value::Object(payload),
        });
        self.inflight.push_back(i);
        self.last_ask[i] = now;
    }

    fn ask_all(&mut self, out: &mut Vec<Cmd>) {
        self.inflight.clear();
        let now = ro::now_secs();
        for i in 0..self.sections.len() {
            self.ask(i, now, out);
        }
    }

    fn generic_view(&self) -> bool {
        self.show_generic || self.decl_error.is_some()
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (sec, slot) in self.sections.iter().zip(&self.slots) {
            out.push(ro::head(&sec.title));
            out.extend(render_section(sec, slot));
            out.push(Line::default());
        }
        out
    }

    fn has_log(&self) -> bool {
        self.sections.iter().any(|s| s.show == Show::Log)
    }

    fn effective_scroll(&self) -> u16 {
        if self.pinned && self.has_log() {
            self.bottom
        } else {
            self.scroll
        }
    }
}

impl Panel for DeclaredPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Declared
    }

    fn needs(&self) -> Needs {
        Needs {
            rows: false,
            reflect: true,
            watch: false,
        }
    }

    fn on_key(
        &mut self,
        k: crossterm::event::KeyEvent,
        cx: &Cx<'_>,
        out: &mut Vec<Cmd>,
    ) -> Handled {
        use crossterm::event::KeyCode as K;
        if self.decl_error.is_none() && k.code == K::Char('g') {
            self.show_generic = !self.show_generic;
            return Handled::Yes;
        }
        if self.generic_view() {
            return self.generic.on_key(k, cx, out);
        }
        if k.code == K::Char('r') {
            self.ask_all(out);
            return Handled::Yes;
        }
        let mut cursor = 0usize;
        let mut scroll = self.effective_scroll();
        if ro::nav(k, &mut cursor, &mut scroll, 0) {
            // `nav` moves the cursor for j/k; here those scroll a line
            match k.code {
                K::Char('j') | K::Down => scroll = scroll.saturating_add(1),
                K::Char('k') | K::Up => scroll = scroll.saturating_sub(1),
                _ => {}
            }
            self.scroll = scroll.min(self.bottom);
            self.pinned = self.has_log() && self.scroll >= self.bottom;
            return Handled::Yes;
        }
        Handled::No
    }

    fn on_event(&mut self, _payload: &Value, _out: &mut Vec<Cmd>) -> bool {
        false
    }

    fn on_select(&mut self, cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        if self.decl_error.is_some() {
            self.generic.on_select(cx, &mut Vec::new());
            return;
        }
        self.ask_all(out);
    }

    fn on_reply(&mut self, reply: &Value, out: &mut Vec<Cmd>) -> bool {
        let reflect_shaped = reply.get("verbs").is_some() && reply.get("sentence").is_some();
        if reflect_shaped
            || self.decl_error.is_some()
            || (self.show_generic && self.inflight.is_empty())
        {
            return self.generic.on_reply(reply, out);
        }
        let Some(i) = self.inflight.pop_front() else {
            return false;
        };
        if let Some(slot) = self.slots.get_mut(i) {
            *slot = match fantastic_kernel::error_text(reply) {
                Some(error) => Slot::Refused {
                    error,
                    reason: reply
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
                None => Slot::Reply(reply.clone()),
            };
        }
        true
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn layout(&mut self, area: Rect, _cx: &Cx<'_>) {
        let total = self.lines().len();
        self.bottom = total
            .saturating_sub(area.height as usize)
            .min(u16::MAX as usize) as u16;
        if self.pinned && self.has_log() {
            self.scroll = self.bottom;
        } else {
            self.scroll = self.scroll.min(self.bottom);
        }
    }

    fn tick(&mut self, now: f64, out: &mut Vec<Cmd>) {
        if self.decl_error.is_some() {
            return;
        }
        for i in 0..self.sections.len() {
            let Some(every) = self.sections[i].every_s else {
                continue;
            };
            if now - self.last_ask[i] >= every && !self.inflight.contains(&i) {
                self.ask(i, now, out);
            }
        }
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, cx: &Cx<'_>) {
        if let Some(e) = &self.decl_error {
            if area.height > 1 {
                let top = Rect { height: 1, ..area };
                let rest = Rect {
                    y: area.y + 1,
                    height: area.height - 1,
                    ..area
                };
                ro::draw(f, top, vec![ro::bad(format!("  refused: {e}"))], 0);
                self.generic.render(f, rest, cx);
            }
            return;
        }
        if self.show_generic {
            if area.height > 1 {
                let top = Rect { height: 1, ..area };
                let rest = Rect {
                    y: area.y + 1,
                    height: area.height - 1,
                    ..area
                };
                ro::draw(f, top, vec![ro::dim("  g: declared view".into())], 0);
                self.generic.render(f, rest, cx);
            }
            return;
        }
        ro::draw(f, area, self.lines(), self.effective_scroll());
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(format!("{} · declared · g generic", self.agent))
    }
}

#[cfg(test)]
mod tests {
    use super::ro::testing::*;
    use super::*;

    fn sec(show: &str, fields: Value) -> Section {
        parse_tui(&json!([{"title": "T", "call": {"verb": "v"}, "show": show, "fields": fields}]))
            .unwrap()
            .remove(0)
    }

    fn body(sec: &Section, slot: Slot) -> String {
        text(&render_section(sec, &slot))
    }

    fn refusal_of(tui: Value) -> Value {
        parse_tui(&tui).expect_err("refused")
    }

    fn err(refusal: &Value) -> String {
        fantastic_kernel::error_text(refusal).unwrap_or_default()
    }

    fn panel_with(view: Value) -> DeclaredPanel {
        let mut p = DeclaredPanel::new("a".into());
        p.set_decl(&view);
        p
    }

    fn two() -> Value {
        json!({"tui": [
            {"title": "One", "call": {"verb": "one", "args": {"n": 1}}, "show": "kv"},
            {"title": "Two", "call": {"verb": "two"}, "show": "text", "every_s": 5},
        ]})
    }

    fn select(p: &mut DeclaredPanel) -> Vec<Cmd> {
        let mut out = Vec::new();
        with_cx("a", None, |cx| p.on_select(cx, &mut out));
        out
    }

    #[test]
    fn a_valid_tui_declaration_parses_every_show_kind() {
        let v = json!({"kind": "html", "tui": [
            {"title": "A", "call": {"verb": "status"}, "show": "kv", "fields": ["x", {"path": "a.b", "label": "ab"}, {"path": "c"}]},
            {"title": "B", "call": {"verb": "list", "args": {"limit": 3}}, "show": "table", "every_s": 5},
            {"title": "C", "call": {"verb": "c"}, "show": "list"},
            {"title": "D", "call": {"verb": "d"}, "show": "log", "every_s": 2.5},
            {"title": "E", "call": {"verb": "e"}, "show": "text"},
        ]});
        assert!(declared(&v));
        let s = parse(&v).unwrap();
        assert_eq!(s.len(), 5);
        assert_eq!(
            s[0],
            Section {
                title: "A".into(),
                verb: "status".into(),
                args: None,
                show: Show::Kv,
                fields: vec![
                    Field {
                        path: "x".into(),
                        label: "x".into()
                    },
                    Field {
                        path: "a.b".into(),
                        label: "ab".into()
                    },
                    Field {
                        path: "c".into(),
                        label: "c".into()
                    },
                ],
                every_s: None,
            }
        );
        assert_eq!(s[1].show, Show::Table);
        assert_eq!(s[1].args.as_ref().unwrap()["limit"], json!(3));
        assert_eq!(s[1].every_s, Some(5.0));
        assert_eq!(
            [s[2].show, s[3].show, s[4].show],
            [Show::List, Show::Log, Show::Text]
        );
        assert_eq!(s[3].every_s, Some(2.5));
    }

    #[test]
    fn tui_must_be_a_non_empty_array_of_sections() {
        let one = json!({"title": "T", "call": {"verb": "v"}, "show": "kv"});
        for (tui, want) in [
            (json!({}), "tui"),
            (json!("x"), "tui"),
            (json!([]), "tui"),
            (json!([1]), "view.tui[0]"),
            (json!([one.clone(), {"title": ""}]), "view.tui[1]"),
        ] {
            let r = refusal_of(tui.clone());
            assert!(r["reason"].is_string(), "{tui}");
            assert!(err(&r).contains(want), "{tui}: {r}");
        }
        assert!(parse_tui(&json!(vec![one; 16])).is_ok());
        assert!(!declared(&json!({"kind": "html"})));
    }

    #[test]
    fn the_declared_cap_is_sixteen_sections() {
        let one = json!({"title": "T", "call": {"verb": "v"}, "show": "kv"});
        assert!(parse_tui(&json!(vec![one.clone(); 16])).is_ok());
        let r = refusal_of(json!(vec![one; 17]));
        assert!(r["reason"].is_string(), "{r}");
        assert!(err(&r).contains("tui"), "17 sections are refused by the declaration, naming `tui`: {r}");
        assert!(!declared(&json!({"tui": vec![json!({"title": "T", "call": {"verb": "v"}, "show": "kv"}); 17]})));
    }

    #[test]
    fn an_unknown_show_kind_is_refused_naming_the_valid_ones() {
        let r = refusal_of(json!([{"title": "T", "call": {"verb": "v"}, "show": "grid"}]));
        assert!(err(&r).contains("view.tui[0]") && err(&r).contains("show"), "{r}");
        assert!(err(&r).contains("kv") && err(&r).contains("text"), "{r}");
        let r = refusal_of(json!([{"title": "T", "call": {"verb": "v"}}]));
        assert_eq!(r["reason"], "missing_arg", "{r}");
        assert_eq!(r["arg"], "show", "{r}");
    }

    #[test]
    fn an_unknown_section_key_or_call_key_is_refused_naming_it() {
        let r = refusal_of(
            json!([{"title": "T", "call": {"verb": "v"}, "show": "kv", "colour": "red"}]),
        );
        assert!(err(&r).contains("colour") && err(&r).contains("view.tui[0]"), "{r}");
        let r =
            refusal_of(json!([{"title": "T", "call": {"verb": "v", "target": "x"}, "show": "kv"}]));
        assert!(err(&r).contains("call.target"), "{r}");
    }

    #[test]
    fn a_call_needs_a_verb_and_args_must_be_an_object() {
        for (call, want) in [
            (json!({}), "call.verb"),
            (json!({"verb": ""}), "call.verb"),
            (json!({"verb": 3}), "call.verb"),
            (json!("v"), "call"),
            (json!({"verb": "v", "args": [1]}), "call.args"),
        ] {
            let r = refusal_of(json!([{"title": "T", "call": call, "show": "kv"}]));
            assert!(err(&r).contains(want), "{call}: {r}");
        }
        let r = refusal_of(json!([{"title": "T", "show": "kv"}]));
        assert_eq!(r["arg"], "call", "{r}");
    }

    #[test]
    fn every_s_out_of_range_is_refused() {
        for bad in [json!(0), json!(-1), json!(3601), json!("5"), json!(0.5)] {
            let r = refusal_of(
                json!([{"title": "T", "call": {"verb": "v"}, "show": "kv", "every_s": bad}]),
            );
            assert_eq!(r["arg"], "every_s", "{bad}: {r}");
        }
        for ok in [json!(1), json!(3600)] {
            assert!(parse_tui(
                &json!([{"title": "T", "call": {"verb": "v"}, "show": "kv", "every_s": ok}])
            )
            .is_ok());
        }
    }

    #[test]
    fn fields_must_be_paths_or_path_label_objects() {
        for fields in [
            json!("x"),
            json!([1]),
            json!([""]),
            json!(["a", {"label": "x"}]),
            json!([{"path": "a", "colour": 1}]),
            json!([{"path": "a", "label": 2}]),
        ] {
            let r = refusal_of(
                json!([{"title": "T", "call": {"verb": "v"}, "show": "kv", "fields": fields}]),
            );
            assert!(err(&r).contains("view.tui[0]"), "{fields}: {r}");
        }
    }

    #[test]
    fn a_bad_declaration_in_the_record_shows_its_refusal_and_falls_back_to_generic() {
        let mut p =
            panel_with(json!({"tui": [{"title": "T", "call": {"verb": "v"}, "show": "grid"}]}));
        assert!(p.sections.is_empty());
        let e = p.decl_error.clone().expect("kept");
        assert!(
            e.contains("view.tui[0]") && e.contains("show") && e.contains("bad_arg"),
            "{e}"
        );
        // nothing but the reflect is asked, and the generic panel gets the keys
        let out = select(&mut p);
        assert_eq!(sent(&out), Vec::<String>::new());
        assert!(matches!(&out[0], Cmd::Reflect { .. }));
        let backend = ratatui::backend::TestBackend::new(60, 8);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| with_cx("a", None, |cx| p.render(f, f.area(), cx)))
            .unwrap();
        let shown: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(shown.contains("refused: view.tui[0]"), "{shown}");
        // a good declaration afterwards clears it
        p.set_decl(&two());
        assert!(p.decl_error.is_none() && p.sections.len() == 2);
    }
}
