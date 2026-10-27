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
use serde_json::{json, Map, Value};

use super::generic::GenericPanel;
use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

/// At most this many sections in one declaration.
pub const MAX_SECTIONS: usize = 16;
/// At most this many body lines drawn per section.
pub const MAX_LINES: usize = 200;
const SHOWS: &str = "kv, table, list, log, text";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    Kv,
    Table,
    List,
    Log,
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

/// A declaration refused: a sentence naming the section and the field, and the
/// kernel's reason code.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub error: String,
    pub reason: &'static str,
}

fn bad(error: String) -> Refusal {
    Refusal {
        error,
        reason: "bad_arg",
    }
}

/// The sections `view.tui` declares, or the refusal.
pub fn parse(view: &Value) -> Result<Vec<Section>, Refusal> {
    match view.get("tui") {
        Some(t) => parse_tui(t),
        None => Err(bad("view.tui: missing".into())),
    }
}

/// Does `view` declare a TUI view that parses?
pub fn declared(view: &Value) -> bool {
    parse(view).is_ok()
}

pub fn parse_tui(tui: &Value) -> Result<Vec<Section>, Refusal> {
    let Some(list) = tui.as_array() else {
        return Err(bad("view.tui: must be a list of sections".into()));
    };
    if list.is_empty() {
        return Err(bad("view.tui: must hold at least one section".into()));
    }
    if list.len() > MAX_SECTIONS {
        return Err(bad(format!(
            "view.tui: {} sections, at most {MAX_SECTIONS} are allowed",
            list.len()
        )));
    }
    list.iter()
        .enumerate()
        .map(|(i, s)| section(i, s))
        .collect()
}

fn section(i: usize, s: &Value) -> Result<Section, Refusal> {
    let at = format!("view.tui[{i}]");
    let Some(o) = s.as_object() else {
        return Err(bad(format!(
            "{at}: a section is an object {{title, call, show, fields?, every_s?}}"
        )));
    };
    for k in o.keys() {
        if !["title", "call", "show", "fields", "every_s"].contains(&k.as_str()) {
            return Err(bad(format!(
                "{at}: unknown key `{k}` (the keys are title, call, show, fields, every_s)"
            )));
        }
    }
    let title = match o.get("title").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => return Err(bad(format!("{at}.title: a non-empty string is required"))),
    };
    let Some(call) = o.get("call").and_then(Value::as_object) else {
        return Err(bad(format!(
            "{at}.call: an object {{verb, args?}} is required"
        )));
    };
    for k in call.keys() {
        if k != "verb" && k != "args" {
            return Err(bad(format!(
                "{at}.call: unknown key `{k}` (the keys are verb, args)"
            )));
        }
    }
    let verb = match call.get("verb").and_then(Value::as_str) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            return Err(bad(format!(
                "{at}.call.verb: a non-empty string is required"
            )))
        }
    };
    let args = match call.get("args") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m.clone()),
        Some(_) => return Err(bad(format!("{at}.call.args: must be an object"))),
    };
    let show = match o.get("show").and_then(Value::as_str) {
        Some("kv") => Show::Kv,
        Some("table") => Show::Table,
        Some("list") => Show::List,
        Some("log") => Show::Log,
        Some("text") => Show::Text,
        Some(other) => {
            return Err(bad(format!(
                "{at}.show: `{other}` is not a kind; the kinds are {SHOWS}"
            )))
        }
        None => return Err(bad(format!("{at}.show: required; one of {SHOWS}"))),
    };
    let mut fields = Vec::new();
    match o.get("fields") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            for (j, f) in a.iter().enumerate() {
                fields.push(field(&at, j, f)?);
            }
        }
        Some(_) => return Err(bad(format!("{at}.fields: must be a list of paths"))),
    }
    let every_s = match o.get("every_s") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(n) if (1.0..=3600.0).contains(&n) => Some(n),
            _ => {
                return Err(bad(format!(
                    "{at}.every_s: a number from 1 to 3600 seconds, got {v}"
                )))
            }
        },
    };
    Ok(Section {
        title,
        verb,
        args,
        show,
        fields,
        every_s,
    })
}

