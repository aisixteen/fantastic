//! SET UP AN AI AGENT — the canvas's two-page wizard, in the terminal (owner
//! 2026-10-12: "make the ai agent wizard simple as the ai agent html").
//!
//! 1. a provider: a SAVED one (enter makes an agent from it at once; `e` edits
//!    it; `d` makes it the default, or stops it being one) or a NEW one of the
//!    four (responses, anthropic, ollama, vllm — dimmed when this build does not
//!    link it);
//! 2. its details: a name (auto), the URL (filled), the model, the key (the
//!    stored one for the URL's host, a new one pasted, or none — and where keys
//!    are kept, the first time), the backend's REQUIRED fields, its optional
//!    ones under "advanced" — then CREATE & OPEN CHAT: saved under the name
//!    (the first becomes the default) and an AI agent made from it, its chat
//!    opened. Editing a saved one, the button SAVES it back.
//!
//! Keys: ↑ ↓ pick · enter edit / go · ← → choose (key, where keys go, an enum
//! field) · esc back (page 2) or leave (page 1). The pure part is `wizard.rs`;
//! the acts are `kern/wizard`'s, one call each.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::ev::Cmd;
use crate::kern::saved::{Saved, Shelf};
use crate::kern::setup::Backend;
use crate::panel::wizard::{own, provider_of, Details, KeySource, PROVIDERS};
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};
use crate::theme;

/// The tree row's id. `#` never appears in a kernel agent id.
pub const SETUP_ID: &str = "#setup";

/// A line of page 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum One {
    Saved(usize),
    Provider(usize),
}

/// A line of page 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Two {
    Name,
    Url,
    Model,
    Key,
    Secret,
    Keep,
    /// one of the backend's own fields, by name order in `own()`
    Field(usize),
    Advanced,
    Go,
    Back,
}

enum Page {
    One {
        at: usize,
    },
    Two {
        d: Details,
        at: usize,
        editing: Option<(Two, String)>,
    },
}

pub struct SetupPanel {
    /// `None` until the kernel said which backends it links (with schemas).
    backends: Option<Vec<Backend>>,
    shelf: Option<Result<Shelf, String>>,
    /// the stored keys' refs and hosts (never values)
    keys: Vec<(String, String)>,
    /// where keys are kept (`None` until chosen), once read
    key_mode: Option<Result<(Option<String>, String), String>>,
    page: Page,
    result: Option<Result<String, String>>,
    /// an act is on its way: no second one
    busy: bool,
}

impl SetupPanel {
    /// Tests: open a text field (the key's), and read what it holds.
    #[cfg(test)]
    pub fn open_key_prompt(&mut self) {
        let b = Backend {
            module: "x.tools".into(),
            fields: vec![],
            key: Some(true),
        };
        self.page = Page::Two {
            d: Details::new(&b, None, &[]),
            at: 0,
            editing: Some((Two::Secret, String::new())),
        };
        self.backends = Some(vec![b]);
    }
    #[cfg(test)]
    pub fn key_typed(&self) -> Option<&str> {
        match &self.page {
            Page::Two {
                editing: Some((_, buf)),
                ..
            } => Some(buf.as_str()),
            _ => None,
        }
    }

    pub fn new() -> Self {
        Self {
            backends: None,
            shelf: None,
            keys: Vec::new(),
            key_mode: None,
            page: Page::One { at: 0 },
            result: None,
            busy: false,
        }
    }

    pub fn set_backends(&mut self, b: Vec<Backend>) {
        self.backends = Some(b);
    }
    pub fn set_key_mode(&mut self, m: Result<(Option<String>, String), String>) {
        self.key_mode = Some(m);
    }
    pub fn set_shelf(&mut self, s: Result<Shelf, String>) {
        self.shelf = Some(s);
    }
    pub fn set_keys(&mut self, k: Result<Vec<(String, String)>, String>) {
        self.keys = k.unwrap_or_default();
    }

