//! THE PANEL FOR `agentic_resources`: roles, the fleet it spawned, and the
//! counts against their limits. Read-only: only `roles`, `fleet`, `role` and
//! `get_count` are ever sent.
//!
//! What the replies do NOT carry: `fleet` answers `{roles:{role:[{id}]}}`, so a
//! parent or a paused flag shows only when an entry carries one (`parent` or
//! `spawned_by`; `paused` or `status`). A reply to a call is routed to the panel
//! of the agent it was sent TO, so this panel cannot read a child's own `status`.
//! No answer counts as active, as the bundle's readme says.

use std::collections::{BTreeMap, HashSet, VecDeque};

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;
use serde_json::{json, Value};

use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Role {
    pub name: String,
    pub purpose: String,
    pub tier: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Member {
    pub id: String,
    pub parent: Option<String>,
    pub paused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Count {
    pub running: u64,
    pub max: Option<u64>,
}

#[derive(Debug, Default)]
pub struct State {
    pub roles: Vec<Role>,
    pub fleet: BTreeMap<String, Vec<Member>>,
    pub total: Option<Count>,
    pub per_role: BTreeMap<String, Count>,
    pub refusal: Option<String>,
}

pub struct ResourcesPanel {
    agent: AgentId,
    scroll: u16,
    cursor: usize,
    pub state: State,
    asked: HashSet<String>,
    pending: VecDeque<Option<String>>,
}

fn count_text(c: &Count) -> String {
    match c.max {
        Some(m) => format!("{}/{}", c.running, m),
        None => format!("{} · no limit", c.running),
    }
}

impl ResourcesPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            scroll: 0,
            cursor: 0,
            state: State::default(),
            asked: HashSet::new(),
            pending: VecDeque::new(),
        }
    }

    fn send(&self, out: &mut Vec<Cmd>, payload: Value) {
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload,
        });
    }

    fn reads(&mut self, out: &mut Vec<Cmd>) {
        self.asked.clear();
        self.pending.clear();
        self.send(out, json!({"type": "roles"}));
        self.send(out, json!({"type": "fleet"}));
        self.send(out, json!({"type": "get_count"}));
        self.pending.push_back(None);
    }

    /// Ask a role's card and its count once per refresh.
    fn ask_role(&mut self, name: &str, out: &mut Vec<Cmd>) {
        if name.is_empty() || !self.asked.insert(name.to_string()) {
            return;
        }
        self.send(out, json!({"type": "role", "name": name}));
        self.send(out, json!({"type": "get_count", "role": name}));
        self.pending.push_back(Some(name.to_string()));
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let s = &self.state;
        let mut out = Vec::new();
        if let Some(e) = &s.refusal {
            out.push(ro::bad(format!("  refused: {e}")));
            out.push(Line::from(""));
        }
        out.push(ro::head("TOTAL"));
        out.push(match &s.total {
            Some(c) => ro::plain(format!("    {}", count_text(c))),
            None => ro::dim("    reading…".into()),
        });
        out.push(Line::from(""));
        out.push(ro::head("ROLES"));
        if s.roles.is_empty() {
            out.push(ro::dim("    no roles".into()));
        }
        for r in &s.roles {
            let tier = r.tier.as_deref().unwrap_or("?");
            let c = s
                .per_role
                .get(&r.name)
                .map(count_text)
                .unwrap_or_else(|| "?".into());
            out.push(ro::plain(format!("    {}  tier {tier}  {c}", r.name)));
            if !r.purpose.is_empty() {
                out.push(ro::dim(format!("      {}", r.purpose)));
            }
        }
        out.push(Line::from(""));
        out.push(ro::head("FLEET"));
        let n: usize = s.fleet.values().map(Vec::len).sum();
        if n == 0 {
            out.push(ro::dim("    nothing spawned".into()));
        }
        for (role, members) in &s.fleet {
            for m in members {
                let parent = m.parent.as_deref().unwrap_or("-");
                let state = if m.paused { "paused" } else { "active" };
                out.push(ro::plain(format!(
                    "    {}  {role}  parent {parent}  {state}",
                    m.id
                )));
            }
        }
        out
    }
}