fn field(at: &str, j: usize, f: &Value) -> Result<Field, Refusal> {
    let at = format!("{at}.fields[{j}]");
    match f {
        Value::String(p) if !p.is_empty() => Ok(Field {
            path: p.clone(),
            label: p.clone(),
        }),
        Value::Object(o) => {
            for k in o.keys() {
                if k != "path" && k != "label" {
                    return Err(bad(format!(
                        "{at}: unknown key `{k}` (the keys are path, label)"
                    )));
                }
            }
            let path = match o.get("path").and_then(Value::as_str) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return Err(bad(format!("{at}.path: a non-empty string is required"))),
            };
            let label = match o.get("label") {
                None | Some(Value::Null) => path.clone(),
                Some(Value::String(l)) => l.clone(),
                Some(_) => return Err(bad(format!("{at}.label: must be a string"))),
            };
            Ok(Field { path, label })
        }
        _ => Err(bad(format!(
            "{at}: a path string or an object {{path, label?}} is required"
        ))),
    }
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
                self.decl_error = Some(format!("{} ({})", r.error, r.reason));
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
            *slot = match ro::refusal(reply) {
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

    fn refusal_of(tui: Value) -> Refusal {
        parse_tui(&tui).expect_err("refused")
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
            (json!({}), "list of sections"),
            (json!("x"), "list of sections"),
            (json!([]), "at least one"),
            (json!(vec![one.clone(); 17]), "17 sections, at most 16"),
            (json!([1]), "view.tui[0]"),
            (json!([one.clone(), {"title": ""}]), "view.tui[1].title"),
        ] {
            let r = refusal_of(tui.clone());
            assert_eq!(r.reason, "bad_arg", "{tui}");
            assert!(r.error.contains(want), "{tui}: {}", r.error);
        }
        assert!(parse_tui(&json!(vec![one; 16])).is_ok());
        assert!(!declared(&json!({"kind": "html"})));
    }

    #[test]
    fn an_unknown_show_kind_is_refused_naming_the_valid_ones() {
        let r = refusal_of(json!([{"title": "T", "call": {"verb": "v"}, "show": "grid"}]));
        assert!(r.error.contains("view.tui[0].show"), "{}", r.error);
        assert!(
            r.error.contains("`grid`") && r.error.contains("kv, table, list, log, text"),
            "{}",
            r.error
        );
        let r = refusal_of(json!([{"title": "T", "call": {"verb": "v"}}]));
        assert!(r.error.contains("show: required"), "{}", r.error);
    }

    #[test]
    fn an_unknown_section_key_or_call_key_is_refused_naming_it() {
        let r = refusal_of(
            json!([{"title": "T", "call": {"verb": "v"}, "show": "kv", "colour": "red"}]),
        );
        assert!(
            r.error.contains("`colour`") && r.error.contains("view.tui[0]"),
            "{}",
            r.error
        );
        let r =
            refusal_of(json!([{"title": "T", "call": {"verb": "v", "target": "x"}, "show": "kv"}]));
        assert!(
            r.error.contains("`target`") && r.error.contains("call"),
            "{}",
            r.error
        );
    }

    #[test]
    fn a_call_needs_a_verb_and_args_must_be_an_object() {
        for (call, want) in [
            (json!({}), "call.verb"),
            (json!({"verb": ""}), "call.verb"),
            (json!({"verb": 3}), "call.verb"),
            (json!("v"), "call: an object"),
            (
                json!({"verb": "v", "args": [1]}),
                "call.args: must be an object",
            ),
        ] {
            let r = refusal_of(json!([{"title": "T", "call": call, "show": "kv"}]));
            assert!(r.error.contains(want), "{call}: {}", r.error);
        }
        let r = refusal_of(json!([{"title": "T", "show": "kv"}]));
        assert!(r.error.contains("call"), "{}", r.error);
    }

    #[test]
    fn every_s_out_of_range_is_refused() {
        for bad in [json!(0), json!(-1), json!(3601), json!("5"), json!(0.5)] {
            let r = refusal_of(
                json!([{"title": "T", "call": {"verb": "v"}, "show": "kv", "every_s": bad}]),
            );
            assert!(
                r.error.contains("view.tui[0].every_s"),
                "{bad}: {}",
                r.error
            );
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
        for (fields, want) in [
            (json!("x"), "fields: must be a list"),
            (json!([1]), "fields[0]"),
            (json!([""]), "fields[0]"),
            (json!(["a", {"label": "x"}]), "fields[1].path"),
            (json!([{"path": "a", "colour": 1}]), "`colour`"),
            (
                json!([{"path": "a", "label": 2}]),
                "label: must be a string",
            ),
        ] {
            let r = refusal_of(
                json!([{"title": "T", "call": {"verb": "v"}, "show": "kv", "fields": fields}]),
            );
            assert!(r.error.contains(want), "{fields}: {}", r.error);
        }
    }

    #[test]
    fn pick_walks_dotted_paths_and_array_indexes() {
        let v = json!({"a": {"b": [10, {"c": "deep"}]}, "n": 1});
        assert_eq!(pick(&v, "a.b.0"), Some(&json!(10)));
        assert_eq!(pick(&v, "a.b.1.c"), Some(&json!("deep")));
        assert_eq!(pick(&v, "n"), Some(&json!(1)));
        assert_eq!(pick(&v, "a.x"), None);
        assert_eq!(pick(&v, "a.b.9"), None);
        assert_eq!(pick(&v, "n.z"), None);
    }

    #[test]
    fn kv_renders_the_picked_fields_with_labels_and_dotted_paths() {
        let s = sec("kv", json!(["c", {"path": "a.b", "label": "ab"}, "gone"]));
        let t = body(&s, Slot::Reply(json!({"a": {"b": 1}, "c": "x"})));
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines, vec!["    c  x", "    ab  1", "    gone  -"], "{t}");
    }

    #[test]
    fn kv_without_fields_lists_the_top_level_scalars() {
        let s = sec("kv", json!([]));
        let t = body(
            &s,
            Slot::Reply(json!({"a": 1, "nested": {"z": 1}, "list": [1], "s": "x"})),
        );
        assert!(t.contains("a  1") && t.contains("s  x"), "{t}");
        assert!(!t.contains("nested") && !t.contains("list"), "{t}");
    }

    #[test]
    fn table_renders_a_header_and_one_line_per_row_from_a_bare_array_and_from_the_first_array_key()
    {
        let s = sec("table", json!([]));
        let rows = json!([{"id": "a", "n": 1}, {"id": "longer", "n": 22}]);
        let t = body(&s, Slot::Reply(rows.clone()));
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(
            lines,
            vec!["    id      n", "    a       1", "    longer  22"],
            "{t}"
        );
        // the first array-valued key of an object
        let t2 = body(&s, Slot::Reply(json!({"count": 2, "jobs": rows})));
        assert_eq!(t2, t);
        // fields choose and label the columns
        let f = sec("table", json!([{"path": "n", "label": "num"}]));
        assert_eq!(body(&f, Slot::Reply(rows)), "    num\n    1\n    22");
    }

    #[test]
    fn list_renders_one_line_per_item_strings_and_objects() {
        let s = sec("list", json!([]));
        let t = body(&s, Slot::Reply(json!({"items": ["one", "two"]})));
        assert_eq!(t, "    one\n    two");
        let f = sec("list", json!(["name"]));
        let t = body(
            &f,
            Slot::Reply(json!([{"name": "ann", "x": 1}, {"name": "bo"}])),
        );
        assert_eq!(t, "    ann\n    bo");
        let t = body(&s, Slot::Reply(json!([{"name": "ann"}, 7])));
        assert_eq!(t, "    {\"name\":\"ann\"}\n    7");
    }

    #[test]
    fn log_renders_in_order_and_sticks_to_the_newest_line() {
        let s = sec("log", json!([]));
        let reply = json!({"lines": ["first", "second", "third"]});
        assert_eq!(
            body(&s, Slot::Reply(reply.clone())),
            "    first\n    second\n    third"
        );
        // a panel with a log is scrolled to the bottom until the human scrolls up
        let many: Vec<String> = (0..30).map(|i| format!("l{i}")).collect();
        let mut p =
            panel_with(json!({"tui": [{"title": "L", "call": {"verb": "log"}, "show": "log"}]}));
        p.slots[0] = Slot::Reply(json!({"lines": many}));
        let area = Rect::new(0, 0, 40, 10);
        with_cx("a", None, |cx| p.layout(area, cx));
        let total = p.lines().len() as u16;
        assert_eq!(p.effective_scroll(), total - 10);
        // new lines keep it pinned
        p.slots[0] =
            Slot::Reply(json!({"lines": (0..40).map(|i| format!("l{i}")).collect::<Vec<_>>()}));
        with_cx("a", None, |cx| p.layout(area, cx));
        assert_eq!(p.effective_scroll(), p.lines().len() as u16 - 10);
        // scrolling up lets go; further new lines no longer move it
        with_cx("a", None, |cx| {
            p.on_key(key(crossterm::event::KeyCode::Up), cx, &mut Vec::new())
        });
        let held = p.effective_scroll();
        assert_eq!(held, p.lines().len() as u16 - 11);
        p.slots[0] =
            Slot::Reply(json!({"lines": (0..50).map(|i| format!("l{i}")).collect::<Vec<_>>()}));
        with_cx("a", None, |cx| p.layout(area, cx));
        assert_eq!(p.effective_scroll(), held);
    }

    #[test]
    fn text_renders_a_string_reply_and_a_picked_field() {
        let s = sec("text", json!([]));
        assert_eq!(
            body(&s, Slot::Reply(json!("hello\nworld"))),
            "    hello\n    world"
        );
        assert_eq!(
            body(&s, Slot::Reply(json!({"text": "from text"}))),
            "    from text"
        );
        let f = sec("text", json!(["a.b"]));
        assert_eq!(
            body(&f, Slot::Reply(json!({"a": {"b": "picked"}, "text": "no"}))),
            "    picked"
        );
    }

    #[test]
    fn a_refusal_renders_error_and_reason_and_error_only_without_reason() {
        let mut p = panel_with(two());
        select(&mut p);
        with_cx("a", None, |_| ());
        p.on_reply(
            &json!({"error": "nope", "reason": "denied"}),
            &mut Vec::new(),
        );
        p.on_reply(&json!({"hello": "there"}), &mut Vec::new());
        let t = text(&p.lines());
        assert!(t.contains("refused: nope (denied)"), "{t}");
        assert!(t.contains("there"), "the other section still renders:\n{t}");
        let s = sec("kv", json!([]));
        assert_eq!(
            body(
                &s,
                Slot::Refused {
                    error: "nope".into(),
                    reason: None
                }
            ),
            "    refused: nope"
        );
        assert_eq!(body(&s, Slot::Pending), "    loading...");
    }

    #[test]
    fn on_select_sends_each_sections_verb_with_its_args_and_nothing_else() {
        let mut p = panel_with(two());
        let out = select(&mut p);
        assert_eq!(sent(&out), vec!["one", "two"]);
        assert!(matches!(&out[0], Cmd::Reflect { .. }));
        assert!(
            matches!(&out[1], Cmd::Send { target, payload } if target == "a" && payload == &json!({"type": "one", "n": 1}))
        );
        assert_eq!(out.len(), 3);
        assert_eq!(p.inflight, VecDeque::from([0, 1]));
        // `r` asks again, whole, without piling up the in-flight list
        let mut out = Vec::new();
        with_cx("a", None, |cx| {
            p.on_key(key(crossterm::event::KeyCode::Char('r')), cx, &mut out)
        });
        assert_eq!(sent(&out), vec!["one", "two"]);
        assert_eq!(p.inflight.len(), 2);
    }

    #[test]
    fn replies_land_in_the_section_that_asked_in_send_order() {
        let mut p = panel_with(two());
        select(&mut p);
        // the first is refused, the second answers: no shift
        assert!(p.on_reply(&json!({"error": "no", "reason": "denied"}), &mut Vec::new()));
        assert!(p.on_reply(&json!("second answer"), &mut Vec::new()));
        assert!(
            matches!(&p.slots[0], Slot::Refused { error, reason } if error == "no" && reason.as_deref() == Some("denied"))
        );
        assert_eq!(p.slots[1], Slot::Reply(json!("second answer")));
        assert!(p.inflight.is_empty());
        // a stray reply with nothing in flight is not taken
        assert!(!p.on_reply(&json!({"x": 1}), &mut Vec::new()));
    }

    #[test]
    fn a_reflect_shaped_reply_is_not_a_section_answer() {
        let mut p = panel_with(two());
        select(&mut p);
        p.on_reply(&json!({"sentence": "s", "verbs": {}}), &mut Vec::new());
        assert_eq!(p.inflight.len(), 2, "the reflect did not consume a slot");
        assert_eq!(p.slots, vec![Slot::Pending, Slot::Pending]);
    }

    #[test]
    fn tick_asks_only_sections_with_every_s_when_due() {
        let mut p = panel_with(two());
        let mut out = Vec::new();
        p.tick(100.0, &mut out);
        // never asked yet: the every_s section is due once
        assert_eq!(sent(&out), vec!["two"]);
        let mut out = Vec::new();
        p.tick(102.0, &mut out);
        assert!(out.is_empty(), "before due, and already in flight");
        p.on_reply(&json!("a"), &mut Vec::new());
        p.tick(104.9, &mut out);
        assert!(out.is_empty(), "not yet due");
        p.tick(105.0, &mut out);
        assert_eq!(
            sent(&out),
            vec!["two"],
            "only the section with every_s, never `one`"
        );
    }

    #[test]
    fn g_toggles_the_generic_reflect_view_and_back() {
        let mut p = panel_with(two());
        assert!(!p.show_generic);
        let g = key(crossterm::event::KeyCode::Char('g'));
        with_cx("a", None, |cx| p.on_key(g, cx, &mut Vec::new()));
        assert!(p.show_generic);
        with_cx("a", None, |cx| p.on_key(g, cx, &mut Vec::new()));
        assert!(!p.show_generic);
        with_cx("a", None, |cx| {
            assert!(p.status(cx).unwrap().contains("declared · g generic"))
        });
    }

    #[test]
    fn a_bad_declaration_in_the_record_shows_its_refusal_and_falls_back_to_generic() {
        let mut p =
            panel_with(json!({"tui": [{"title": "T", "call": {"verb": "v"}, "show": "grid"}]}));
        assert!(p.sections.is_empty());
        let e = p.decl_error.clone().expect("kept");
        assert!(
            e.contains("view.tui[0].show") && e.contains("bad_arg"),
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
        assert!(shown.contains("refused: view.tui[0].show"), "{shown}");
        // a good declaration afterwards clears it
        p.set_decl(&two());
        assert!(p.decl_error.is_none() && p.sections.len() == 2);
    }
}
