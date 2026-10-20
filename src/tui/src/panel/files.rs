//! A CLAMPED DISK SURFACE, browsed, and the file and folder AGENTS over it (#150).
//!
//! `file_bridge` is the only thing in the kernel that opens a file, and its clamp
//! is not bypassable: a path outside the agent's declared root is refused, not
//! normalised. So this panel never tries to be clever about paths — it sends what
//! the human picked and renders whatever comes back, refusal included.
//!
//! The panel has three modes, told apart by what the agent says it is (its reflect):
//!
//! - a BRIDGE (`file_bridge` itself): browse its root, read files through it;
//! - a FOLDER agent: browse that folder through its own view, and re-list when the
//!   folder's entries change (`added` / `changed` / `removed`);
//! - a FILE agent: view one file through its own view, edit it, and save with
//!   `write` against the version read, so a save that lost a race is refused.
//!
//! A folder or a file agent is `file_bridge`'s own view (`file_bridge/views.py`), so
//! these modes send the same verbs the canvas does and nothing else.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::{json, Value};

use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};
use crate::theme;

/// Lines a file view reads at once (the bridge's own page size for a viewer).
const PAGE: u64 = 2000;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: Option<u64>,
}

/// What the agent is, as its reflect says (`kind` is `file` or `folder` for a view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Its reflect has not arrived yet: the panel asks, and draws nothing else.
    Undecided,
    /// A `file_bridge` itself: browse its root.
    Bridge,
    /// A folder agent: browse the folder, live.
    Folder,
    /// A file agent: view and edit the file, live.
    File,
}

pub struct FilesPanel {
    agent: AgentId,
    mode: Mode,
    /// Relative to the agent's own root, always. An absolute path here would be
    /// refused by the clamp, and rightly.
    pub path: String,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    /// A refusal, shown in place. Not an exception, not a retry.
    pub note: Option<String>,
    /// The file the human opened, with the lines the kernel returned. A file agent
    /// shows its file here too.
    pub open: Option<(String, Vec<String>)>,
    pub scroll: u16,
    /// A file agent's own path (its reflect's `path`): what it views and writes.
    view_path: String,
    /// The version of the file as it was last read: a save names it (`if_match`).
    version: Option<String>,
    /// The lines the human is typing, while editing.
    editing: bool,
    buf: Vec<String>,
    row: usize,
    /// The cursor's column, in characters.
    col: usize,
    dirty: bool,
    /// A `write` is in flight: its reply is the save's answer.
    saving: bool,
    /// Commands the decision or a change asked for, drained after `layout`.
    pending: Vec<Cmd>,
}

