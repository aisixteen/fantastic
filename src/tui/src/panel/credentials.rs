//! THE PANEL FOR `credential_store`: key NAMES, the host each is bound to, the
//! field NAMES, and who may use the store (the reflect's `callable_by`).
//!
//! NEVER A VALUE. Only `list` is sent (never `get`), and the parser keeps three
//! whitelisted things per ref (name, host, field names). Any other key of a reply
//! (a value, a length, a mask) is dropped at parse time, so it cannot reach a
//! line even if a kernel were to send one.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;
use serde_json::{json, Value};

use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CredRef {
    pub name: String,
    pub host: Option<String>,
    pub fields: Vec<String>,
}

/// Names only: a list of strings keeps its strings; an object keeps its KEYS.
fn field_names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        Some(Value::Object(o)) => o.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

pub fn parse_refs(list: &[Value]) -> Vec<CredRef> {
    list.iter()
        .map(|r| CredRef {
            name: ro::st(r, "ref"),
            host: Some(ro::st(r, "host")).filter(|h| !h.is_empty()),
            fields: field_names(r.get("fields")),
        })
        .collect()
}

/// caller -> verbs, from the store's own reflect.
pub fn grants(reflect: Option<&Value>) -> Vec<(String, String)> {
    reflect
        .and_then(|r| r.get("callable_by"))
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !k.starts_with('$'))
                .map(|(k, v)| {
                    let verbs = match v {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", "),
                        _ => String::new(),
                    };
                    (k.clone(), verbs)
                })
                .collect()
        })
        .unwrap_or_default()
}

pub struct CredentialsPanel {
    agent: AgentId,
    scroll: u16,
    cursor: usize,
    pub refs: Vec<CredRef>,
    pub loaded: bool,
    pub refusal: Option<String>,
}

impl CredentialsPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            scroll: 0,
            cursor: 0,
            refs: Vec::new(),
            loaded: false,
            refusal: None,
        }
    }

    fn list(&self, out: &mut Vec<Cmd>) {
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type": "list"}),
        });
    }

    pub fn lines(&self, reflect: Option<&Value>) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Some(e) = &self.refusal {
            out.push(ro::bad(format!("  refused: {e}")));
        }
        out.push(ro::head("REFS"));
        if self.loaded && self.refs.is_empty() {
            out.push(ro::dim("    no refs".into()));
        }
        for r in &self.refs {
            out.push(ro::plain(format!(
                "    {}  host {}",
                r.name,
                r.host.as_deref().unwrap_or("-")
            )));
            out.push(ro::dim(format!("      fields: {}", r.fields.join(", "))));
        }
        out.push(Line::from(""));
        out.push(ro::head("GRANTS · who may use the store"));
        let g = grants(reflect);
        if g.is_empty() {
            out.push(ro::dim("    nobody named".into()));
        }
        for (who, verbs) in g {
            out.push(ro::plain(format!("    {who}: {verbs}")));
        }
        out
    }
}

impl Panel for CredentialsPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Credentials
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
            self.list(out);
            return Handled::Yes;
        }
        let n = self.refs.len();
        if ro::nav(k, &mut self.cursor, &mut self.scroll, n) {
            Handled::Yes
        } else {
            Handled::No
        }
    }

    fn on_event(&mut self, _payload: &Value, out: &mut Vec<Cmd>) -> bool {
        self.list(out);
        true
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        self.list(out);
    }

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        if reply.get("verbs").is_some() && reply.get("sentence").is_some() {
            return false;
        }
        if let Some(e) = ro::refusal(reply) {
            self.refusal = Some(e);
            return true;
        }
        if let Some(list) = reply.get("refs").and_then(Value::as_array) {
            self.refusal = None;
            self.refs = parse_refs(list);
            self.loaded = true;
            return true;
        }
        false
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, cx: &Cx<'_>) {
        ro::draw(f, area, self.lines(cx.reflect), self.scroll);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(format!("{} · {} refs", self.agent, self.refs.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::ro::testing::*;
    use super::*;

    fn reflect() -> Value {
        json!({"callable_by":{"$self":"*","openai-key":["set","get","forget"],"exec":"*"}})
    }

    #[test]
    fn refs_show_name_host_and_field_names() {
        let mut p = CredentialsPanel::new("cred".into());
        p.on_reply(
            &json!({"refs":[{"ref":"openai","host":"api.openai.com","fields":["api_key","org"]}]}),
            &mut Vec::new(),
        );
        let t = text(&p.lines(None));
        assert!(
            t.contains("openai  host api.openai.com") && t.contains("fields: api_key, org"),
            "{t}"
        );
    }

    #[test]
    fn grants_come_from_callable_by() {
        let p = CredentialsPanel::new("cred".into());
        let r = reflect();
        let t = text(&p.lines(Some(&r)));
        assert!(
            t.contains("openai-key: set, get, forget") && t.contains("exec: *"),
            "{t}"
        );
        assert!(!t.contains("$self"), "{t}");
    }

    #[test]
    fn no_value_ever_reaches_a_line() {
        let mut p = CredentialsPanel::new("cred".into());
        p.on_reply(
            &json!({"refs":[{"ref":"k","host":"h.example","fields":{"api_key":"sk-SECRET-123"},
                "value":"sk-SECRET-123","len":13,"masked":"****"}],"masked":"****","len":13}),
            &mut Vec::new(),
        );
        let t = text(&p.lines(None));
        for bad in ["SECRET", "sk-", "13", "****"] {
            assert!(!t.contains(bad), "{bad:?} in:\n{t}");
        }
        assert!(
            t.contains("fields: api_key"),
            "the field NAME is shown:\n{t}"
        );
        assert!(!format!("{:?}", p.refs).contains("SECRET"));
    }

    #[test]
    fn only_list_is_ever_sent() {
        let mut p = CredentialsPanel::new("cred".into());
        let mut out = Vec::new();
        with_cx("cred", None, |cx| {
            p.on_select(cx, &mut out);
            for c in ['r', 'j', 'k', 'g', 'x'] {
                p.on_key(key(crossterm::event::KeyCode::Char(c)), cx, &mut out);
            }
        });
        p.on_event(&json!({}), &mut out);
        let v = sent(&out);
        assert!(!v.is_empty() && v.iter().all(|x| x == "list"), "{v:?}");
    }
}
