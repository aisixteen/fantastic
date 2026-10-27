//! THE PANEL FOR `cabinet`: a manager lists its cabinets (`cabinets`); a cabinet
//! cell shows its own (`view`). Which one it is comes from the reply shape: a
//! manager answers `{cabinets:[..]}`, a cell answers `{task, worker, status}`.
//! Read-only: only `cabinets` and `view` are ever sent.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;
use serde_json::{json, Value};

use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cab {
    pub id: String,
    pub task: String,
    pub worker: String,
    pub canvas: String,
    pub line: String,
    pub progress: Option<f64>,
    pub equipment: usize,
}

fn parse_cab(c: &Value) -> Cab {
    let status = c.get("status").filter(|s| s.is_object());
    Cab {
        id: ro::st(c, "id"),
        task: ro::st(c, "task"),
        worker: ro::st(c, "worker"),
        canvas: ro::st(c, "canvas"),
        line: status.map(|s| ro::st(s, "line")).unwrap_or_default(),
        progress: status
            .and_then(|s| s.get("progress"))
            .and_then(Value::as_f64),
        equipment: match c.get("equipment") {
            Some(Value::Array(a)) => a.len(),
            Some(Value::Object(o)) => o.len(),
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as usize,
            _ => 0,
        },
    }
}

pub struct CabinetPanel {
    agent: AgentId,
    scroll: u16,
    cursor: usize,
    /// None until a reply says which kind of cabinet agent this is.
    pub manager: Option<bool>,
    pub cabs: Vec<Cab>,
    pub refusal: Option<String>,
}

impl CabinetPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            scroll: 0,
            cursor: 0,
            manager: None,
            cabs: Vec::new(),
            refusal: None,
        }
    }

    fn reads(&self, role: Option<&str>, out: &mut Vec<Cmd>) {
        let verbs: &[&str] = match role {
            Some("manager") => &["cabinets"],
            Some("cabinet") => &["view"],
            _ => &["cabinets", "view"],
        };
        for v in verbs {
            out.push(Cmd::Send {
                target: self.agent.clone(),
                payload: json!({"type": v}),
            });
        }
    }

    fn known_role(&self) -> Option<&'static str> {
        self.manager.map(|m| if m { "manager" } else { "cabinet" })
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        // The probe the other role refuses is not shown once a reply has landed.
        if let (Some(e), None) = (&self.refusal, self.manager) {
            out.push(ro::bad(format!("  refused: {e}")));
        }
        if self.manager == Some(true) {
            out.push(ro::head(&format!("CABINETS · {}", self.cabs.len())));
            if self.cabs.is_empty() {
                out.push(ro::dim("    no cabinets yet".into()));
            }
        }
        for c in &self.cabs {
            out.push(ro::plain(format!("    {}  {}", c.id, c.task)));
            out.push(ro::dim(format!(
                "      worker {}  canvas {}  {} equipment",
                c.worker, c.canvas, c.equipment
            )));
            let status = if c.line.is_empty() {
                "not reported yet".to_string()
            } else {
                c.line.clone()
            };
            let pct = c
                .progress
                .map(|p| format!("  {}%", p.round() as i64))
                .unwrap_or_default();
            out.push(ro::dim(format!("      {status}{pct}")));
        }
        out
    }
}

impl Panel for CabinetPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Cabinet
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
        cx: &Cx<'_>,
        out: &mut Vec<Cmd>,
    ) -> Handled {
        if k.code == crossterm::event::KeyCode::Char('r') {
            let role = self.known_role().or_else(|| {
                cx.reflect
                    .and_then(|r| r.get("cabinet_role"))
                    .and_then(Value::as_str)
            });
            self.reads(role, out);
            return Handled::Yes;
        }
        let n = self.cabs.len();
        if ro::nav(k, &mut self.cursor, &mut self.scroll, n) {
            Handled::Yes
        } else {
            Handled::No
        }
    }

    fn on_event(&mut self, _payload: &Value, out: &mut Vec<Cmd>) -> bool {
        self.reads(self.known_role(), out);
        true
    }

    fn on_select(&mut self, cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        let role = cx
            .reflect
            .and_then(|r| r.get("cabinet_role"))
            .and_then(Value::as_str);
        self.reads(role, out);
    }

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        if reply.get("verbs").is_some() && reply.get("sentence").is_some() {
            return false;
        }
        if let Some(e) = ro::refusal(reply) {
            self.refusal = Some(e);
            return true;
        }
        if let Some(list) = reply.get("cabinets").and_then(Value::as_array) {
            self.manager = Some(true);
            self.refusal = None;
            self.cabs = list.iter().map(parse_cab).collect();
            return true;
        }
        if ["task", "worker", "status"]
            .iter()
            .any(|k| reply.get(k).is_some())
        {
            self.manager = Some(false);
            self.refusal = None;
            self.cabs = vec![parse_cab(reply)];
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
        Some(format!("{} · {} cabinets", self.agent, self.cabs.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::ro::testing::*;
    use super::*;

    #[test]
    fn manager_lists_cabinets_with_task_worker_status_and_equipment_count() {
        let mut p = CabinetPanel::new("mgr".into());
        p.on_reply(
            &json!({"count":1,"cabinets":[{"id":"c1","task":"build it","worker":"w1","canvas":"cv1",
                "status":{"line":"halfway","progress":50},"equipment":[{"id":"a"},{"id":"b"}]}]}),
            &mut Vec::new(),
        );
        let t = text(&p.lines());
        for want in [
            "c1  build it",
            "worker w1",
            "canvas cv1",
            "2 equipment",
            "halfway  50%",
            "CABINETS · 1",
        ] {
            assert!(t.contains(want), "lacks {want:?}:\n{t}");
        }
    }

    #[test]
    fn a_cabinet_cell_shows_its_own_view_fields() {
        let mut p = CabinetPanel::new("c1".into());
        p.on_reply(
            &json!({"id":"c1","task":"t","worker":"w","canvas":"cv","status":null,"equipment":[]}),
            &mut Vec::new(),
        );
        assert_eq!(p.manager, Some(false));
        let t = text(&p.lines());
        assert!(
            t.contains("c1  t") && t.contains("not reported yet") && !t.contains("CABINETS"),
            "{t}"
        );
    }

    #[test]
    fn empty_manager_says_no_cabinets_yet() {
        let mut p = CabinetPanel::new("mgr".into());
        p.on_reply(&json!({"cabinets":[],"count":0}), &mut Vec::new());
        assert!(text(&p.lines()).contains("no cabinets yet"));
    }

    #[test]
    fn selecting_reads_by_role_and_probes_both_when_unknown() {
        let mut p = CabinetPanel::new("mgr".into());
        let mut out = Vec::new();
        with_cx("mgr", None, |cx| p.on_select(cx, &mut out));
        assert_eq!(sent(&out), vec!["cabinets", "view"]);
        let r = json!({"cabinet_role":"manager"});
        let mut out = Vec::new();
        with_cx("mgr", Some(&r), |cx| p.on_select(cx, &mut out));
        assert_eq!(sent(&out), vec!["cabinets"]);
    }

    #[test]
    fn a_refusal_is_shown_until_a_reply_says_what_this_is() {
        let mut p = CabinetPanel::new("x".into());
        p.on_reply(&json!({"error":"unknown type"}), &mut Vec::new());
        assert!(text(&p.lines()).contains("refused: unknown type"));
        p.on_reply(&json!({"cabinets":[]}), &mut Vec::new());
        assert!(!text(&p.lines()).contains("refused"));
    }
}
