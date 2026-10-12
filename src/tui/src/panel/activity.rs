//! ACTIVITY — every open question to the human, across the whole kernel.
//!
//! Not an agent: a view over all of them. It sits in the tree right under the
//! kernel's own row, with the open count as its badge. Questions never block a
//! conversation, so this page is where the human finds them without having to
//! visit every agent: OPEN (still waiting on an answer) and ANSWERED (answered
//! during this session). `Enter` jumps to the agent that asked, with the cursor
//! on the question, where the answer is an ordinary reply.
//!
//! Derived, never stored: "open" is `thread::waiting_for` over every agent, the
//! same function the tree's presence and the thread badge use, so the three can
//! never disagree.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::ev::Cmd;
use crate::kern::model::{AgentId, RowId};
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};
use crate::theme;
use crate::thread::{self, Waiting};

/// The tree row's id. `#` never appears in a kernel agent id, so it cannot
/// collide with one.
pub const ACTIVITY_ID: &str = "#activity";

/// How many answered questions the page remembers for this session.
const ANSWERED_KEPT: usize = 50;

pub struct ActivityPanel {
    /// Every agent in the tree, handed over by the app whenever the tree moves.
    agents: Vec<AgentId>,
    open: Vec<(AgentId, Waiting)>,
    answered: Vec<(AgentId, Waiting)>,
    cursor: usize,
}

impl ActivityPanel {
    pub fn new() -> Self {
        Self { agents: Vec::new(), open: Vec::new(), answered: Vec::new(), cursor: 0 }
    }

    /// The app calls this on every reflow: the page reads every agent's rows,
    /// and only the app knows which agents exist.
    pub fn set_agents(&mut self, agents: Vec<AgentId>) {
        self.agents = agents;
    }

    /// Open questions right now, newest first.
    pub fn open(&self) -> &[(AgentId, Waiting)] {
        &self.open
    }
}