    /// A saved edit or a default came back: page 1 again on a saved edit.
    pub fn set_result(&mut self, r: Result<String, String>) {
        self.busy = false;
        if r.is_ok() && matches!(&self.page, Page::Two { d, .. } if d.edit.is_some()) {
            self.page = Page::One { at: 0 };
        }
        self.result = Some(r);
    }

    /// The wizard made an agent (the app opens its chat) — or why not.
    pub fn set_created(&mut self, r: &Result<String, String>) {
        self.busy = false;
        match r {
            Ok(id) => {
                self.page = Page::One { at: 0 };
                self.result = Some(Ok(format!("made `{id}` — its chat is open")));
            }
            Err(e) => self.result = Some(Err(e.clone())),
        }
    }

    fn mode_chosen(&self) -> bool {
        matches!(&self.key_mode, Some(Ok((Some(_), _))))
    }

    fn backend(&self, module: &str) -> Option<&Backend> {
        self.backends.as_ref()?.iter().find(|b| b.module == module)
    }

    /// The saved set-ups this build can run (an AI backend it links), in order.
    fn saved(&self) -> Vec<&Saved> {
        let Some(Ok(s)) = &self.shelf else {
            return Vec::new();
        };
        s.saved
            .iter()
            .filter(|t| self.backend(&t.handler()).is_some())
            .collect()
    }

    fn taken(&self) -> Vec<String> {
        match &self.shelf {
            Some(Ok(s)) => s.saved.iter().map(|t| t.name.clone()).collect(),
            _ => Vec::new(),
        }
    }

    fn preferred(&self) -> Option<&str> {
        match &self.shelf {
            Some(Ok(s)) => s.preferred.as_deref(),
            _ => None,
        }
    }

    fn ones(&self) -> Vec<One> {
        let mut v: Vec<One> = (0..self.saved().len()).map(One::Saved).collect();
        v.extend((0..PROVIDERS.len()).map(One::Provider));
        v
    }

    fn twos(&self, d: &Details) -> Vec<Two> {
        let Some(b) = self.backend(&d.module) else {
            return vec![Two::Back];
        };
        let mut v = vec![Two::Name, Two::Url, Two::Model];
        if let Some(s) = d.key_source(b, &self.keys) {
            v.push(Two::Key);
            if s == KeySource::New {
                v.push(Two::Secret);
                if !self.mode_chosen() {
                    v.push(Two::Keep);
                }
            }
        }
        let fields = own(b);
        v.extend(
            (0..fields.len())
                .filter(|i| fields[*i].required)
                .map(Two::Field),
        );
        if fields.iter().any(|f| !f.required) {
            v.push(Two::Advanced);
            if d.advanced {
                v.extend(
                    (0..fields.len())
                        .filter(|i| !fields[*i].required)
                        .map(Two::Field),
                );
            }
        }
        v.extend([Two::Go, Two::Back]);
        v
    }

    fn open(&mut self, module: &str, current: Option<&Saved>) {
        let taken = self.taken();
        match self.backend(module) {
            Some(b) => {
                let d = Details::new(b, current, &taken);
                self.page = Page::Two {
                    d,
                    at: 0,
                    editing: None,
                };
                self.result = None;
            }
            None => self.result = Some(Err(format!("this build links no {}", provider_of(module)))),
        }
    }

