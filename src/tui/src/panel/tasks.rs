//! THE PANEL FOR `task_registry`: the task tree, each task's holder, status and
//! source, `versions` when an entry carries them (#437), and the reassign history
//! on expand. Read-only: only `list` is ever sent.

use std::collections::{BTreeSet, HashSet};

use crossterm::event::KeyCode as K;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;
use serde_json::{json, Value};

use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    pub id: String,
    pub parent: Option<String>,
    pub source: String,
    pub update_to: String,
    pub status: String,
    pub holder: String,
    /// (from, to, by)
    pub history: Vec<(String, String, String)>,
    pub report: Option<String>,
    pub versions: Option<String>,
}

/// A holder as one word: an agent id, `subtree:<root>` or `unheld`.
pub fn holder_text(v: &Value) -> String {
    match v {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Object(o) => match o.get("subtree").and_then(Value::as_str) {
            Some(r) => format!("subtree:{r}"),
            None => "unheld".into(),
        },
        _ => "unheld".into(),
    }
}

fn versions_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(format!("v{s}")),
        Value::Number(n) => Some(format!("v{n}")),
        Value::Array(a) if a.is_empty() => None,
        Value::Array(a) => Some(format!(
            "versions [{}]",
            a.iter()
                .map(|x| ro::brief(x, 24))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        other => Some(format!("versions {}", ro::brief(other, 40))),
    }
}