impl Panel for ResourcesPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Resources
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
        if k.code == crossterm::event::KeyCode::Char('r') {
            self.reads(out);
            return Handled::Yes;
        }
        if ro::nav(k, &mut self.cursor, &mut self.scroll, 0) {
            Handled::Yes
        } else {
            Handled::No
        }
    }

    fn on_event(&mut self, _payload: &Value, out: &mut Vec<Cmd>) -> bool {
        self.reads(out);
        true
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        self.reads(out);
    }

    fn on_reply(&mut self, reply: &Value, out: &mut Vec<Cmd>) -> bool {
        if reply.get("verbs").is_some() && reply.get("sentence").is_some() {
            return false;
        }
        if let Some(e) = ro::refusal(reply) {
            self.state.refusal = Some(e);
            return true;
        }
        if let Some(roles) = reply.get("roles").and_then(Value::as_array) {
            self.state.refusal = None;
            self.state.roles = roles
                .iter()
                .map(|r| Role {
                    name: ro::st(r, "name"),
                    purpose: ro::st(r, "purpose"),
                    tier: self
                        .state
                        .roles
                        .iter()
                        .find(|o| o.name == ro::st(r, "name"))
                        .and_then(|o| o.tier.clone()),
                })
                .collect();
            let names: Vec<String> = self.state.roles.iter().map(|r| r.name.clone()).collect();
            for n in names {
                self.ask_role(&n, out);
            }
            return true;
        }
        if let Some(fleet) = reply.get("roles").and_then(Value::as_object) {
            self.state.refusal = None;
            self.state.fleet = fleet
                .iter()
                .map(|(role, list)| {
                    let members = list
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .map(|m| Member {
                                    id: ro::st(m, "id"),
                                    parent: ["parent", "spawned_by"]
                                        .iter()
                                        .map(|k| ro::st(m, k))
                                        .find(|p| !p.is_empty()),
                                    paused: m.get("paused").and_then(Value::as_bool) == Some(true)
                                        || ro::st(m, "status") == "paused",
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (role.clone(), members)
                })
                .collect();
            let names: Vec<String> = self.state.fleet.keys().cloned().collect();
            for n in names {
                self.ask_role(&n, out);
            }
            return true;
        }
        if let (Some(name), Some(tier)) =
            (reply.get("name").and_then(Value::as_str), reply.get("tier"))
        {
            let tier = tier
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| tier.to_string());
            match self.state.roles.iter_mut().find(|r| r.name == name) {
                Some(r) => r.tier = Some(tier),
                None => self.state.roles.push(Role {
                    name: name.to_string(),
                    purpose: ro::st(reply, "purpose"),
                    tier: Some(tier),
                }),
            }
            return true;
        }
        if let Some(running) = reply.get("running").and_then(Value::as_u64) {
            let c = Count {
                running,
                max: reply.get("max").and_then(Value::as_u64),
            };
            match self.pending.pop_front().flatten() {
                Some(role) => {
                    self.state.per_role.insert(role, c);
                }
                None => self.state.total = Some(c),
            }
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
        Some(format!("{} · resources", self.agent))
    }
}

#[cfg(test)]
mod tests {
    use super::ro::testing::*;
    use super::*;

    fn panel() -> ResourcesPanel {
        ResourcesPanel::new("res".into())
    }

    #[test]
    fn roles_show_name_purpose_and_tier() {
        let mut p = panel();
        let mut out = Vec::new();
        p.on_reply(
            &json!({"roles":[{"name":"worker","purpose":"does the thing"}]}),
            &mut out,
        );
        p.on_reply(
            &json!({"name":"worker","tier":"small","purpose":"does the thing"}),
            &mut out,
        );
        let t = text(&p.lines());
        assert!(
            t.contains("worker") && t.contains("tier small") && t.contains("does the thing"),
            "{t}"
        );
    }

    #[test]
    fn fleet_lists_each_agent_with_role_parent_and_active_or_paused() {
        let mut p = panel();
        let mut out = Vec::new();
        p.on_reply(
            &json!({"roles":{"worker":[{"id":"w1","parent":"boss","paused":true},{"id":"w2"}]}}),
            &mut out,
        );
        let t = text(&p.lines());
        assert!(t.contains("w1  worker  parent boss  paused"), "{t}");
        assert!(
            t.contains("w2  worker  parent -  active"),
            "no answer is active:\n{t}"
        );
    }

    #[test]
    fn counts_show_role_and_total_limits_and_uncapped_is_no_limit() {
        let mut p = panel();
        let mut out = Vec::new();
        with_cx("res", None, |cx| p.on_select(cx, &mut out));
        p.on_reply(&json!({"roles":[{"name":"worker","purpose":""}]}), &mut out);
        // pending order: total first, then the role
        p.on_reply(&json!({"running":2,"max":null}), &mut out);
        p.on_reply(&json!({"running":2,"max":5}), &mut out);
        let t = text(&p.lines());
        assert!(t.contains("2 · no limit"), "{t}");
        assert!(t.contains("worker  tier ?  2/5"), "{t}");
    }

    #[test]
    fn on_select_asks_roles_fleet_and_count() {
        let mut p = panel();
        let mut out = Vec::new();
        with_cx("res", None, |cx| p.on_select(cx, &mut out));
        assert!(matches!(&out[0], Cmd::Reflect { .. }));
        assert_eq!(sent(&out), vec!["roles", "fleet", "get_count"]);
    }

    #[test]
    fn a_refusal_reply_is_shown_not_swallowed() {
        let mut p = panel();
        let mut out = Vec::new();
        assert!(p.on_reply(&json!({"error":"not_allowed"}), &mut out));
        assert!(text(&p.lines()).contains("refused: not_allowed"));
    }
}