    fn page_one(&mut self, k: KeyCode, out: &mut Vec<Cmd>) -> Handled {
        let Page::One { at } = self.page else {
            return Handled::No;
        };
        let ones = self.ones();
        let n = ones.len();
        let line = ones.get(at.min(n.saturating_sub(1))).copied();
        match k {
            KeyCode::Up => {
                self.page = Page::One {
                    at: at.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                self.page = Page::One {
                    at: (at + 1).min(n.saturating_sub(1)),
                }
            }
            KeyCode::Enter if self.busy => {}
            KeyCode::Enter => match line {
                Some(One::Saved(i)) => {
                    let name = self.saved()[i].name.clone();
                    self.busy = true;
                    self.result = Some(Ok(format!("making an agent from `{name}`…")));
                    out.push(Cmd::WizardFromSaved { name });
                }
                Some(One::Provider(j)) => self.open(PROVIDERS[j].module, None),
                None => {}
            },
            KeyCode::Char('e') => {
                if let Some(One::Saved(i)) = line {
                    let s = self.saved()[i].clone();
                    self.open(&s.handler(), Some(&s));
                }
            }
            KeyCode::Char('d') => {
                if let Some(One::Saved(i)) = line {
                    let name = self.saved()[i].name.clone();
                    let is_default = self.preferred() == Some(name.as_str());
                    out.push(Cmd::Prefer {
                        name: (!is_default).then_some(name),
                    });
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn field_mut<'a>(d: &'a mut Details, b: &Backend, l: Two) -> Option<&'a mut String> {
        match l {
            Two::Name if d.edit.is_none() => Some(&mut d.name),
            Two::Url => Some(&mut d.url),
            Two::Model => Some(&mut d.model),
            Two::Secret => Some(&mut d.secret),
            Two::Field(i) => {
                let name = own(b).get(i)?.name.clone();
                Some(d.extra.entry(name).or_default())
            }
            _ => None,
        }
    }

    fn page_two(&mut self, k: KeyCode, out: &mut Vec<Cmd>) -> Handled {
        let keys = self.keys.clone();
        let mode_chosen = self.mode_chosen();
        let taken = self.taken();
        let backends = self.backends.clone().unwrap_or_default();
        let lines = match &self.page {
            Page::Two { d, .. } => self.twos(d),
            Page::One { .. } => return Handled::No,
        };
        let Page::Two { d, at, editing } = &mut self.page else {
            return Handled::No;
        };
        let Some(b) = backends.iter().find(|b| b.module == d.module) else {
            self.page = Page::One { at: 0 };
            return Handled::Yes;
        };
        // ── typing into a field ───────────────────────────────────────────
        if let Some((line, buf)) = editing.as_mut() {
            match k {
                KeyCode::Esc => *editing = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                KeyCode::Enter => {
                    let (line, text) = (*line, buf.clone());
                    *editing = None;
                    if let Some(f) = Self::field_mut(d, b, line) {
                        *f = text;
                    }
                }
                _ => {}
            }
            return Handled::Yes;
        }
        let line = lines[(*at).min(lines.len() - 1)];
        let fields = own(b);
        match k {
            KeyCode::Up => *at = at.saturating_sub(1),
            KeyCode::Down => *at = (*at + 1).min(lines.len() - 1),
            KeyCode::Esc => {
                self.page = Page::One { at: 0 };
                self.result = None;
            }
            KeyCode::Left | KeyCode::Right if line == Two::Key => {
                let offered = d.sources(b, &keys);
                let now = d
                    .key_source(b, &keys)
                    .and_then(|s| offered.iter().position(|o| *o == s))
                    .unwrap_or(0);
                let n = offered.len();
                let next = if k == KeyCode::Left {
                    (now + n - 1) % n
                } else {
                    (now + 1) % n
                };
                d.source = Some(offered[next]);
            }
            KeyCode::Left | KeyCode::Right if line == Two::Keep => {
                d.keep = if d.keep == "secure" {
                    "plain"
                } else {
                    "secure"
                }
            }
            KeyCode::Left | KeyCode::Right if matches!(line, Two::Field(i) if !fields[i].one_of.is_empty()) =>
            {
                if let Two::Field(i) = line {
                    let f = fields[i];
                    // "" — the backend's default — and each value
                    let mut opts: Vec<String> = vec![String::new()];
                    opts.extend(f.one_of.iter().cloned());
                    let now = opts
                        .iter()
                        .position(|o| Some(o) == d.extra.get(&f.name))
                        .unwrap_or(0);
                    let n = opts.len();
                    let next = if k == KeyCode::Left {
                        (now + n - 1) % n
                    } else {
                        (now + 1) % n
                    };
                    d.extra.insert(f.name.clone(), opts[next].clone());
                }
            }
            KeyCode::Enter if line == Two::Advanced => d.advanced = !d.advanced,
            KeyCode::Enter if line == Two::Back => {
                self.page = Page::One { at: 0 };
                self.result = None;
            }
            KeyCode::Enter if line == Two::Go => {
                if self.busy {
                    return Handled::Yes;
                }
                let m = d.missing(b, &keys, &taken);
                if !m.is_empty() {
                    self.result = Some(Err(format!("missing: {}", m.join(", "))));
                    return Handled::Yes;
                }
                let (texts, key) = (d.texts(b), d.key_in(b, &keys, mode_chosen));
                let module = d.module.clone();
                match d.edit.clone() {
                    Some(name) => out.push(Cmd::WizardSaveEdit {
                        name,
                        module,
                        texts,
                        key,
                    }),
                    None => out.push(Cmd::WizardCreate {
                        name: d.name.trim().to_string(),
                        module,
                        texts,
                        key,
                    }),
                }
                // the pasted key leaves the page with the act — never kept on it
                d.secret.clear();
                self.busy = true;
                self.result = Some(Ok("on its way…".into()));
            }
            KeyCode::Enter => {
                if let Some(f) = Self::field_mut(d, b, line) {
                    let text = if line == Two::Secret {
                        String::new()
                    } else {
                        f.clone()
                    };
                    *editing = Some((line, text));
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
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
        Needs {
            rows: false,
            reflect: false,
            watch: false,
        }
    }

    fn text_input(&self) -> bool {
        matches!(
            &self.page,
            Page::Two {
                editing: Some(_),
                ..
            }
        )
    }

    fn on_key(&mut self, k: KeyEvent, _cx: &Cx<'_>, out: &mut Vec<Cmd>) -> Handled {
        match self.page {
            Page::One { .. } => self.page_one(k.code, out),
            Page::Two { .. } => self.page_two(k.code, out),
        }
    }

    fn on_event(&mut self, _payload: &Value, _out: &mut Vec<Cmd>) -> bool {
        false
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.extend([Cmd::Backends, Cmd::KeyMode, Cmd::Saved, Cmd::Keys]);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        let dim = Style::default().fg(theme::DIM);
        let muted = Style::default().fg(theme::MUTED);
        let fg = Style::default().fg(theme::FG);
        let bold = Style::default().fg(theme::FG).add_modifier(Modifier::BOLD);
        let warn = Style::default()
            .fg(theme::BLOCKED)
            .add_modifier(Modifier::BOLD);
        let arrow = |c: bool| {
            Span::styled(
                if c { "  ▸ " } else { "    " },
                Style::default().fg(theme::ACCENT_HI),
            )
        };
        let pick = |c: bool| {
            if c {
                bold.add_modifier(Modifier::REVERSED)
            } else {
                bold
            }
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        match &self.page {
            Page::One { at } => {
                let ones = self.ones();
                let cur = ones.get((*at).min(ones.len().saturating_sub(1))).copied();
                lines.push(Line::from(vec![
                    Span::styled("  NEW AI AGENT", bold),
                    Span::styled("      1 / 2", dim),
                ]));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  You can set everything up here, or let your coding agent do it.",
                    muted,
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("  SAVED", dim)));
                let saved = self.saved();
                if saved.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "    Nothing saved yet — pick a provider below.",
                        dim,
                    )));
                }
                for (i, s) in saved.iter().enumerate() {
                    let c = cur == Some(One::Saved(i));
                    let star = if self.preferred() == Some(s.name.as_str()) {
                        "  ★ default"
                    } else {
                        ""
                    };
                    lines.push(Line::from(vec![
                        arrow(c),
                        Span::styled(format!(" {} ", s.name), pick(c)),
                        Span::styled(star.to_string(), Style::default().fg(theme::ACCENT_HI)),
                        Span::styled(
                            format!(
                                "   {} · {} · {}",
                                provider_of(&s.handler()),
                                s.model(),
                                s.endpoint()
                            ),
                            dim,
                        ),
                    ]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("  NEW", dim)));
                for (j, p) in PROVIDERS.iter().enumerate() {
                    let c = cur == Some(One::Provider(j));
                    let linked = self.backend(p.module).is_some();
                    lines.push(Line::from(vec![
                        arrow(c),
                        Span::styled(format!(" {} ", p.label), if linked { pick(c) } else { dim }),
                        Span::styled(
                            if linked {
                                String::new()
                            } else {
                                "   not in this build".into()
                            },
                            dim,
                        ),
                    ]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ↑ ↓ pick · enter: a saved one makes an agent now, a new one opens its details · e edit · d default · esc leave",
                    dim,
                )));
            }
            Page::Two { d, at, editing } => {
                let Some(b) = self.backend(&d.module) else {
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            "  this build links no such backend",
                            warn,
                        ))),
                        area,
                    );
                    return;
                };
                let twos = self.twos(d);
                let cur = twos[(*at).min(twos.len() - 1)];
                let title = match &d.edit {
                    Some(n) => format!("  EDIT SAVED · {n}"),
                    None => format!("  NEW AI AGENT · {}", provider_of(&d.module)),
                };
                lines.push(Line::from(vec![
                    Span::styled(title, bold),
                    Span::styled(if d.edit.is_some() { "" } else { "      2 / 2" }, dim),
                ]));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    if d.edit.is_some() {
                        "  Agents made from it from now on use these values; running ones keep theirs."
                    } else {
                        "  Check the values, then create it — you can change them later."
                    },
                    muted,
                )));
                lines.push(Line::from(""));
                let shown = |l: Two, value: String, secret: bool| -> (String, bool) {
                    match editing {
                        Some((e, buf)) if *e == l => (
                            format!(
                                "{}▌",
                                if secret {
                                    "•".repeat(buf.chars().count())
                                } else {
                                    buf.clone()
                                }
                            ),
                            false,
                        ),
                        _ if value.is_empty() => ("— not set —".into(), true),
                        _ => (
                            if secret {
                                "•".repeat(value.chars().count())
                            } else {
                                value
                            },
                            false,
                        ),
                    }
                };
                // the label column fits the longest label (a backend's own field names run long)
                let w = own(b)
                    .iter()
                    .map(|f| f.name.chars().count())
                    .max()
                    .unwrap_or(0)
                    .max(12)
                    + 2;
                let row = |l: Two,
                           label: &str,
                           value: String,
                           note: String,
                           required: bool,
                           secret: bool|
                 -> Line<'static> {
                    let c = cur == l;
                    let (text, empty) = shown(l, value, secret);
                    Line::from(vec![
                        arrow(c),
                        Span::styled(format!("{label:<w$}"), muted),
                        Span::styled(text, if empty && required { warn } else { pick(c) }),
                        Span::styled(format!("   {note}"), dim),
                    ])
                };
                let fields = own(b);
                for l in &twos {
                    match *l {
                        Two::Name => lines.push(row(
                            Two::Name,
                            "name",
                            d.name.clone(),
                            if d.edit.is_some() {
                                String::new()
                            } else {
                                "a name for reuse — it appears under SAVED".into()
                            },
                            true,
                            false,
                        )),
                        Two::Url => lines.push(row(
                            Two::Url,
                            "URL",
                            d.url.clone(),
                            "filled for you — change it for your own server".into(),
                            true,
                            false,
                        )),
                        Two::Model => {
                            let req = b.fields.iter().any(|f| f.name == "model" && f.required);
                            lines.push(row(
                                Two::Model,
                                "model",
                                d.model.clone(),
                                String::new(),
                                req,
                                false,
                            ));
                        }
                        Two::Key => {
                            let text = match d.key_source(b, &self.keys) {
                                Some(KeySource::Saved) => format!(
                                    "‹ saved key · {} ›",
                                    d.fit(&self.keys).unwrap_or_default()
                                ),
                                Some(KeySource::New) => "‹ paste a new key ›".into(),
                                _ => "‹ no key ›".into(),
                            };
                            lines.push(Line::from(vec![
                                arrow(cur == Two::Key),
                                Span::styled(format!("{:<w$}", "key"), muted),
                                Span::styled(text, pick(cur == Two::Key)),
                                Span::styled("   ← → saved / new / none", dim),
                            ]));
                        }
                        Two::Secret => lines.push(row(
                            Two::Secret,
                            "  value",
                            d.secret.clone(),
                            "bound to the URL's host; never shown again".into(),
                            true,
                            true,
                        )),
                        Two::Keep => lines.push(Line::from(vec![
                            arrow(cur == Two::Keep),
                            Span::styled(format!("{:<w$}", "  keep it in"), muted),
                            Span::styled(
                                if d.keep == "secure" {
                                    "‹ the keychain (secure) ›".to_string()
                                } else {
                                    "‹ a 0600 text file (plain) ›".to_string()
                                },
                                pick(cur == Two::Keep),
                            ),
                            Span::styled("   chosen once, for every key", dim),
                        ])),
                        Two::Field(i) => {
                            let fl = fields[i];
                            let v = d.extra.get(&fl.name).cloned().unwrap_or_default();
                            let mut note = fl.hint.clone();
                            if !fl.one_of.is_empty() {
                                note = format!("{note} — ← → {}", fl.one_of.join(" | "));
                            }
                            lines.push(row(
                                Two::Field(i),
                                &fl.name.replace('_', " "),
                                v,
                                note,
                                fl.required,
                                false,
                            ));
                        }
                        Two::Advanced => lines.push(Line::from(vec![
                            arrow(cur == Two::Advanced),
                            Span::styled(
                                format!("{} advanced", if d.advanced { "▾" } else { "▸" }),
                                pick(cur == Two::Advanced),
                            ),
                            Span::styled(
                                "   each starts at the backend's own default — enter shows them",
                                dim,
                            ),
                        ])),
                        Two::Go => {
                            lines.push(Line::from(""));
                            let ok = d.missing(b, &self.keys, &self.taken()).is_empty();
                            let label = if d.edit.is_some() {
                                " SAVE "
                            } else {
                                " CREATE & OPEN CHAT "
                            };
                            lines.push(Line::from(vec![
                                arrow(cur == Two::Go),
                                Span::styled(
                                    label,
                                    if ok && cur == Two::Go {
                                        Style::default()
                                            .fg(theme::FG)
                                            .bg(theme::ACCENT_HI)
                                            .add_modifier(Modifier::BOLD)
                                    } else if ok {
                                        bold
                                    } else {
                                        dim
                                    },
                                ),
                            ]));
                        }
                        Two::Back => lines.push(Line::from(vec![
                            arrow(cur == Two::Back),
                            Span::styled(" ‹ back ", pick(cur == Two::Back)),
                        ])),
                    }
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ↑ ↓ pick · enter edit / go · ← → choose · esc back",
                    dim,
                )));
            }
        }
        if let Some(r) = &self.result {
            lines.push(Line::from(""));
            lines.push(match r {
                Ok(s) => Line::from(Span::styled(
                    format!("  ✓ {s}"),
                    Style::default()
                        .fg(theme::WORKING)
                        .add_modifier(Modifier::BOLD),
                )),
                Err(e) => Line::from(Span::styled(
                    format!("  ✕ {e}"),
                    Style::default()
                        .fg(theme::FAILED)
                        .add_modifier(Modifier::BOLD),
                )),
            });
        }
        let _ = fg;
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
    use serde_json::json;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn drive(p: &mut SetupPanel, keys: &[KeyCode]) -> Vec<Cmd> {
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let a: crate::kern::model::AgentId = SETUP_ID.into();
        let cx = Cx {
            agent: &a,
            store: &store,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
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

    fn vllm() -> Backend {
        Backend::from_schema(
            "vllm_backend.tools",
            &json!({"fields": [
                {"name": "endpoint", "type": "str", "required": true},
                {"name": "model", "type": "str", "required": true},
                {"name": "max_tokens", "type": "int", "required": true},
                {"name": "temperature", "type": "float", "required": false, "default": 0.2},
                {"name": "credentials", "type": "credentials", "required": false},
            ]}),
        )
    }

    fn shelf(names: &[&str], preferred: Option<&str>) -> Shelf {
        Shelf {
            saved: names
                .iter()
                .map(|n| Saved {
                    name: (*n).into(),
                    version: String::new(),
                    record: json!({"handler_module": "vllm_backend.tools", "endpoint": "http://box:8000/v1", "model": "q", "max_tokens": 9})
                        .as_object()
                        .unwrap()
                        .clone(),
                })
                .collect(),
            preferred: preferred.map(str::to_string),
            dir: "/h/user/setups/ai_agent".into(),
        }
    }

    fn panel() -> SetupPanel {
        let mut p = SetupPanel::new();
        p.set_backends(vec![vllm()]);
        p.set_key_mode(Ok((Some("plain".into()), "a file".into())));
        p.set_shelf(Ok(shelf(&[], None)));
        p
    }

    #[test]
    fn page_one_lists_saved_then_the_four_providers_unlinked_ones_refused() {
        let mut p = panel();
        p.set_shelf(Ok(shelf(&["box"], Some("box"))));
        assert_eq!(
            p.ones(),
            [
                One::Saved(0),
                One::Provider(0),
                One::Provider(1),
                One::Provider(2),
                One::Provider(3)
            ]
        );
        // responses is not linked: enter says so, page 1 stays
        drive(&mut p, &[KeyCode::Down, KeyCode::Enter]);
        assert!(matches!(&p.page, Page::One { .. }));
        assert!(
            matches!(&p.result, Some(Err(e)) if e == "this build links no responses"),
            "{:?}",
            p.result
        );
    }

    #[test]
    fn a_saved_one_makes_an_agent_at_once_e_edits_it_d_toggles_the_default() {
        let mut p = panel();
        p.set_shelf(Ok(shelf(&["box"], Some("box"))));
        let out = drive(&mut p, &[KeyCode::Enter]);
        assert!(
            matches!(&out[..], [Cmd::WizardFromSaved { name }] if name == "box"),
            "{out:?}"
        );
        assert!(
            drive(&mut p, &[KeyCode::Enter]).is_empty(),
            "busy: no second act"
        );
        p.set_created(&Ok("box".into()));
        let out = drive(&mut p, &[KeyCode::Char('d')]);
        assert!(
            matches!(&out[..], [Cmd::Prefer { name: None }]),
            "the default stops being one: {out:?}"
        );
        drive(&mut p, &[KeyCode::Char('e')]);
        match &p.page {
            Page::Two { d, .. } => assert_eq!(
                (d.edit.as_deref(), d.url.as_str()),
                (Some("box"), "http://box:8000/v1")
            ),
            Page::One { .. } => panic!("e opens page 2"),
        }
    }

    #[test]
    fn a_new_provider_fills_page_two_and_create_sends_one_act() {
        let mut p = panel();
        // vllm is the 4th provider (no saved ones)
        drive(
            &mut p,
            &[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter],
        );
        let lines = match &p.page {
            Page::Two { d, .. } => {
                assert_eq!(
                    (d.name.as_str(), d.url.as_str()),
                    ("vllm-1", "http://127.0.0.1:8000/v1")
                );
                p.twos(d)
            }
            Page::One { .. } => panic!("page 2"),
        };
        assert_eq!(
            lines,
            [
                Two::Name,
                Two::Url,
                Two::Model,
                Two::Key,
                Two::Field(0),
                Two::Advanced,
                Two::Go,
                Two::Back
            ]
        );
        // GO with gaps: refused, naming them
        let go = lines.iter().position(|l| *l == Two::Go).unwrap();
        let mut ks = vec![KeyCode::Down; go];
        ks.push(KeyCode::Enter);
        assert!(drive(&mut p, &ks).is_empty());
        assert!(
            matches!(&p.result, Some(Err(e)) if e == "missing: the model, max tokens"),
            "{:?}",
            p.result
        );
        // fill model and max tokens, then GO
        let mut ks = vec![KeyCode::Up; go];
        ks.extend([KeyCode::Down, KeyCode::Down]);
        ks.extend(typed("qwen"));
        ks.extend([KeyCode::Down, KeyCode::Down]);
        ks.extend(typed("262144"));
        ks.extend([KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        let out = drive(&mut p, &ks);
        match &out[..] {
            [Cmd::WizardCreate {
                name,
                module,
                texts,
                key,
            }] => {
                assert_eq!(
                    (name.as_str(), module.as_str()),
                    ("vllm-1", "vllm_backend.tools")
                );
                assert_eq!(
                    Value::Object(texts.clone()),
                    json!({"endpoint": "http://127.0.0.1:8000/v1", "model": "qwen", "max_tokens": "262144"})
                );
                assert_eq!(*key, None, "an optional key left at none");
            }
            other => panic!("one create: {other:?}"),
        }
    }

    #[test]
    fn a_pasted_key_goes_with_the_act_masked_and_never_kept() {
        let mut p = panel();
        p.set_key_mode(Ok((None, String::new())));
        drive(
            &mut p,
            &[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter],
        );
        // key: ← → to "paste a new key" — its value and where keys go appear
        drive(
            &mut p,
            &[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Right],
        );
        let lines = match &p.page {
            Page::Two { d, .. } => p.twos(d),
            Page::One { .. } => panic!(),
        };
        assert_eq!(&lines[3..6], [Two::Key, Two::Secret, Two::Keep]);
        let mut ks = vec![KeyCode::Down];
        ks.extend(typed("sk-1"));
        ks.extend([KeyCode::Down, KeyCode::Right]); // keep it in: plain
        drive(&mut p, &ks);
        match &p.page {
            Page::Two { d, .. } => assert_eq!((d.secret.as_str(), d.keep), ("sk-1", "plain")),
            Page::One { .. } => panic!(),
        }
        if let Page::Two { d, .. } = &mut p.page {
            d.model = "q".into();
            d.extra.insert("max_tokens".into(), "9".into());
        }
        let go = match &p.page {
            Page::Two { d, .. } => p.twos(d).iter().position(|l| *l == Two::Go).unwrap(),
            Page::One { .. } => panic!(),
        };
        let mut ks = vec![KeyCode::Up; 20];
        ks.extend(vec![KeyCode::Down; go]);
        ks.push(KeyCode::Enter);
        let out = drive(&mut p, &ks);
        assert!(
            matches!(&out[..], [Cmd::WizardCreate { key: Some(k), .. }] if k.secret == "sk-1" && k.mode.as_deref() == Some("plain")),
            "{out:?}"
        );
        assert!(
            matches!(&p.page, Page::Two { d, .. } if d.secret.is_empty()),
            "the key left the page with the act"
        );
    }

    #[test]
    fn esc_goes_back_from_page_two_and_leaves_from_page_one() {
        let mut p = panel();
        drive(
            &mut p,
            &[
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Enter,
                KeyCode::Esc,
            ],
        );
        assert!(matches!(&p.page, Page::One { .. }));
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let a: crate::kern::model::AgentId = SETUP_ID.into();
        let cx = Cx {
            agent: &a,
            store: &store,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
        assert!(
            matches!(
                p.on_key(key(KeyCode::Esc), &cx, &mut Vec::new()),
                Handled::No
            ),
            "page 1: esc is the app's — leave"
        );
    }

    #[test]
    fn a_saved_edit_saves_back_under_its_name_and_returns_to_page_one() {
        let mut p = panel();
        p.set_shelf(Ok(shelf(&["box"], None)));
        drive(&mut p, &[KeyCode::Char('e')]);
        let go = match &p.page {
            Page::Two { d, .. } => p.twos(d).iter().position(|l| *l == Two::Go).unwrap(),
            Page::One { .. } => panic!(),
        };
        let mut ks = vec![KeyCode::Down; go];
        ks.push(KeyCode::Enter);
        let out = drive(&mut p, &ks);
        assert!(
            matches!(&out[..], [Cmd::WizardSaveEdit { name, .. }] if name == "box"),
            "{out:?}"
        );
        p.set_result(Ok("saved".into()));
        assert!(matches!(&p.page, Page::One { .. }));
    }
}
