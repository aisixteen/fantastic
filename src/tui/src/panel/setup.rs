//! SET UP AN AI AGENT — one page, not a wizard.
//!
//! An agent is five fields, and this page shows all five at once: the id, the
//! handler (a backend this build links, from the kernel's own catalog), the
//! model (as that backend names it), its endpoint, and who it may call.
//! None has a default;
//! a missing one is called out in place. `enter` on WRITE THE RECORD writes it
//! (`kern/setup`), and the answer, a path or the kernel's refusal, shows right
//! there.
//!
//! Keys: ↑ ↓ pick a line · `enter` edit that field (enter again keeps it, esc
//! drops the edit) · ← → pick the handler · `k` put the backend's key in the
//! keychain, when it takes one · `esc` leave.
//!
//! KEYS (`kern/keys`): the `keys kept` line chooses where provider keys go —
//! PLAIN (one 0600 file per key; a yellow warning says anything running as the
//! user can read them) or SECURE (one sealed file, its master key the only
//! keychain item). ← → picks, enter applies (moving any stored keys); there is
//! no default, and `k` is refused until one is chosen. The line says where.
//!
//! TEMPLATES (`kern/templates`): the backend, model and endpoint can be SAVED
//! under a name the human types (never derived) in the user's home config dir,
//! and LOADED back into the page later, in any project. `d` on a saved template
//! makes it the default for new AI agents — nothing is a default until then.
//!
//! Handing the page to a coder in the in-app terminal is future work: a coder
//! cannot attach to the running kernel yet.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::ev::Cmd;
use crate::kern::setup::Record;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};
use crate::theme;

/// The tree row's id. `#` never appears in a kernel agent id.
pub const SETUP_ID: &str = "#setup";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line_ {
    Id,
    Handler,
    Model,
    Endpoint,
    MayCall,
    Write,
    KeyMode,
    TplName,
    TplSave,
    TplPick,
}

const LINES: [Line_; 10] = [
    Line_::Id,
    Line_::Handler,
    Line_::Model,
    Line_::Endpoint,
    Line_::MayCall,
    Line_::Write,
    Line_::KeyMode,
    Line_::TplName,
    Line_::TplSave,
    Line_::TplPick,
];

pub struct SetupPanel {
    /// `None` until the kernel said which backends it links.
    backends: Option<Vec<String>>,
    handler: Option<usize>,
    id: String,
    model: String,
    endpoint: String,
    may_call: String,
    at: usize,
    /// The text being edited, and for which line.
    editing: Option<(Line_, String)>,
    /// A key being typed, masked on screen.
    key: Option<String>,
    /// The last write or key store.
    result: Option<Result<String, String>>,
    /// The name to save the template under — typed, never derived.
    tpl_name: String,
    /// The saved templates, once read (or why they cannot be).
    shelf: Option<Result<crate::kern::templates::Shelf, String>>,
    /// Which saved template LOAD / `d` act on.
    pick: Option<usize>,
    /// Where keys are kept (mode `None` until chosen, and where), once read.
    key_mode: Option<Result<(Option<String>, String), String>>,
    /// The mode the `keys kept` line shows while the human picks (← →).
    key_pick: Option<&'static str>,
}

impl SetupPanel {
    pub fn new() -> Self {
        Self {
            backends: None,
            handler: None,
            id: String::new(),
            model: String::new(),
            endpoint: String::new(),
            may_call: String::new(),
            at: 0,
            editing: None,
            key: None,
            result: None,
            tpl_name: String::new(),
            shelf: None,
            pick: None,
            key_mode: None,
            key_pick: None,
        }
    }

    pub fn set_key_mode(&mut self, m: Result<(Option<String>, String), String>) {
        self.key_pick = None;
        self.key_mode = Some(m);
    }

    /// The chosen mode, once read and chosen.
    fn chosen_mode(&self) -> Option<&str> {
        self.key_mode.as_ref()?.as_ref().ok()?.0.as_deref()
    }

    pub fn set_shelf(&mut self, s: Result<crate::kern::templates::Shelf, String>) {
        let n = s.as_ref().map_or(0, |s| s.templates.len());
        self.pick = self.pick.filter(|i| *i < n).or((n > 0).then_some(0));
        self.shelf = Some(s);
    }

