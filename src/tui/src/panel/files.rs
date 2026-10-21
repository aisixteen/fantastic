//! A CLAMPED DISK SURFACE, browsed.
//!
//! `file_bridge` is the only thing in the kernel that opens a file, and its clamp
//! is not bypassable: a path outside the agent's declared root is refused, not
//! normalised. So this panel never tries to be clever about paths — it sends what
//! the human picked and renders whatever comes back, refusal included.
//!
//! Listing is a verb on the agent itself. Reading a file's bytes is a separate
//! separate concern, and a directory listing is all this pane
//! promises; opening a file hands it to the docs panel's reader.

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

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: Option<u64>,
}

pub struct FilesPanel {
    agent: AgentId,
    /// Relative to the agent's own root, always. An absolute path here would be
    /// refused by the clamp, and rightly.
    pub path: String,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    /// A refusal, shown in place. Not an exception, not a retry.
    pub note: Option<String>,
    /// The file the human opened, with the lines the kernel returned.
    pub open: Option<(String, Vec<String>)>,
    pub scroll: u16,
}

impl FilesPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            path: ".".into(),
            entries: Vec::new(),
            cursor: 0,
            note: None,
            open: None,
            scroll: 0,
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
                name: v
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
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
}

impl Panel for FilesPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Files
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
        _cx: &Cx<'_>,
        out: &mut Vec<Cmd>,
    ) -> Handled {
        use crossterm::event::KeyCode as K;
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

    fn on_event(&mut self, _payload: &Value, _out: &mut Vec<Cmd>) -> bool {
        false
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        out.push(self.list_cmd());
    }

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        if reply.get("lines").is_some() {
            self.absorb_read(reply)
        } else {
            self.absorb_list(reply)
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
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
        Some(format!(
            "{} · {} · {} entries",
            self.agent,
            self.path,
            self.entries.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> FilesPanel {
        FilesPanel::new("files".into())
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
        f.absorb_list(
            &json!({"files":[{"name":"a","type":"dir"},{"name":"b","type":"file","size":12}]}),
        );
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
}