fn parse_entry(t: &Value) -> Entry {
    let history = t
        .get("history")
        .and_then(Value::as_array)
        .map(|h| {
            h.iter()
                .map(|e| {
                    (
                        e.get("from")
                            .map(holder_text)
                            .unwrap_or_else(|| "unheld".into()),
                        e.get("to")
                            .map(holder_text)
                            .unwrap_or_else(|| "unheld".into()),
                        ro::st(e, "by"),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let report = t.get("report").filter(|r| r.is_object()).map(|r| {
        let mut s = format!("{} {}", ro::st(r, "from"), ro::st(r, "state"));
        if let Some(res) = r.get("result").filter(|x| !x.is_null()) {
            s.push_str(&format!(" {}", ro::brief(res, 60)));
        }
        s.trim().to_string()
    });
    Entry {
        id: ro::st(t, "id"),
        parent: Some(ro::st(t, "parent")).filter(|p| !p.is_empty()),
        source: ro::st(t, "source"),
        update_to: ro::st(t, "update_to"),
        status: ro::st(t, "status"),
        holder: t
            .get("holder")
            .map(holder_text)
            .unwrap_or_else(|| "unheld".into()),
        history,
        report,
        versions: t.get("versions").and_then(versions_text),
    }
}

pub struct TasksPanel {
    agent: AgentId,
    scroll: u16,
    pub cursor: usize,
    pub tasks: Vec<Entry>,
    pub expanded: BTreeSet<String>,
    pub refusal: Option<String>,
}

impl TasksPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            scroll: 0,
            cursor: 0,
            tasks: Vec::new(),
            expanded: BTreeSet::new(),
            refusal: None,
        }
    }

    /// (depth, index) in display order: roots first, children under their
    /// parent, ids sorted. An orphan or a member of a cycle is a root.
    pub fn order(&self) -> Vec<(usize, usize)> {
        let mut idx: Vec<usize> = (0..self.tasks.len()).collect();
        idx.sort_by(|a, b| self.tasks[*a].id.cmp(&self.tasks[*b].id));
        let ids: HashSet<&str> = self.tasks.iter().map(|t| t.id.as_str()).collect();
        let mut seen = vec![false; self.tasks.len()];
        let mut out = Vec::new();
        fn walk(
            p: &TasksPanel,
            idx: &[usize],
            i: usize,
            d: usize,
            seen: &mut [bool],
            out: &mut Vec<(usize, usize)>,
        ) {
            if seen[i] {
                return;
            }
            seen[i] = true;
            out.push((d, i));
            for &c in idx {
                if p.tasks[c].parent.as_deref() == Some(p.tasks[i].id.as_str()) && c != i {
                    walk(p, idx, c, d + 1, seen, out);
                }
            }
        }
        for &i in &idx {
            let orphan = match &self.tasks[i].parent {
                None => true,
                Some(p) => !ids.contains(p.as_str()),
            };
            if orphan {
                walk(self, &idx, i, 0, &mut seen, &mut out);
            }
        }
        // what is left is a cycle: break it at the lowest id
        for &i in &idx {
            walk(self, &idx, i, 0, &mut seen, &mut out);
        }
        out
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Some(e) = &self.refusal {
            out.push(ro::bad(format!("  refused: {e}")));
        }
        out.push(ro::head(&format!("TASKS · {}", self.tasks.len())));
        if self.tasks.is_empty() {
            out.push(ro::dim("    no tasks".into()));
        }
        for (row, (d, i)) in self.order().into_iter().enumerate() {
            let t = &self.tasks[i];
            let open = self.expanded.contains(&t.id);
            let mut s = format!(
                "  {}{}{}  {}  {}  {}",
                if row == self.cursor { "▸ " } else { "  " },
                "  ".repeat(d),
                t.id,
                t.status,
                t.holder,
                t.source
            );
            if let Some(v) = &t.versions {
                s.push_str(&format!("  {v}"));
            }
            out.push(ro::sel(s, row == self.cursor));
            if open {
                let pad = format!("      {}", "  ".repeat(d));
                if t.update_to != t.source && !t.update_to.is_empty() {
                    out.push(ro::dim(format!("{pad}update_to {}", t.update_to)));
                }
                if t.history.is_empty() {
                    out.push(ro::dim(format!("{pad}no reassign history")));
                }
                for (from, to, by) in &t.history {
                    out.push(ro::dim(format!("{pad}{from} -> {to} by {by}")));
                }
                if let Some(r) = &t.report {
                    out.push(ro::dim(format!("{pad}report {r}")));
                }
            }
        }
        out
    }
}

impl Panel for TasksPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Tasks
    }

    fn needs(&self) -> Needs {
        Needs {
            rows: false,
            reflect: true,
            watch: true,
        }
    }

    fn on_key(
        &mut self,
        k: crossterm::event::KeyEvent,
        _cx: &Cx<'_>,
        out: &mut Vec<Cmd>,
    ) -> Handled {
        match k.code {
            K::Char('r') => {
                out.push(Cmd::Send {
                    target: self.agent.clone(),
                    payload: json!({"type": "list"}),
                });
                Handled::Yes
            }
            K::Enter | K::Char('e') => {
                if let Some((_, i)) = self.order().get(self.cursor).copied() {
                    let id = self.tasks[i].id.clone();
                    if !self.expanded.remove(&id) {
                        self.expanded.insert(id);
                    }
                }
                Handled::Yes
            }
            _ => {
                let n = self.tasks.len();
                if ro::nav(k, &mut self.cursor, &mut self.scroll, n) {
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
        }
    }

    fn on_event(&mut self, _payload: &Value, out: &mut Vec<Cmd>) -> bool {
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type": "list"}),
        });
        true
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type": "list"}),
        });
    }

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        if reply.get("verbs").is_some() && reply.get("sentence").is_some() {
            return false;
        }
        if let Some(e) = ro::refusal(reply) {
            self.refusal = Some(e);
            return true;
        }
        if let Some(list) = reply.get("tasks").and_then(Value::as_array) {
            self.refusal = None;
            self.tasks = list.iter().map(parse_entry).collect();
            self.cursor = self.cursor.min(self.tasks.len().saturating_sub(1));
            return true;
        }
        false
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        ro::draw(f, area, self.lines(), self.scroll);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(format!("{} · {} tasks", self.agent, self.tasks.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::ro::testing::*;
    use super::*;

    fn loaded(v: Value) -> TasksPanel {
        let mut p = TasksPanel::new("tasks".into());
        p.on_reply(&json!({ "tasks": v }), &mut Vec::new());
        p
    }

    #[test]
    fn tasks_render_as_a_parent_child_tree() {
        let p = loaded(json!([
            {"id":"b","parent":"a","status":"waiting","source":"gh:o/r#2","holder":null},
            {"id":"a","parent":null,"status":"running","source":"gh:o/r#1","holder":"w1"},
            {"id":"c","parent":"a","status":"done","source":"gh:o/r#3","holder":null},
        ]));
        let t = text(&p.lines());
        let (a, b, c) = (
            t.find("a  running").unwrap(),
            t.find("  b  waiting").unwrap(),
            t.find("  c  done").unwrap(),
        );
        assert!(a < b && b < c, "{t}");
        assert!(
            t.contains("      b  waiting"),
            "the child is indented under its parent:\n{t}"
        );
    }

    #[test]
    fn an_orphan_or_cyclic_parent_falls_to_the_root_level_and_does_not_loop() {
        let p = loaded(json!([
            {"id":"o","parent":"ghost","status":"waiting","source":"s","holder":null},
            {"id":"x","parent":"y","status":"waiting","source":"s","holder":null},
            {"id":"y","parent":"x","status":"waiting","source":"s","holder":null},
            {"id":"s","parent":"s","status":"waiting","source":"s","holder":null},
        ]));
        let ord = p.order();
        assert_eq!(ord.len(), 4, "every task shown exactly once");
        assert_eq!(
            ord.iter().find(|(_, i)| p.tasks[*i].id == "o").unwrap().0,
            0
        );
        assert_eq!(
            ord.iter().find(|(_, i)| p.tasks[*i].id == "s").unwrap().0,
            0
        );
        assert_eq!(ord.iter().filter(|(d, _)| *d == 0).count(), 3);
    }

    #[test]
    fn holder_forms_render_agent_subtree_and_unheld() {
        let p = loaded(json!([
            {"id":"a","status":"running","source":"s","holder":"w1"},
            {"id":"b","status":"running","source":"s","holder":{"subtree":"lead"}},
            {"id":"c","status":"waiting","source":"s","holder":null},
        ]));
        let t = text(&p.lines());
        assert!(t.contains("running  w1"), "{t}");
        assert!(t.contains("subtree:lead"), "{t}");
        assert!(t.contains("waiting  unheld"), "{t}");
    }

    #[test]
    fn source_and_status_are_shown() {
        let p = loaded(
            json!([{"id":"a","status":"blocked","source":"gh:aisixteen/fantastic#438","holder":null}]),
        );
        let t = text(&p.lines());
        assert!(
            t.contains("blocked") && t.contains("gh:aisixteen/fantastic#438"),
            "{t}"
        );
    }

    #[test]
    fn versions_show_when_present_and_are_absent_without_them() {
        let p = loaded(json!([
            {"id":"a","status":"s","source":"x","holder":null,"versions":"1.2"},
            {"id":"b","status":"s","source":"x","holder":null,"versions":["1","2"]},
            {"id":"c","status":"s","source":"x","holder":null},
        ]));
        let t = text(&p.lines());
        assert!(t.contains("v1.2") && t.contains("versions [1, 2]"), "{t}");
        let c = t.lines().find(|l| l.contains("  c  ")).unwrap();
        assert!(!c.contains(" v1") && !c.contains("versions"), "{c}");
    }

    #[test]
    fn reassign_history_shows_only_when_expanded() {
        let mut p = loaded(
            json!([{"id":"a","status":"running","source":"s","update_to":"gh:o/r#9","holder":"w2",
            "history":[{"from":null,"to":"w1","by":"lead","at":1},{"from":"w1","to":"w2","by":"lead","at":2}],
            "report":{"from":"w2","state":"running","result":"half","at":3}}]),
        );
        let t = text(&p.lines());
        assert!(!t.contains(" by "), "{t}");
        with_cx("tasks", None, |cx| {
            p.on_key(key(K::Enter), cx, &mut Vec::new())
        });
        let t = text(&p.lines());
        assert!(
            t.contains("unheld -> w1 by lead") && t.contains("w1 -> w2 by lead"),
            "{t}"
        );
        assert!(
            t.contains("update_to gh:o/r#9") && t.contains("report w2 running half"),
            "{t}"
        );
        with_cx("tasks", None, |cx| {
            p.on_key(key(K::Char('e')), cx, &mut Vec::new())
        });
        assert!(!text(&p.lines()).contains(" by "));
    }

    #[test]
    fn no_write_verb_is_ever_sent() {
        let mut p = loaded(json!([{"id":"a","status":"s","source":"x","holder":null}]));
        let mut out = Vec::new();
        with_cx("tasks", None, |cx| {
            p.on_select(cx, &mut out);
            for c in [
                K::Char('j'),
                K::Char('k'),
                K::Enter,
                K::Char('e'),
                K::Char('r'),
                K::Char('g'),
                K::PageDown,
                K::Char('x'),
            ] {
                p.on_key(key(c), cx, &mut out);
            }
        });
        p.on_event(&json!({}), &mut out);
        let verbs = sent(&out);
        assert!(
            !verbs.is_empty() && verbs.iter().all(|v| v == "list" || v == "get"),
            "{verbs:?}"
        );
    }

    #[test]
    fn a_refusal_reply_is_shown_not_swallowed() {
        let mut p = TasksPanel::new("tasks".into());
        assert!(p.on_reply(&json!({"error":"disabled"}), &mut Vec::new()));
        assert!(text(&p.lines()).contains("refused: disabled"));
    }
}