    fn picked(&self) -> Option<&crate::kern::templates::Template> {
        let shelf = self.shelf.as_ref()?.as_ref().ok()?;
        self.pick.and_then(|i| shelf.templates.get(i))
    }

    /// Fill the page from the picked template: the handler (when this build
    /// links it), the model and the endpoint. The id stays the human's.
    fn load_picked(&mut self) {
        let Some(t) = self.picked().cloned() else {
            self.result = Some(Err("no saved template to load".into()));
            return;
        };
        let at = self.backends.as_ref().and_then(|b| b.iter().position(|h| *h == t.handler()));
        let Some(i) = at else {
            self.result = Some(Err(format!("template `{}` needs {}, which this build does not link", t.name, t.handler())));
            return;
        };
        self.handler = Some(i);
        self.model = t.model();
        self.endpoint = t.endpoint();
        self.result = Some(Ok(format!("loaded template `{}` — give it an id, then WRITE THE RECORD", t.name)));
    }

    pub fn set_backends(&mut self, b: Vec<String>) {
        // One backend is still a choice the human makes, so none is pre-picked.
        self.handler = self.handler.filter(|i| *i < b.len());
        self.backends = Some(b);
    }

    pub fn set_result(&mut self, r: Result<String, String>) {
        if r.is_ok() && self.key.is_none() && self.result_is_write(&r) {
            // Written: the page starts over for the next agent.
            self.id.clear();
            self.model.clear();
            self.endpoint.clear();
            self.may_call.clear();
            self.handler = None;
            self.at = 0;
        }
        self.result = Some(r);
    }

    fn result_is_write(&self, r: &Result<String, String>) -> bool {
        matches!(r, Ok(s) if s.starts_with("written"))
    }

    fn handler_name(&self) -> Option<&str> {
        let b = self.backends.as_ref()?;
        self.handler.and_then(|i| b.get(i)).map(String::as_str)
    }

    /// The record as it stands.
    pub fn record(&self) -> Record {
        Record {
            id: self.id.trim().to_string(),
            handler: self.handler_name().unwrap_or("").to_string(),
            model: self.model.trim().to_string(),
            endpoint: self.endpoint.trim().to_string(),
            may_call: self.may_call.split_whitespace().map(str::to_string).collect(),
        }
    }

    fn field(&mut self, l: Line_) -> Option<&mut String> {
        match l {
            Line_::Id => Some(&mut self.id),
            Line_::Model => Some(&mut self.model),
            Line_::Endpoint => Some(&mut self.endpoint),
            Line_::MayCall => Some(&mut self.may_call),
            Line_::TplName => Some(&mut self.tpl_name),
            Line_::Handler | Line_::Write | Line_::KeyMode | Line_::TplSave | Line_::TplPick => None,
        }
    }

    fn takes_key(&self) -> Option<&'static str> {
        self.handler_name().and_then(crate::kern::keys::account_of)
    }
}