impl Default for ActivityPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel for ActivityPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Activity
    }

    fn needs(&self) -> Needs {
        // Nothing of its own: it reads the rows the tree already keeps fresh.
        Needs { rows: false, reflect: false, watch: false }
    }

    fn on_key(&mut self, k: KeyEvent, _cx: &Cx<'_>, out: &mut Vec<Cmd>) -> Handled {
        match k.code {
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                Handled::Yes
            }
            KeyCode::Down => {
                if self.cursor + 1 < self.open.len() {
                    self.cursor += 1;
                }
                Handled::Yes
            }
            KeyCode::Enter => {
                if let Some((agent, w)) = self.open.get(self.cursor) {
                    out.push(Cmd::Jump { agent: agent.clone(), row: w.row });
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn on_event(&mut self, _payload: &Value, _out: &mut Vec<Cmd>) -> bool {
        false
    }

    fn on_select(&mut self, _cx: &Cx<'_>, _out: &mut Vec<Cmd>) {}

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn layout(&mut self, _area: Rect, cx: &Cx<'_>) {
        let mut now: Vec<(AgentId, Waiting)> = self
            .agents
            .iter()
            .flat_map(|a| thread::waiting_for(cx.store, a, crate::env::HUMAN).into_iter().map(move |w| (a.clone(), w)))
            .collect();
        now.sort_by(|a, b| b.1.ts.partial_cmp(&a.1.ts).unwrap_or(std::cmp::Ordering::Equal));
        // What was open and is no longer was answered.
        let still: HashSet<RowId> = now.iter().map(|(_, w)| w.row).collect();
        for (a, w) in self.open.drain(..) {
            if !still.contains(&w.row) && !self.answered.iter().any(|(_, x)| x.row == w.row) {
                self.answered.insert(0, (a, w));
            }
        }
        self.answered.truncate(ANSWERED_KEPT);
        self.open = now;
        self.cursor = self.cursor.min(self.open.len().saturating_sub(1));
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        let label = |t: String| {
            Line::from(Span::styled(t, Style::default().fg(theme::LABEL).add_modifier(Modifier::BOLD)))
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(format!("  {} Activity  ", theme::INQUIRY), Style::default().fg(theme::BLOCKED).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("{} open · {} answered", self.open.len(), self.answered.len()),
                    Style::default().fg(theme::DIM),
                ),
            ]),
            Line::from(Span::styled(
                "  questions to you, from every agent — none of them blocks anything",
                Style::default().fg(theme::DIM),
            )),
            Line::from(""),
            label(format!("  ── OPEN · {}", self.open.len())),
        ];
        if self.open.is_empty() {
            lines.push(Line::from(Span::styled("    nothing waiting on you", Style::default().fg(theme::DIM))));
        }
        for (i, (agent, w)) in self.open.iter().enumerate() {
            let cur = i == self.cursor;
            let style = if cur {
                Style::default().fg(theme::FG).add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(theme::FG)
            };
            let text = format!("{agent} · from {} · {}", w.from, w.head);
            lines.push(Line::from(vec![
                Span::styled(format!("{}{} ", if cur { "  ▸ " } else { "    " }, theme::INQUIRY), Style::default().fg(theme::BLOCKED)),
                Span::styled(crate::ui::chrome::clip_str(&text, area.width.saturating_sub(8) as usize), style),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(label(format!("  ── ANSWERED · {}", self.answered.len())));
        for (agent, w) in self.answered.iter().take(10) {
            let text = format!("✓ {agent} · from {} · {}", w.from, w.head);
            lines.push(Line::from(Span::styled(
                format!("    {}", crate::ui::chrome::clip_str(&text, area.width.saturating_sub(6) as usize)),
                Style::default().fg(theme::DIM),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ↑ ↓ pick · Enter jump to the question — the answer is an ordinary reply",
            Style::default().fg(theme::DIM),
        )));
        f.render_widget(Paragraph::new(lines), area);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(format!("Activity · {} open", self.open.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::json;

    fn store(rows: Vec<Value>) -> Store {
        let mut s = Store::default();
        let by: std::collections::BTreeMap<String, Vec<Value>> =
            rows.into_iter().fold(Default::default(), |mut m, r| {
                m.entry(r["agent"].as_str().unwrap().to_string()).or_default().push(r);
                m
            });
        for (agent, rs) in by {
            s.apply_full(&agent, rs.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect());
        }
        s
    }

    fn cx<'a>(s: &'a Store, a: &'a AgentId, seen: &'a HashSet<RowId>) -> Cx<'a> {
        Cx { agent: a, store: s, tasks: &[], seen, reflect: None }
    }

    #[test]
    fn it_gathers_every_agents_open_questions_newest_first() {
        let s = store(vec![
            json!({"id":1,"agent":"ai","ts":1.0,"content":"[from files · msg 1]\nwhich root?"}),
            json!({"id":2,"agent":"reviewer","ts":2.0,"content":"[from exec · msg 2]\nrun again?"}),
        ]);
        let mut p = ActivityPanel::new();
        p.set_agents(vec!["ai".into(), "reviewer".into()]);
        let seen = HashSet::new();
        let a: AgentId = ACTIVITY_ID.into();
        p.layout(Rect::new(0, 0, 80, 20), &cx(&s, &a, &seen));
        let heads: Vec<&str> = p.open().iter().map(|(_, w)| w.head.as_str()).collect();
        assert_eq!(heads, vec!["run again?", "which root?"]);
    }

    #[test]
    fn an_answered_question_moves_to_answered_and_enter_jumps_to_the_asker() {
        let mut s = store(vec![json!({"id":1,"agent":"ai","ts":1.0,"content":"[from files · msg 1]\nwhich root?"})]);
        let mut p = ActivityPanel::new();
        p.set_agents(vec!["ai".into()]);
        let seen = HashSet::new();
        let a: AgentId = ACTIVITY_ID.into();
        p.layout(Rect::new(0, 0, 80, 20), &cx(&s, &a, &seen));
        let mut out = Vec::new();
        p.on_key(KeyEvent::from(KeyCode::Enter), &cx(&s, &a, &seen), &mut out);
        assert!(matches!(&out[0], Cmd::Jump { agent, row: 1 } if agent == "ai"));

        // The answer lives in the asker's conversation and points back with `re`.
        s.apply_full(&"files".to_string(), vec![
            serde_json::from_value(json!({"id":2,"agent":"files","ts":2.0,"content":"[from ai · re 1]\n/work"})).unwrap(),
        ]);
        p.layout(Rect::new(0, 0, 80, 20), &cx(&s, &a, &seen));
        assert!(p.open().is_empty(), "answered is no longer open");
        assert_eq!(p.answered.len(), 1);
    }
}
