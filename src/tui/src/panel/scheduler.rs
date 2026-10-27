//! THE PANEL FOR `scheduler`: one row per job (target, verb, interval, next run,
//! paused, run count) and the last result of each from the newest history event.
//! Read-only: only `list` and `history` are ever sent.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;
use serde_json::{json, Value};

use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Job {
    pub id: String,
    pub target: String,
    pub verb: String,
    pub every: u64,
    pub next_run: Option<f64>,
    pub paused: bool,
    pub run_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fire {
    pub schedule_id: String,
    pub ts: f64,
    pub result: String,
    pub error: Option<String>,
}

/// "in 42s", or "due" when the time has come.
pub fn next_text(next: Option<f64>, now: f64) -> String {
    match next {
        None => "-".into(),
        Some(n) if n > now => format!("in {}s", (n - now).ceil() as u64),
        Some(_) => "due".into(),
    }
}

pub struct SchedulerPanel {
    agent: AgentId,
    scroll: u16,
    cursor: usize,
    pub jobs: Vec<Job>,
    pub history: Vec<Fire>,
    pub refusal: Option<String>,
}

impl SchedulerPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            scroll: 0,
            cursor: 0,
            jobs: Vec::new(),
            history: Vec::new(),
            refusal: None,
        }
    }

    fn reads(&self, out: &mut Vec<Cmd>) {
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type": "list"}),
        });
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type": "history", "limit": 20}),
        });
    }

    /// The newest fire of a schedule: the latest `ts`, the later entry on a tie.
    fn last(&self, id: &str) -> Option<&Fire> {
        self.history
            .iter()
            .filter(|f| f.schedule_id == id)
            .fold(None, |best: Option<&Fire>, f| match best {
                Some(b) if b.ts > f.ts => Some(b),
                _ => Some(f),
            })
    }

    pub fn lines(&self, now: f64) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Some(e) = &self.refusal {
            out.push(ro::bad(format!("  refused: {e}")));
        }
        out.push(ro::head(&format!("JOBS · {}", self.jobs.len())));
        if self.jobs.is_empty() {
            out.push(ro::dim("    nothing scheduled".into()));
        }
        for j in &self.jobs {
            out.push(ro::plain(format!(
                "    {}  {} · {}  every {}s  next {}  runs {}{}",
                j.id,
                j.target,
                j.verb,
                j.every,
                next_text(j.next_run, now),
                j.run_count,
                if j.paused { "  PAUSED" } else { "" }
            )));
            out.push(match self.last(&j.id) {
                None => ro::dim("      last: no runs yet".into()),
                Some(f) => match &f.error {
                    Some(e) => ro::bad(format!("      last: ERROR {e}")),
                    None => ro::dim(format!("      last: ok {}", f.result)),
                },
            });
        }
        out
    }
}

impl Panel for SchedulerPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Scheduler
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
        let n = self.jobs.len();
        if ro::nav(k, &mut self.cursor, &mut self.scroll, n) {
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

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        if reply.get("verbs").is_some() && reply.get("sentence").is_some() {
            return false;
        }
        if let Some(e) = ro::refusal(reply) {
            self.refusal = Some(e);
            return true;
        }
        if let Some(list) = reply.get("schedules").and_then(Value::as_array) {
            self.refusal = None;
            self.jobs = list
                .iter()
                .map(|s| Job {
                    id: ro::st(s, "id"),
                    target: ro::st(s, "target"),
                    verb: s
                        .get("payload")
                        .map(|p| ro::st(p, "type"))
                        .unwrap_or_default(),
                    every: s
                        .get("interval_seconds")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    next_run: s.get("next_run").and_then(Value::as_f64),
                    paused: s.get("paused").and_then(Value::as_bool) == Some(true),
                    run_count: s.get("run_count").and_then(Value::as_u64).unwrap_or(0),
                })
                .collect();
            return true;
        }
        if let Some(h) = reply.get("history").and_then(Value::as_array) {
            self.refusal = None;
            self.history = h
                .iter()
                .map(|e| Fire {
                    schedule_id: ro::st(e, "schedule_id"),
                    ts: e.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
                    result: e
                        .get("result")
                        .filter(|r| !r.is_null())
                        .map(|r| ro::brief(r, 60))
                        .unwrap_or_default(),
                    error: e
                        .get("error")
                        .filter(|r| !r.is_null())
                        .map(|r| ro::brief(r, 80)),
                })
                .collect();
            return true;
        }
        false
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        ro::draw(f, area, self.lines(ro::now_secs()), self.scroll);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(format!("{} · {} jobs", self.agent, self.jobs.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::ro::testing::*;
    use super::*;

    fn loaded() -> SchedulerPanel {
        let mut p = SchedulerPanel::new("sched".into());
        let mut out = Vec::new();
        p.on_reply(
            &json!({"schedules":[
                {"id":"s1","target":"ai","payload":{"type":"send","text":"hi"},"interval_seconds":60,"next_run":1100.0,"run_count":3,"paused":false},
                {"id":"s2","target":"files","payload":{"type":"list"},"interval_seconds":30,"next_run":900.0,"run_count":0,"paused":true},
            ]}),
            &mut out,
        );
        p.on_reply(
            &json!({"history":[
                {"schedule_id":"s1","result":{"ok":true},"error":null,"ts":10.0,"duration_ms":5},
                {"schedule_id":"s1","result":null,"error":"timeout","ts":20.0,"duration_ms":9},
            ]}),
            &mut out,
        );
        p
    }

    #[test]
    fn jobs_show_target_verb_interval_and_next_run() {
        let t = text(&loaded().lines(1000.0));
        assert!(
            t.contains("s1  ai · send  every 60s  next in 100s  runs 3"),
            "{t}"
        );
        assert!(t.contains("s2  files · list  every 30s"), "{t}");
    }

    #[test]
    fn next_run_is_relative_to_now_and_a_past_one_says_due() {
        assert_eq!(next_text(Some(1100.0), 1000.0), "in 100s");
        assert_eq!(next_text(Some(1000.4), 1000.0), "in 1s");
        assert_eq!(next_text(Some(900.0), 1000.0), "due");
        assert_eq!(next_text(None, 1000.0), "-");
    }

    #[test]
    fn last_result_comes_from_the_newest_history_event_and_an_error_is_marked() {
        let t = text(&loaded().lines(1000.0));
        assert!(
            t.contains("last: ERROR timeout"),
            "newest is the error:\n{t}"
        );
        assert!(t.contains("last: no runs yet"), "{t}");
        let mut p = loaded();
        p.on_reply(
            &json!({"history":[{"schedule_id":"s1","result":{"ok":true},"error":null,"ts":10.0}]}),
            &mut Vec::new(),
        );
        assert!(text(&p.lines(1000.0)).contains("last: ok {\"ok\":true}"));
    }

    #[test]
    fn a_paused_job_is_marked() {
        let t = text(&loaded().lines(1000.0));
        let s1 = t.lines().find(|l| l.contains("s1  ai")).unwrap();
        let s2 = t.lines().find(|l| l.contains("s2  files")).unwrap();
        assert!(s2.contains("PAUSED") && !s1.contains("PAUSED"), "{t}");
    }

    #[test]
    fn on_select_asks_list_and_history_only() {
        let mut p = SchedulerPanel::new("sched".into());
        let mut out = Vec::new();
        with_cx("sched", None, |cx| p.on_select(cx, &mut out));
        assert_eq!(sent(&out), vec!["list", "history"]);
        assert!(matches!(&out[2], Cmd::Send { payload, .. } if payload["limit"] == 20));
    }
}