impl FilesPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            mode: Mode::Undecided,
            path: ".".into(),
            entries: Vec::new(),
            cursor: 0,
            note: None,
            open: None,
            scroll: 0,
            view_path: String::new(),
            version: None,
            editing: false,
            buf: Vec::new(),
            row: 0,
            col: 0,
            dirty: false,
            saving: false,
            pending: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The reflect says what the agent is: a folder or a file view, or a bridge. Makes
    /// the first call the mode needs, and returns it.
    fn decide(&mut self, reflect: &Value) -> Vec<Cmd> {
        match reflect.get("kind").and_then(Value::as_str) {
            Some("file") => {
                self.mode = Mode::File;
                self.view_path = reflect
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                vec![self.read_cmd()]
            }
            Some("folder") => {
                self.mode = Mode::Folder;
                self.path = ".".into();
                vec![self.list_cmd()]
            }
            _ => {
                self.mode = Mode::Bridge;
                vec![self.list_cmd()]
            }
        }
    }

    /// The `list` reply. Shapes differ between kernels, so both the flat name list
    /// and the record list are read — and an unknown shape leaves a note rather
    /// than an empty pane that looks like an empty directory.
    pub fn absorb_list(&mut self, reply: &Value) -> bool {
        if let Some(e) = reply.get("error").and_then(Value::as_str) {
            self.note = Some(e.to_string());
            return true;
        }
        let Some(items) = reply.get("files").and_then(Value::as_array) else {
            self.note = Some(format!("the list reply had no `files`: {reply}"));
            return true;
        };
        self.entries = items
            .iter()
            .map(|v| Entry {
                name: v.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
                dir: v.get("type").and_then(Value::as_str) == Some("dir"),
                size: v.get("size").and_then(Value::as_u64),
            })
            .collect();
        // Directories first, then names: a listing a human can scan.
        self.entries
            .sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.cmp(&b.name)));
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        self.note = None;
        true
    }

    pub fn absorb_read(&mut self, reply: &Value) -> bool {
        if let Some(e) = reply.get("error").and_then(Value::as_str) {
            self.note = Some(e.to_string());
            return true;
        }
        let Some(lines) = reply.get("lines").and_then(Value::as_array) else {
            return false;
        };
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        let name = reply
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        self.version = reply.get("version").and_then(Value::as_str).map(str::to_string);
        self.open = Some((name, text));
        self.scroll = 0;
        true
    }

    fn list_cmd(&self) -> Cmd {
        Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type":"list","path":self.path}),
        }
    }

    /// A file agent's whole text (up to a page; a longer file is said so by the bridge).
    fn read_cmd(&self) -> Cmd {
        Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type":"read","path":self.view_path,"limit":PAGE}),
        }
    }

    /// The save: the whole document, against the version it was read at.
    fn save_cmd(&self) -> Cmd {
        let mut text = self.buf.join("\n");
        text.push('\n');
        Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type":"write","path":self.view_path,"bytes":text,"if_match":self.version}),
        }
    }

    /// Join, without ever producing an absolute path or escaping above the root:
    /// the clamp would refuse either, and a pane that builds paths the kernel must
    /// reject is just a slower way to show an error.
    fn join(&self, name: &str) -> String {
        match self.path.as_str() {
            "." | "" => name.to_string(),
            p => format!("{}/{}", p.trim_end_matches('/'), name),
        }
    }

    fn up(&self) -> String {
        match self.path.rsplit_once('/') {
            Some((head, _)) if !head.is_empty() => head.to_string(),
            _ => ".".to_string(),
        }
    }

    // ─── the file agent's editor ─────────────────────────────────────────

    /// The line the cursor is on, as its characters.
    fn line_len(&self) -> usize {
        self.buf.get(self.row).map_or(0, |l| l.chars().count())
    }

    /// The byte offset of character `col` on the cursor's line.
    fn byte_at(&self, col: usize) -> usize {
        self.buf
            .get(self.row)
            .and_then(|l| l.char_indices().nth(col).map(|(i, _)| i).or(Some(l.len())))
            .unwrap_or(0)
    }

    fn insert(&mut self, c: char) {
        let at = self.byte_at(self.col);
        if let Some(line) = self.buf.get_mut(self.row) {
            line.insert(at, c);
        }
        self.col += 1;
        self.dirty = true;
    }

    fn newline(&mut self) {
        let at = self.byte_at(self.col);
        let rest = match self.buf.get_mut(self.row) {
            Some(line) => line.split_off(at),
            None => String::new(),
        };
        self.row += 1;
        self.buf.insert(self.row, rest);
        self.col = 0;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let at = self.byte_at(self.col - 1);
            if let Some(line) = self.buf.get_mut(self.row) {
                line.remove(at);
            }
            self.col -= 1;
            self.dirty = true;
        } else if self.row > 0 {
            let line = self.buf.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            if let Some(prev) = self.buf.get_mut(self.row) {
                prev.push_str(&line);
            }
            self.dirty = true;
        }
    }

    fn move_to(&mut self, dr: isize, dc: isize) {
        if dr != 0 {
            let row = (self.row as isize + dr).clamp(0, self.buf.len().saturating_sub(1) as isize);
            self.row = row as usize;
            self.col = self.col.min(self.line_len());
        }
        if dc < 0 {
            if self.col > 0 {
                self.col -= 1;
            } else if self.row > 0 {
                self.row -= 1;
                self.col = self.line_len();
            }
        } else if dc > 0 {
            if self.col < self.line_len() {
                self.col += 1;
            } else if self.row + 1 < self.buf.len() {
                self.row += 1;
                self.col = 0;
            }
        }
    }

    fn file_key(&mut self, k: crossterm::event::KeyEvent, out: &mut Vec<Cmd>) -> Handled {
        use crossterm::event::{KeyCode as K, KeyModifiers as M};
        let ctrl = k.modifiers.contains(M::CONTROL);
        if self.editing {
            match k.code {
                K::Esc => {
                    self.editing = false;
                    self.buf.clear();
                    self.dirty = false;
                    self.note = Some("edit discarded: the file is as it was".into());
                }
                K::Char('s') if ctrl => {
                    self.saving = true;
                    out.push(self.save_cmd());
                }
                K::Char(c) if !ctrl => self.insert(c),
                K::Enter => self.newline(),
                K::Backspace => self.backspace(),
                K::Left => self.move_to(0, -1),
                K::Right => self.move_to(0, 1),
                K::Up => self.move_to(-1, 0),
                K::Down => self.move_to(1, 0),
                K::Home => self.col = 0,
                K::End => self.col = self.line_len(),
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        match k.code {
            K::Char('e') => {
                let lines = self.open.as_ref().map(|(_, l)| l.clone()).unwrap_or_default();
                self.buf = if lines.is_empty() { vec![String::new()] } else { lines };
                self.row = 0;
                self.col = 0;
                self.editing = true;
                self.dirty = false;
            }
            K::Down | K::Char('j') => self.scroll = self.scroll.saturating_add(1),
            K::Up | K::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            K::PageDown => self.scroll = self.scroll.saturating_add(20),
            K::PageUp => self.scroll = self.scroll.saturating_sub(20),
            K::Char('r') => out.push(self.read_cmd()),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn file_render(&self, f: &mut Frame<'_>, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        let title = if self.editing {
            format!("  {} · editing{}", self.view_path, if self.dirty { " · unsaved" } else { "" })
        } else {
            format!("  {}", self.view_path)
        };
        lines.push(Line::from(Span::styled(
            title,
            Style::default().fg(theme::LABEL).add_modifier(Modifier::BOLD),
        )));
        if let Some(n) = &self.note {
            lines.push(Line::from(Span::styled(format!("  {n}"), Style::default().fg(theme::FAILED))));
        }
        let text: Vec<String> = if self.editing {
            self.buf.clone()
        } else {
            self.open.as_ref().map(|(_, l)| l.clone()).unwrap_or_default()
        };
        for (i, l) in text.iter().enumerate() {
            // the cursor is a bar drawn into its own line while editing
            let shown = if self.editing && i == self.row {
                let at = self.byte_at(self.col);
                format!("{}│{}", &l[..at], &l[at..])
            } else {
                l.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:>5} ", i + 1), Style::default().fg(theme::DIM)),
                Span::styled(shown, Style::default().fg(theme::FG)),
            ]));
        }
        if text.is_empty() {
            lines.push(Line::from(Span::styled("  (empty file)", Style::default().fg(theme::DIM))));
        }
        f.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), area);
    }
}