impl Default for SetupPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel for SetupPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Setup
    }

    fn needs(&self) -> Needs {
        Needs { rows: false, reflect: false, watch: false }
    }

    fn on_key(&mut self, k: KeyEvent, _cx: &Cx<'_>, out: &mut Vec<Cmd>) -> Handled {
        // ── typing a key: masked, never shown, never kept after enter ───────
        if let Some(buf) = self.key.as_mut() {
            match k.code {
                KeyCode::Esc => self.key = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                KeyCode::Enter => {
                    let secret = self.key.take().unwrap_or_default();
                    if let (false, Some(module)) = (secret.is_empty(), self.handler_name()) {
                        out.push(Cmd::StoreKey { module: module.to_string(), secret });
                    }
                }
                _ => {}
            }
            return Handled::Yes;
        }
        // ── editing a field ────────────────────────────────────────────────
        if let Some((line, buf)) = self.editing.as_mut() {
            match k.code {
                KeyCode::Esc => self.editing = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                KeyCode::Enter => {
                    let (line, text) = (*line, buf.clone());
                    self.editing = None;
                    if let Some(f) = self.field(line) {
                        *f = text;
                    }
                }
                _ => {}
            }
            return Handled::Yes;
        }
        let line = LINES[self.at];
        match k.code {
            KeyCode::Up => self.at = self.at.saturating_sub(1),
            KeyCode::Down => self.at = (self.at + 1).min(LINES.len() - 1),
            KeyCode::Left | KeyCode::Right if line == Line_::Handler => {
                let n = self.backends.as_ref().map_or(0, Vec::len);
                if n > 0 {
                    self.handler = Some(match (self.handler, k.code) {
                        (None, KeyCode::Left) => n - 1,
                        (None, _) => 0,
                        (Some(i), KeyCode::Left) => (i + n - 1) % n,
                        (Some(i), _) => (i + 1) % n,
                    });
                }
            }
            KeyCode::Left | KeyCode::Right if line == Line_::KeyMode => {
                let now = self.key_pick.or(match self.chosen_mode() {
                    Some("plain") => Some("plain"),
                    Some("secure") => Some("secure"),
                    _ => None,
                });
                self.key_pick = Some(match now {
                    Some("plain") => "secure",
                    Some(_) => "plain",
                    // nothing chosen: → offers secure first, ← plain
                    None if k.code == KeyCode::Right => "secure",
                    None => "plain",
                });
            }
            KeyCode::Enter if line == Line_::KeyMode => match self.key_pick {
                Some(m) if Some(m) != self.chosen_mode() => out.push(Cmd::SetKeyMode { mode: m.to_string() }),
                Some(_) => {}
                None => self.result = Some(Err("pick plain or secure with ← →, then enter".into())),
            },
            KeyCode::Left | KeyCode::Right if line == Line_::TplPick => {
                let n = self.shelf.as_ref().and_then(|s| s.as_ref().ok()).map_or(0, |s| s.templates.len());
                if n > 0 {
                    let i = self.pick.unwrap_or(0);
                    self.pick = Some(if k.code == KeyCode::Left { (i + n - 1) % n } else { (i + 1) % n });
                }
            }
            KeyCode::Enter if line == Line_::TplSave => {
                let rec = self.record();
                let mut miss: Vec<&str> = Vec::new();
                if self.tpl_name.trim().is_empty() {
                    miss.push("template name");
                }
                miss.extend(rec.missing().into_iter().filter(|m| *m != "id"));
                if miss.is_empty() {
                    out.push(Cmd::SaveTemplate {
                        name: self.tpl_name.trim().to_string(),
                        record: serde_json::json!({
                            "handler_module": rec.handler,
                            "model": rec.model,
                            "endpoint": rec.endpoint,
                        }),
                    });
                } else {
                    self.result = Some(Err(format!("missing: {}", miss.join(", "))));
                }
            }
            KeyCode::Enter if line == Line_::TplPick => self.load_picked(),
            KeyCode::Char('d') if line == Line_::TplPick => {
                if let Some(t) = self.picked() {
                    let is_default = self.shelf.as_ref().and_then(|s| s.as_ref().ok()).and_then(|s| s.preferred.as_deref())
                        == Some(t.name.as_str());
                    out.push(Cmd::PreferTemplate { name: (!is_default).then(|| t.name.clone()) });
                }
            }
            KeyCode::Enter if line == Line_::Write => {
                let rec = self.record();
                if rec.missing().is_empty() {
                    out.push(Cmd::WriteAi(rec));
                } else {
                    self.result = Some(Err(format!("missing: {}", rec.missing().join(", "))));
                }
            }
            KeyCode::Enter => {
                if let Some(f) = self.field(line) {
                    let text = f.clone();
                    self.editing = Some((line, text));
                }
            }
            KeyCode::Char('k') if self.takes_key().is_some() && self.chosen_mode().is_none() => {
                self.result = Some(Err("choose where keys are kept first — the `keys kept` line (← → then enter)".into()));
            }
            KeyCode::Char('k') if self.takes_key().is_some() => self.key = Some(String::new()),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_event(&mut self, _payload: &Value, _out: &mut Vec<Cmd>) -> bool {
        false
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Backends);
        out.push(Cmd::KeyMode);
        out.push(Cmd::Templates);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        let dim = Style::default().fg(theme::DIM);
        let fg = Style::default().fg(theme::FG);
        let bold = Style::default().fg(theme::FG).add_modifier(Modifier::BOLD);
        let warn = Style::default().fg(theme::BLOCKED).add_modifier(Modifier::BOLD);
        let rule = |label: &str| {
            let w = area.width.saturating_sub(4 + label.chars().count() as u16) as usize;
            Line::from(vec![
                Span::styled(format!("  ── {label} "), dim),
                Span::styled("─".repeat(w.saturating_sub(4)), Style::default().fg(theme::RULE)),
            ])
        };
        let none = self.backends.as_ref().is_some_and(Vec::is_empty);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("  SET UP AN AI AGENT", bold),
                Span::styled(
                    if none { "      no ai backend is linked into this build" } else { "" },
                    warn,
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  An ai agent is four fields. Nothing is filled in for you, and nothing is written",
                Style::default().fg(theme::MUTED),
            )),
            Line::from(Span::styled(
                "  until you pick WRITE THE RECORD.",
                Style::default().fg(theme::MUTED),
            )),
            Line::from(""),
            rule("THE RECORD"),
            Line::from(""),
        ];
        let handler_text = match (&self.backends, self.handler_name()) {
            (None, _) => "reading the catalog…".to_string(),
            (Some(b), _) if b.is_empty() => "— none linked —".to_string(),
            (Some(_), Some(h)) => format!("‹ {h} ›"),
            (Some(b), None) => format!("‹ pick one of {} ›", b.len()),
        };
        let rows: [(Line_, &str, String, &str); 5] = [
            (Line_::Id, "id", self.id.clone(), "a new agent id"),
            (Line_::Handler, "handler", handler_text, "what makes it a conversation — ← → to pick"),
            (Line_::Model, "model", self.model.clone(), "as the backend names it"),
            (Line_::Endpoint, "endpoint", self.endpoint.clone(), "the URL it serves on — required"),
            (Line_::MayCall, "may call", self.may_call.clone(), "agent ids, space-separated — optional"),
        ];
        for (i, (line, label, value, note)) in rows.into_iter().enumerate() {
            let cur = i == self.at && self.key.is_none();
            let (shown, missing) = match &self.editing {
                Some((l, buf)) if *l == line => (format!("{buf}▌"), false),
                _ if value.is_empty() || (line == Line_::Handler && self.handler.is_none()) => {
                    let empty = if value.is_empty() { "— not set —".to_string() } else { value };
                    (empty, !matches!(line, Line_::MayCall | Line_::Endpoint))
                }
                _ => (value, false),
            };
            lines.push(Line::from(vec![
                Span::styled(if cur { "  ▸ " } else { "    " }, Style::default().fg(theme::ACCENT_HI)),
                Span::styled(format!("{label:<10}"), Style::default().fg(theme::MUTED)),
                Span::styled(
                    shown.clone(),
                    if missing {
                        warn
                    } else if cur {
                        bold.add_modifier(Modifier::REVERSED)
                    } else {
                        bold
                    },
                ),
                Span::styled(if missing { "  ⚠" } else { "" }, warn),
                Span::styled(format!("   {note}"), dim),
            ]));
        }
        lines.push(Line::from(""));
        let can = self.record().missing().is_empty();
        let cur = LINES[self.at] == Line_::Write && self.key.is_none();
        lines.push(Line::from(vec![
            Span::styled(if cur { "  ▸ " } else { "    " }, Style::default().fg(theme::ACCENT_HI)),
            Span::styled(
                " WRITE THE RECORD ",
                if can && cur {
                    Style::default().fg(theme::FG).bg(theme::ACCENT_HI).add_modifier(Modifier::BOLD)
                } else if can {
                    bold
                } else {
                    dim
                },
            ),
        ]));
        // ── where keys are kept ─────────────────────────────────────────
        lines.push(Line::from(""));
        let km_cur = LINES[self.at] == Line_::KeyMode && self.key.is_none();
        let (mode_text, where_text) = match &self.key_mode {
            None => ("reading…".to_string(), String::new()),
            Some(Err(e)) => (format!("unavailable: {e}"), String::new()),
            Some(Ok((m, w))) => {
                let shown = self.key_pick.or(m.as_deref().and_then(|m| if m == "plain" { Some("plain") } else if m == "secure" { Some("secure") } else { None }));
                let pending = self.key_pick.is_some() && self.key_pick != m.as_deref();
                (
                    match shown {
                        Some(s) if pending => format!("‹ {s} ›  enter switches to it"),
                        Some(s) => format!("‹ {s} ›"),
                        None => "‹ not chosen ›".to_string(),
                    },
                    w.clone(),
                )
            }
        };
        lines.push(Line::from(vec![
            Span::styled(if km_cur { "  ▸ " } else { "    " }, Style::default().fg(theme::ACCENT_HI)),
            Span::styled(format!("{:<10}", "keys kept"), Style::default().fg(theme::MUTED)),
            Span::styled(
                mode_text,
                if self.chosen_mode().is_none() {
                    warn
                } else if km_cur {
                    bold.add_modifier(Modifier::REVERSED)
                } else {
                    bold
                },
            ),
            Span::styled("   ← → plain / secure · enter applies", dim),
        ]));
        if !where_text.is_empty() {
            lines.push(Line::from(Span::styled(format!("              {where_text}"), dim)));
        }
        if self.chosen_mode() == Some("plain") {
            lines.push(Line::from(Span::styled(
                "              ⚠ not secured: keys are plain text — anything running as you can read them",
                Style::default().fg(theme::BLOCKED),
            )));
        }
        if self.takes_key().is_some() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("    k ", bold),
                Span::styled(
                    match self.chosen_mode() {
                        Some(m) => format!("put this backend's key where keys are kept ({m})"),
                        None => "put this backend's key — choose where keys are kept first".to_string(),
                    },
                    dim,
                ),
            ]));
        }
        if let Some(buf) = &self.key {
            lines.push(Line::from(vec![
                Span::styled("    key: ", bold),
                Span::styled(format!("{}▌", "•".repeat(buf.chars().count())), fg),
                Span::styled("   enter stores it · esc drops it", dim),
            ]));
        }
        if let Some(r) = &self.result {
            lines.push(Line::from(""));
            lines.push(match r {
                Ok(s) => Line::from(Span::styled(format!("  ✓ {s}"), Style::default().fg(theme::WORKING).add_modifier(Modifier::BOLD))),
                Err(e) => Line::from(Span::styled(format!("  ✕ {e}"), Style::default().fg(theme::FAILED).add_modifier(Modifier::BOLD))),
            });
        }
        // ── TEMPLATES ─────────────────────────────────────────────────────
        lines.push(Line::from(""));
        lines.push(rule("TEMPLATES — REUSE IT IN ANY PROJECT"));
        lines.push(Line::from(""));
        let row_cur = |l: Line_| LINES[self.at] == l && self.key.is_none();
        let arrow = |c: bool| Span::styled(if c { "  ▸ " } else { "    " }, Style::default().fg(theme::ACCENT_HI));
        let name_shown = match &self.editing {
            Some((Line_::TplName, buf)) => format!("{buf}▌"),
            _ if self.tpl_name.is_empty() => "— not set —".to_string(),
            _ => self.tpl_name.clone(),
        };
        lines.push(Line::from(vec![
            arrow(row_cur(Line_::TplName)),
            Span::styled(format!("{:<10}", "name"), Style::default().fg(theme::MUTED)),
            Span::styled(name_shown, if row_cur(Line_::TplName) { bold.add_modifier(Modifier::REVERSED) } else { bold }),
            Span::styled("   what to call it — you choose; letters, digits, - _", dim),
        ]));
        lines.push(Line::from(vec![
            arrow(row_cur(Line_::TplSave)),
            Span::styled(" SAVE AS TEMPLATE ", if row_cur(Line_::TplSave) { bold.add_modifier(Modifier::REVERSED) } else { bold }),
            Span::styled("   handler + model + endpoint, never the key, the id or grants", dim),
        ]));
        let (pick_text, pick_note) = match &self.shelf {
            None => ("reading your templates…".to_string(), String::new()),
            Some(Err(e)) => (format!("unavailable: {e}"), String::new()),
            Some(Ok(s)) if s.templates.is_empty() => ("— none saved yet —".to_string(), format!("   in {}", s.dir)),
            Some(Ok(s)) => {
                let t = self.picked().cloned();
                let name = t.as_ref().map_or(String::new(), |t| t.name.clone());
                let star = if s.preferred.as_deref() == Some(name.as_str()) { " ★ default" } else { "" };
                (
                    format!("‹ {name} ›{star}"),
                    t.map_or(String::new(), |t| format!("   {} · {} · {}", t.handler(), t.model(), t.endpoint())),
                )
            }
        };
        lines.push(Line::from(vec![
            arrow(row_cur(Line_::TplPick)),
            Span::styled(" LOAD TEMPLATE ", if row_cur(Line_::TplPick) { bold.add_modifier(Modifier::REVERSED) } else { bold }),
            Span::styled(format!(" {pick_text}"), fg),
            Span::styled(pick_note, dim),
        ]));
        lines.push(Line::from(Span::styled(
            "    ← → pick · enter loads it into the record · d makes it (or stops it being) your default for new AI agents",
            dim,
        )));
        lines.push(Line::from(""));
        lines.push(rule("OR HAND IT TO A CODER"));
        lines.push(Line::from(Span::styled(
            "  later: a coder in the in-app terminal (t) cannot attach to this kernel yet.",
            dim,
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ↑ ↓ pick · enter edit / write / save / load · ← → handler, template · d default · esc leave",
            dim,
        )));
        f.render_widget(Paragraph::new(lines), area);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some("setting up an ai agent".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn drive(p: &mut SetupPanel, keys: &[KeyCode]) -> Vec<Cmd> {
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let a: crate::kern::model::AgentId = SETUP_ID.into();
        let cx = Cx { agent: &a, store: &store, tasks: &[], seen: &seen, reflect: None };
        let mut out = Vec::new();
        for k in keys {
            p.on_key(key(*k), &cx, &mut out);
        }
        out
    }

    fn typed(s: &str) -> Vec<KeyCode> {
        let mut v = vec![KeyCode::Enter];
        v.extend(s.chars().map(KeyCode::Char));
        v.push(KeyCode::Enter);
        v
    }

    #[test]
    fn nothing_is_written_until_every_required_field_is_set() {
        let mut p = SetupPanel::new();
        p.set_backends(vec!["anthropic_backend.tools".into()]);
        assert_eq!(p.handler_name(), None, "one backend is still not a default");
        let out = drive(&mut p, &[KeyCode::Down; 5].into_iter().chain([KeyCode::Enter]).collect::<Vec<_>>());
        assert!(out.is_empty());
        assert!(matches!(&p.result, Some(Err(e)) if e == "missing: id, handler, model, endpoint"));
    }

    #[test]
    fn the_five_fields_make_the_record_and_enter_writes_it() {
        let mut p = SetupPanel::new();
        p.set_backends(vec!["anthropic_backend.tools".into(), "llamacpp_backend.tools".into()]);
        let mut keys = typed("docreader");
        keys.extend([KeyCode::Down, KeyCode::Right, KeyCode::Right, KeyCode::Down]);
        keys.extend(typed("m-1"));
        keys.push(KeyCode::Down);
        keys.extend(typed("http://127.0.0.1:9/v1/messages"));
        keys.push(KeyCode::Down);
        keys.extend(typed("files exec"));
        keys.extend([KeyCode::Down, KeyCode::Enter]);
        let out = drive(&mut p, &keys);
        match &out[..] {
            [Cmd::WriteAi(r)] => assert_eq!(
                r,
                &Record {
                    id: "docreader".into(),
                    handler: "llamacpp_backend.tools".into(),
                    model: "m-1".into(),
                    endpoint: "http://127.0.0.1:9/v1/messages".into(),
                    may_call: vec!["files".into(), "exec".into()],
                }
            ),
            other => panic!("{other:?}"),
        }
        p.set_result(Ok("written to  x".into()));
        assert!(p.id.is_empty(), "a written record clears the page for the next one");
    }

    #[test]
    fn esc_drops_an_edit_and_a_build_with_no_keyed_backend_offers_no_k() {
        let mut p = SetupPanel::new();
        p.set_backends(vec!["llamacpp_backend.tools".into()]);
        drive(&mut p, &[KeyCode::Enter, KeyCode::Char('x'), KeyCode::Esc]);
        assert!(p.id.is_empty());
        drive(&mut p, &[KeyCode::Down, KeyCode::Right]);
        assert!(p.takes_key().is_none());
        let out = drive(&mut p, &[KeyCode::Char('k')]);
        assert!(out.is_empty() && p.key.is_none());
    }

    fn shelf(names: &[(&str, &str)], preferred: Option<&str>) -> crate::kern::templates::Shelf {
        crate::kern::templates::Shelf {
            templates: names
                .iter()
                .map(|(n, h)| crate::kern::templates::Template {
                    name: n.to_string(),
                    version: "v".into(),
                    record: serde_json::json!({"handler_module": h, "model": format!("{n}-m"), "endpoint": format!("http://{n}")})
                        .as_object()
                        .unwrap()
                        .clone(),
                })
                .collect(),
            preferred: preferred.map(str::to_string),
            dir: "/home/.config/fantastic/templates".into(),
        }
    }

    #[test]
    fn a_template_is_saved_under_a_typed_name_without_id_or_grants() {
        let mut p = SetupPanel::new();
        p.set_backends(vec!["anthropic_backend.tools".into()]);
        // backend, model, endpoint — and an id and grants, which a template never takes
        let mut keys = typed("docreader");
        keys.extend([KeyCode::Down, KeyCode::Right, KeyCode::Down]);
        keys.extend(typed("m-1"));
        keys.push(KeyCode::Down);
        keys.extend(typed("http://e"));
        keys.push(KeyCode::Down);
        keys.extend(typed("files"));
        keys.extend([KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]); // SAVE with no name
        let out = drive(&mut p, &keys);
        assert!(out.is_empty());
        assert!(matches!(&p.result, Some(Err(e)) if e == "missing: template name"), "{:?}", p.result);
        let mut keys = vec![KeyCode::Up];
        keys.extend(typed("opus"));
        keys.extend([KeyCode::Down, KeyCode::Enter]);
        match &drive(&mut p, &keys)[..] {
            [Cmd::SaveTemplate { name, record }] => {
                assert_eq!(name, "opus");
                assert_eq!(
                    record,
                    &serde_json::json!({"handler_module": "anthropic_backend.tools", "model": "m-1", "endpoint": "http://e"})
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_saved_template_loads_into_the_page_and_d_toggles_the_default() {
        let mut p = SetupPanel::new();
        p.set_backends(vec!["anthropic_backend.tools".into(), "responses_backend.tools".into()]);
        p.set_shelf(Ok(shelf(&[("opus", "anthropic_backend.tools"), ("gpt", "responses_backend.tools")], Some("gpt"))));
        let mut keys = vec![KeyCode::Down; 9]; // LOAD TEMPLATE
        keys.extend([KeyCode::Right, KeyCode::Enter]); // ‹ gpt › — loads it
        drive(&mut p, &keys);
        assert_eq!(p.handler_name(), Some("responses_backend.tools"));
        assert_eq!((p.model.as_str(), p.endpoint.as_str()), ("gpt-m", "http://gpt"));
        assert!(p.id.is_empty(), "the id stays the human's");
        match &drive(&mut p, &[KeyCode::Char('d')])[..] {
            [Cmd::PreferTemplate { name: None }] => {}
            other => panic!("d on the default clears it: {other:?}"),
        }
        drive(&mut p, &[KeyCode::Left]);
        match &drive(&mut p, &[KeyCode::Char('d')])[..] {
            [Cmd::PreferTemplate { name: Some(n) }] => assert_eq!(n, "opus"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_template_for_a_backend_this_build_lacks_is_said_not_half_loaded() {
        let mut p = SetupPanel::new();
        p.set_backends(vec!["anthropic_backend.tools".into()]);
        p.set_shelf(Ok(shelf(&[("q", "vllm_backend.tools")], None)));
        let mut keys = vec![KeyCode::Down; 9];
        keys.push(KeyCode::Enter);
        drive(&mut p, &keys);
        assert!(matches!(&p.result, Some(Err(e)) if e.contains("does not link")), "{:?}", p.result);
        assert!(p.model.is_empty() && p.handler.is_none());
    }

    #[test]
    fn keys_wait_for_an_explicit_mode_and_plain_is_warned() {
        let mut p = SetupPanel::new();
        p.set_backends(vec!["x_backend.tools".into()]);
        p.set_key_mode(Ok((None, "not chosen — …".into())));
        // go to `keys kept`, → offers secure, enter applies it
        let mut keys = vec![KeyCode::Down; 6];
        keys.extend([KeyCode::Right, KeyCode::Enter]);
        match &drive(&mut p, &keys)[..] {
            [Cmd::SetKeyMode { mode }] => assert_eq!(mode, "secure"),
            other => panic!("{other:?}"),
        }
        p.set_key_mode(Ok((Some("plain".into()), "plain text in /h/secrets".into())));
        assert_eq!(p.chosen_mode(), Some("plain"));
        // enter without a new pick applies nothing
        assert!(drive(&mut p, &[KeyCode::Enter]).is_empty());
    }
}