impl Panel for FilesPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Files
    }

    fn needs(&self) -> Needs {
        Needs {
            rows: false,
            reflect: true,
            // a view is live: its folder re-lists, its file re-reads, on what changes
            watch: matches!(self.mode, Mode::Folder | Mode::File),
        }
    }

    fn on_key(
        &mut self,
        k: crossterm::event::KeyEvent,
        _cx: &Cx<'_>,
        out: &mut Vec<Cmd>,
    ) -> Handled {
        use crossterm::event::KeyCode as K;
        if self.mode == Mode::File {
            return self.file_key(k, out);
        }
        if self.open.is_some() {
            match k.code {
                K::Esc | K::Char('q') => {
                    self.open = None;
                    return Handled::Yes;
                }
                K::Down | K::Char('j') => {
                    self.scroll = self.scroll.saturating_add(1);
                    return Handled::Yes;
                }
                K::Up | K::Char('k') => {
                    self.scroll = self.scroll.saturating_sub(1);
                    return Handled::Yes;
                }
                K::PageDown => {
                    self.scroll = self.scroll.saturating_add(20);
                    return Handled::Yes;
                }
                K::PageUp => {
                    self.scroll = self.scroll.saturating_sub(20);
                    return Handled::Yes;
                }
                _ => return Handled::No,
            }
        }
        match k.code {
            K::Down | K::Char('j') => {
                if !self.entries.is_empty() {
                    self.cursor = (self.cursor + 1).min(self.entries.len() - 1);
                }
                Handled::Yes
            }
            K::Up | K::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                Handled::Yes
            }
            K::Enter | K::Right | K::Char('l') => {
                let Some(e) = self.entries.get(self.cursor).cloned() else {
                    return Handled::Yes;
                };
                if e.dir {
                    self.path = self.join(&e.name);
                    self.cursor = 0;
                    out.push(self.list_cmd());
                } else if self.mode == Mode::Folder {
                    // a folder agent lists; a file in it is its own agent (a child of
                    // the bridge), which the agent tree selects
                    self.note = Some(format!(
                        "{} is a file: select its agent in the tree to view and edit it",
                        e.name
                    ));
                } else {
                    out.push(Cmd::Send {
                        target: self.agent.clone(),
                        payload: json!({"type":"read","path":self.join(&e.name),"limit":2000}),
                    });
                }
                Handled::Yes
            }
            K::Left | K::Char('h') => {
                self.path = self.up();
                self.cursor = 0;
                out.push(self.list_cmd());
                Handled::Yes
            }
            K::Char('r') => {
                out.push(self.list_cmd());
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn on_event(&mut self, payload: &Value, out: &mut Vec<Cmd>) -> bool {
        match self.mode {
            Mode::Folder => {
                // a folder's entries came, went or changed: the listing is read again
                let t = payload.get("type").and_then(Value::as_str);
                if matches!(t, Some("added" | "changed" | "removed")) {
                    out.push(self.list_cmd());
                    return true;
                }
                false
            }
            Mode::File => {
                if payload.get("type").and_then(Value::as_str) != Some("changed") {
                    return false;
                }
                if self.editing {
                    // the edits are kept; a save names the version it read, so one that
                    // lost the race is refused, never written over
                    self.note = Some("changed on disk while you edit: saving will refuse if it moved on".into());
                } else {
                    out.push(self.read_cmd());
                }
                true
            }
            _ => false,
        }
    }

    fn on_select(&mut self, cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        match (self.mode, cx.reflect) {
            (Mode::Undecided, Some(r)) => out.extend(self.decide(r)),
            (Mode::Undecided, None) => {}
            (Mode::File, _) => out.push(self.read_cmd()),
            (Mode::Bridge | Mode::Folder, _) => out.push(self.list_cmd()),
        }
    }

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        if let Some(e) = reply.get("error").and_then(Value::as_str) {
            // a refused save is still the save's answer: the document stays as edited
            self.saving = false;
            self.note = Some(e.to_string());
            return true;
        }
        if self.saving && reply.get("version").is_some() && reply.get("lines").is_none() {
            // the save landed: the text it wrote is the file now, at the new version
            self.saving = false;
            self.editing = false;
            self.version = reply.get("version").and_then(Value::as_str).map(str::to_string);
            let saved = std::mem::take(&mut self.buf);
            self.open = Some((self.view_path.clone(), saved));
            self.dirty = false;
            self.note = Some("saved".into());
            return true;
        }
        if reply.get("lines").is_some() {
            self.absorb_read(reply)
        } else {
            self.absorb_list(reply)
        }
    }

    fn text_input(&self) -> bool {
        self.editing
    }

    fn layout(&mut self, _area: Rect, cx: &Cx<'_>) {
        // the agent's reflect arrives after the panel is picked: decide once it has
        if self.mode == Mode::Undecided {
            if let Some(r) = cx.reflect {
                let cmds = self.decide(r);
                self.pending.extend(cmds);
            }
        }
    }

    fn take_cmds(&mut self) -> Vec<Cmd> {
        std::mem::take(&mut self.pending)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        if self.mode == Mode::Undecided {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  asking {} what it is…", self.agent),
                    Style::default().fg(theme::DIM),
                ))),
                area,
            );
            return;
        }
        if self.mode == Mode::File {
            self.file_render(f, area);
            return;
        }
        let mut lines: Vec<Line> = Vec::new();
        if let Some((name, text)) = &self.open {
            lines.push(Line::from(Span::styled(
                format!("  {name}"),
                Style::default()
                    .fg(theme::LABEL)
                    .add_modifier(Modifier::BOLD),
            )));
            for (i, l) in text.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:>5} ", i + 1), Style::default().fg(theme::DIM)),
                    Span::styled(l.clone(), Style::default().fg(theme::FG)),
                ]));
            }
            f.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), area);
            return;
        }

        lines.push(Line::from(Span::styled(
            format!("  {}", self.path),
            Style::default()
                .fg(theme::LABEL)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(n) = &self.note {
            lines.push(Line::from(Span::styled(
                format!("  {n}"),
                Style::default().fg(theme::FAILED),
            )));
        }
        for (i, e) in self.entries.iter().enumerate() {
            let sel = i == self.cursor;
            let style = if sel {
                Style::default()
                    .fg(theme::SELECT)
                    .add_modifier(Modifier::BOLD)
            } else if e.dir {
                Style::default().fg(theme::PROMPT)
            } else {
                Style::default().fg(theme::FG)
            };
            let size = e.size.map(|s| format!("  {s}")).unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} {}{}{}",
                    if sel { theme::FOLDED } else { ' ' },
                    e.name,
                    if e.dir { "/" } else { "" },
                    size
                ),
                style,
            )));
        }
        if self.entries.is_empty() && self.note.is_none() {
            lines.push(Line::from(Span::styled(
                "  empty, or the listing has not arrived yet.",
                Style::default().fg(theme::DIM),
            )));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(match self.mode {
            Mode::Undecided => format!("{} · asking", self.agent),
            Mode::File => format!(
                "{} · {} · file{}",
                self.agent,
                self.view_path,
                if self.editing { " · editing" } else { "" }
            ),
            Mode::Folder => format!("{} · {} · folder · {} entries", self.agent, self.path, self.entries.len()),
            Mode::Bridge => format!("{} · {} · {} entries", self.agent, self.path, self.entries.len()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn p() -> FilesPanel {
        FilesPanel::new("files".into())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Feed one key to a panel, with no context a key handler reads.
    fn press(f: &mut FilesPanel, k: KeyEvent) -> (Handled, Vec<Cmd>) {
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let agent: AgentId = "files".into();
        let cx = Cx { agent: &agent, store: &store, tasks: &[], seen: &seen, reflect: None };
        let mut out = Vec::new();
        let h = f.on_key(k, &cx, &mut out);
        (h, out)
    }

    /// A file agent's panel, reflected and read: the text is `text`, at version `v`.
    fn file_view(text: &str, v: &str) -> FilesPanel {
        let mut f = FilesPanel::new("fv".into());
        let r = json!({"id":"fv","kind":"file","path":"docs/a.md","bridge":"files"});
        let _ = f.decide(&r);
        let lines: Vec<Value> = text.lines().enumerate().map(|(i, t)| json!({"n": i + 1, "text": t})).collect();
        f.absorb_read(&json!({"path":"docs/a.md","lines":lines,"version":v,"eof":true}));
        f
    }

    #[test]
    fn a_listing_is_read_with_directories_first() {
        let mut f = p();
        f.absorb_list(&json!({"files":[
            {"name":"z.txt","type":"file","size":1},
            {"name":"src","type":"dir"},
            {"name":"a.md","type":"file","size":2},
        ]}));
        assert_eq!(
            f.entries
                .iter()
                .map(|e| (e.name.as_str(), e.dir))
                .collect::<Vec<_>>(),
            vec![("src", true), ("a.md", false), ("z.txt", false)]
        );
    }

    #[test]
    fn a_record_list_keeps_its_sizes() {
        let mut f = p();
        f.absorb_list(&json!({"files":[{"name":"a","type":"dir"},{"name":"b","type":"file","size":12}]}));
        assert!(f.entries[0].dir);
        assert_eq!(f.entries[1].size, Some(12));
    }

    #[test]
    fn a_refusal_is_shown_in_place_not_swallowed() {
        let mut f = p();
        f.absorb_list(&json!({"error":"files: outside the root","reason":"bad_arg"}));
        assert_eq!(f.note.as_deref(), Some("files: outside the root"));
    }

    #[test]
    fn an_unrecognised_reply_says_so_rather_than_looking_empty() {
        let mut f = p();
        f.absorb_list(&json!({"somethingelse":[]}));
        assert!(f.note.unwrap().contains("no `files`"));
    }

    #[test]
    fn paths_are_relative_and_never_climb_out_of_the_root() {
        let mut f = p();
        assert_eq!(f.join("a.md"), "a.md");
        f.path = "src/lib".into();
        assert_eq!(f.join("x.rs"), "src/lib/x.rs");
        assert_eq!(f.up(), "src");
        f.path = "src".into();
        assert_eq!(f.up(), ".", "the root is the ceiling");
        f.path = ".".into();
        assert_eq!(f.up(), ".");
    }

    #[test]
    fn opening_a_file_keeps_its_lines_and_esc_closes_it() {
        let mut f = p();
        assert!(f.absorb_read(&json!({"path":"a.md","lines":[{"n":1,"text":"hi"}],"total":1})));
        assert_eq!(f.open.as_ref().unwrap().1, vec!["hi"]);
    }

    // ─── #150: the mode is what the agent says it is ─────────────────────────

    #[test]
    fn a_folder_view_is_told_apart_from_a_bridge_and_a_file_view() {
        let mut folder = p();
        let cmds = folder.decide(&json!({"id":"fo","kind":"folder","path":"docs"}));
        assert_eq!(folder.mode(), Mode::Folder);
        assert_eq!(cmds.len(), 1, "one list, to start");

        let mut file = p();
        let cmds = file.decide(&json!({"id":"fv","kind":"file","path":"docs/a.md"}));
        assert_eq!(file.mode(), Mode::File);
        assert!(matches!(&cmds[0], Cmd::Send { payload, .. } if payload["type"] == "read" && payload["path"] == "docs/a.md"));

        let mut bridge = p();
        bridge.decide(&json!({"id":"files","root":"."}));
        assert_eq!(bridge.mode(), Mode::Bridge, "a reflect with no kind is the bridge itself");
    }

    #[test]
    fn a_folder_view_relists_when_its_entries_change_and_asks_for_watch() {
        let mut f = p();
        f.decide(&json!({"id":"fo","kind":"folder","path":"docs"}));
        assert!(f.needs().watch, "a folder view is live");
        for t in ["added", "changed", "removed"] {
            let mut out = Vec::new();
            assert!(f.on_event(&json!({"type": t, "path": "docs/x", "name": "x"}), &mut out), "{t}");
            assert!(matches!(&out[0], Cmd::Send { payload, .. } if payload["type"] == "list"), "{t} lists again");
        }
        let mut out = Vec::new();
        assert!(!f.on_event(&json!({"type":"bogus"}), &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn a_bridge_is_not_live_and_a_file_view_is() {
        let mut bridge = p();
        bridge.decide(&json!({"id":"files","root":"."}));
        assert!(!bridge.needs().watch);
        assert!(file_view("x", "v").needs().watch);
    }

    #[test]
    fn an_undecided_panel_asks_the_agent_and_says_so() {
        let mut f = p();
        assert_eq!(f.mode(), Mode::Undecided);
        let agent: AgentId = "fv".into();
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let cx = Cx { agent: &agent, store: &store, tasks: &[], seen: &seen, reflect: None };
        let mut out = Vec::new();
        f.on_select(&cx, &mut out);
        assert!(matches!(out[0], Cmd::Reflect { .. }), "it asks what the agent is");
        assert_eq!(out.len(), 1, "and lists nothing until it knows");
    }

    // ─── #150: a file view edits its file, and saves against the version read ──

    #[test]
    fn a_file_view_edits_and_a_save_names_the_version_it_read() {
        let mut f = file_view("ab\ncd", "v1");
        press(&mut f, key(KeyCode::Char('e')));
        assert!(f.text_input(), "editing takes the typed keys");
        press(&mut f, key(KeyCode::End));
        press(&mut f, key(KeyCode::Char('X')));
        press(&mut f, key(KeyCode::Enter));
        press(&mut f, key(KeyCode::Char('Y')));
        let (_, out) = press(&mut f, ctrl('s'));
        match &out[0] {
            Cmd::Send { payload, .. } => {
                assert_eq!(payload["type"], "write");
                assert_eq!(payload["path"], "docs/a.md");
                assert_eq!(payload["bytes"], "abX\nY\ncd\n", "Enter at the end of a line opens the next one");
                assert_eq!(payload["if_match"], "v1", "the version read, so a changed file is refused");
            }
            other => panic!("a save sends a write, got {other:?}"),
        }
    }

    #[test]
    fn a_save_that_lands_is_the_file_now_and_leaves_editing() {
        let mut f = file_view("ab", "v1");
        press(&mut f, key(KeyCode::Char('e')));
        press(&mut f, key(KeyCode::Char('!')));
        press(&mut f, ctrl('s'));
        let mut out = Vec::new();
        f.on_reply(&json!({"path":"docs/a.md","size":4,"version":"v2"}), &mut out);
        assert!(!f.text_input(), "saved: out of the editor");
        assert_eq!(f.open.as_ref().unwrap().1, vec!["!ab"]);
        assert_eq!(f.note.as_deref(), Some("saved"));
        assert_eq!(f.version.as_deref(), Some("v2"));
    }

    #[test]
    fn a_refused_save_keeps_the_edits_and_says_why() {
        let mut f = file_view("ab", "v1");
        press(&mut f, key(KeyCode::Char('e')));
        press(&mut f, key(KeyCode::Char('!')));
        press(&mut f, ctrl('s'));
        let mut out = Vec::new();
        f.on_reply(&json!({"error":"version mismatch — the document changed since you read it","reason":"version_mismatch"}), &mut out);
        assert!(f.text_input(), "still editing: nothing typed is lost");
        assert!(f.note.unwrap().contains("version mismatch"));
    }

    #[test]
    fn typing_moves_and_deletes_across_lines() {
        let mut f = file_view("ab\ncd", "v1");
        press(&mut f, key(KeyCode::Char('e')));
        press(&mut f, key(KeyCode::Down));
        press(&mut f, key(KeyCode::Home));
        press(&mut f, key(KeyCode::Backspace)); // joins the lines: "abcd"
        assert_eq!(f.buf, vec!["abcd"]);
        press(&mut f, key(KeyCode::Right)); // the join left the cursor after "ab": now after "abc"
        press(&mut f, key(KeyCode::Enter)); // splits off "d"
        assert_eq!(f.buf, vec!["abc", "d"]);
        press(&mut f, key(KeyCode::Esc));
        assert!(!f.text_input(), "Esc leaves the editor");
        assert_eq!(f.open.as_ref().unwrap().1, vec!["ab", "cd"], "unsaved edits are discarded, not written");
    }

    #[test]
    fn a_change_on_disk_rereads_a_file_view_unless_it_is_being_edited() {
        let mut f = file_view("ab", "v1");
        let mut out = Vec::new();
        assert!(f.on_event(&json!({"type":"changed","path":"docs/a.md","kind":"modified"}), &mut out));
        assert!(matches!(&out[0], Cmd::Send { payload, .. } if payload["type"] == "read"));
        press(&mut f, key(KeyCode::Char('e')));
        let mut out = Vec::new();
        assert!(f.on_event(&json!({"type":"changed","path":"docs/a.md","kind":"modified"}), &mut out));
        assert!(out.is_empty(), "an edit in progress is never replaced under the person");
        assert!(f.note.unwrap().contains("changed on disk"));
    }

    #[test]
    fn a_file_view_is_read_through_its_own_path_and_never_another() {
        let f = file_view("x", "v");
        assert_eq!(f.view_path, "docs/a.md");
        let cmd = f.read_cmd();
        assert!(matches!(cmd, Cmd::Send { payload, .. } if payload["path"] == "docs/a.md"));
    }
}
