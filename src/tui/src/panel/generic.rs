//! THE PANEL FOR AN AGENT NOBODY TAUGHT THIS SHELL ABOUT.
//!
//! It shows the agent's own self-description — its sentence, its verbs with their
//! argument lines, its reach — and lets the human call any of them. That is the
//! whole point: an agent written after this binary was compiled is usable the day
//! it exists, because the kernel describes itself and this pane renders the
//! description rather than a hard-coded form.
//!
//! It is also the honest fallback for the transport spine. A leg has no
//! conversation and no files; what it has is a reflect, and this shows it.
//!
//! THE DEFAULT VIEW (design decision): what the agent IS and WHO IT TALKS TO —
//! header (id, open or sealed, its sentence), the last call, EGRESS (`can_call`,
//! `egress_rule`), INGRESS (`callable_by`, `ingress_rule`, or "sealed" when
//! absent), its verbs, and its rows. The structural baseline every agent is born
//! with (`$self`, `$parent`, `$subtree`) is folded into one dim line: on every
//! agent it's noise, but it's never hidden completely.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;

use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};
use crate::theme;

pub struct GenericPanel {
    agent: AgentId,
    scroll: u16,
    /// The verb the cursor is on, so `Enter` in the composer can prefill it.
    pub cursor: usize,
    /// WHAT THE LAST CALL ANSWERED, verbatim. A refusal included — it is the
    /// kernel's answer, not an exception, and it renders here rather than being
    /// swallowed.
    pub last: Option<Value>,
}

impl GenericPanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            scroll: 0,
            cursor: 0,
            last: None,
        }
    }

    /// The verbs this agent declares, in the order the kernel listed them.
    pub fn verbs(reflect: Option<&Value>) -> Vec<(String, String)> {
        let Some(v) = reflect
            .and_then(|r| r.get("verbs"))
            .and_then(Value::as_object)
        else {
            return Vec::new();
        };
        v.iter()
            .map(|(k, d)| {
                let doc = d
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| d.to_string());
                (k.clone(), doc)
            })
            .collect()
    }

    fn lines<'a>(&self, cx: &'a Cx<'_>) -> Vec<Line<'a>> {
        let Some(r) = cx.reflect else {
            return vec![Line::from(Span::styled(
                "  reading this agent's self-description…",
                Style::default().fg(theme::DIM),
            ))];
        };
        let mut out = Vec::new();
        let label = |t: &str| {
            Line::from(Span::styled(
                format!("  ── {t} "),
                Style::default()
                    .fg(theme::LABEL)
                    .add_modifier(Modifier::BOLD),
            ))
        };

        // HEADER: who, and whether it answers at all.
        let sealed = r.get("sealed").and_then(Value::as_bool).unwrap_or(false);
        out.push(Line::from(vec![
            Span::styled(
                format!("  {}  ", self.agent),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if sealed { "○ sealed" } else { "● open" },
                Style::default().fg(if sealed { theme::DIM } else { theme::WORKING }),
            ),
        ]));
        if let Some(s) = r.get("sentence").and_then(Value::as_str) {
            out.push(Line::from(Span::styled(
                format!("  {s}"),
                Style::default().fg(theme::FG),
            )));
        }
        out.push(Line::from(""));

        // FIRST after the header, not last. It is what the human just did: below
        // a long verb list it was off-screen and read as the call having done
        // nothing (measured 2026-09-13 on exec_bridge).
        if let Some(v) = &self.last {
            let refused = v.get("error").and_then(Value::as_str).is_some();
            out.push(Line::from(Span::styled(
                if refused {
                    "  LAST CALL — REFUSED"
                } else {
                    "  LAST CALL"
                },
                Style::default()
                    .fg(if refused { theme::FAILED } else { theme::LABEL })
                    .add_modifier(Modifier::BOLD),
            )));
            let body = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
            for l in body.lines() {
                out.push(Line::from(Span::styled(
                    format!("    {l}"),
                    Style::default().fg(if refused { theme::FAILED } else { theme::FG }),
                )));
            }
            out.push(Line::from(""));
        }

        // EGRESS and INGRESS — a refusal you can predict is a refusal you do not
        // have to earn. The kernel's own grant maps, verbatim, one per line.
        for (title, key, rule) in [
            ("EGRESS · may call", "can_call", "egress_rule"),
            ("INGRESS · callable by", "callable_by", "ingress_rule"),
        ] {
            out.push(label(title));
            let m = r.get(key).and_then(Value::as_object);
            let (base, named): (Vec<_>, Vec<_>) = m
                .map(|m| m.iter().partition(|(k, _)| k.starts_with('$')))
                .unwrap_or_default();
            if named.is_empty() {
                out.push(Line::from(Span::styled(
                    "    nobody named",
                    Style::default().fg(theme::DIM),
                )));
            }
            for (k, v) in named {
                out.push(Line::from(vec![
                    Span::styled(format!("    {k:<14}"), Style::default().fg(theme::FG)),
                    Span::styled(compact(v), Style::default().fg(theme::DIM)),
                ]));
            }
            if !base.is_empty() {
                let names: Vec<&str> = base.iter().map(|(k, _)| k.as_str()).collect();
                out.push(Line::from(Span::styled(
                    format!("    baseline: {}", names.join(" ")),
                    Style::default().fg(theme::DIM),
                )));
            }
            if let Some(v) = r.get(rule).and_then(Value::as_str) {
                out.push(Line::from(Span::styled(
                    format!("    {rule}  {v}"),
                    Style::default().fg(theme::DIM),
                )));
            }
            // SEALED is the agent's own word (its reflect says so), never an
            // inference from a missing rule: sealed-by-default is a property of
            // IO legs, not of every agent.
            if key == "callable_by" && sealed {
                out.push(Line::from(Span::styled(
                    "    sealed — only reflect and boot answer until a rule opens it",
                    Style::default().fg(theme::DIM),
                )));
            }
            out.push(Line::from(""));
        }

        let verbs = Self::verbs(Some(r));
        if verbs.is_empty() {
            out.push(Line::from(Span::styled(
                "  this agent declares no verbs of its own.",
                Style::default().fg(theme::DIM),
            )));
        } else {
            out.push(label("VERBS"));
            for (i, (name, doc)) in verbs.iter().enumerate() {
                let sel = i == self.cursor;
                let style = if sel {
                    Style::default()
                        .fg(theme::SELECT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::FG)
                };
                out.push(Line::from(vec![
                    Span::styled(if sel { "  ▸ " } else { "    " }, style),
                    Span::styled(name.clone(), style),
                ]));
                out.push(Line::from(Span::styled(
                    format!("      {doc}"),
                    Style::default().fg(theme::DIM),
                )));
            }
        }

        // ROWS: the count and the newest few, so an agent that does talk is
        // never drawn as empty.
        let rows: Vec<_> = cx.store.rows_of(cx.agent).collect();
        if !rows.is_empty() {
            out.push(Line::from(""));
            out.push(label(&format!(
                "ROWS · {} · newest {}",
                rows.len(),
                rows.len().min(3)
            )));
            for row in rows.iter().rev().take(3).rev() {
                // A call row carries no content; its verb (and name) is what it says.
                let said = row
                    .content
                    .clone()
                    .filter(|c| !c.is_empty())
                    .or_else(|| {
                        row.verb.clone().map(|v| match &row.name {
                            Some(n) => format!("{v} · {n}"),
                            None => v,
                        })
                    })
                    .unwrap_or_default();
                let head: String = said.chars().take(80).collect::<String>().replace('\n', " ");
                out.push(Line::from(Span::styled(
                    format!(
                        "    {:>5}  {:<7} {head}",
                        row.id,
                        row.kind.as_deref().unwrap_or("")
                    ),
                    Style::default().fg(theme::DIM),
                )));
            }
        }

        if let Some(readme) = r.get("readme").and_then(Value::as_str) {
            out.push(Line::from(""));
            out.push(Line::from(Span::styled(
                "  README",
                Style::default()
                    .fg(theme::LABEL)
                    .add_modifier(Modifier::BOLD),
            )));
            for l in readme.lines() {
                out.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(theme::DIM),
                )));
            }
        }
        out
    }
}

/// A grant rendered on one line. `"*"` stays `"*"`; a list stays a list.
fn compact(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().unwrap_or("?").to_string())
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

impl Panel for GenericPanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Generic
    }

    fn needs(&self) -> Needs {
        // Rows too: the default view shows their count and the newest few, and
        // an agent with none costs an empty list.
        Needs {
            rows: true,
            reflect: true,
            watch: false,
        }
    }

    fn on_key(
        &mut self,
        k: crossterm::event::KeyEvent,
        cx: &Cx<'_>,
        _out: &mut Vec<Cmd>,
    ) -> Handled {
        use crossterm::event::KeyCode as K;
        let n = Self::verbs(cx.reflect).len();
        match k.code {
            K::Down | K::Char('j') if n > 0 => {
                self.cursor = (self.cursor + 1).min(n - 1);
                Handled::Yes
            }
            K::Up | K::Char('k') if n > 0 => {
                self.cursor = self.cursor.saturating_sub(1);
                Handled::Yes
            }
            K::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                Handled::Yes
            }
            K::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Handled::Yes
            }
            K::Char('g') => {
                self.scroll = 0;
                self.cursor = 0;
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
    }

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        // A reflect is not a call result — it is this pane's own subject, already
        // rendered above. Anything else is an answer the human asked for.
        if reply.get("verbs").is_some() && reply.get("sentence").is_some() {
            return false;
        }
        self.last = Some(reply.clone());
        self.scroll = 0;
        true
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, cx: &Cx<'_>) {
        let lines = self.lines(cx);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn status(&self, cx: &Cx<'_>) -> Option<String> {
        let n = Self::verbs(cx.reflect).len();
        Some(format!("{} · {n} verbs", self.agent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verbs_come_from_the_reflect_in_kernel_order() {
        let r = json!({"verbs":{"start":"args: command:str","stop":"no args"}});
        let v = GenericPanel::verbs(Some(&r));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "start");
        assert_eq!(v[0].1, "args: command:str");
    }

    #[test]
    fn an_agent_with_no_verbs_is_not_an_error() {
        assert!(GenericPanel::verbs(None).is_empty());
        assert!(GenericPanel::verbs(Some(&json!({}))).is_empty());
    }

    #[test]
    fn a_grant_renders_as_the_kernel_wrote_it() {
        assert_eq!(compact(&json!("*")), "*");
        assert_eq!(compact(&json!(["boot", "reflect"])), "boot, reflect");
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_default_view_shows_who_it_is_and_who_it_talks_to() {
        let r = json!({
            "sentence": "reads and writes files under a root",
            "sealed": false,
            "can_call": {"$self": "*", "$parent": "*", "user": "*"},
            "callable_by": {"$self": "*", "$subtree": "*", "user": "*", "exec": ["read_stream"]},
            "ingress_rule": "allow_all_ingress",
            "verbs": {"read_stream": "args: path", "list": "args: path?"},
        });
        let p = GenericPanel::new("files".into());
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let a: AgentId = "files".into();
        let cx = Cx {
            agent: &a,
            store: &store,
            tasks: &[],
            seen: &seen,
            reflect: Some(&r),
        };
        let t = text(&p.lines(&cx));
        for want in [
            "files  ● open",
            "EGRESS · may call",
            "INGRESS · callable by",
            "ingress_rule  allow_all_ingress",
            "read_stream",
            "VERBS",
        ] {
            assert!(t.contains(want), "default view lacks {want:?}:\n{t}");
        }
        assert!(
            t.contains("exec          read_stream"),
            "a narrow grant shows its verbs:\n{t}"
        );
        assert!(
            t.contains("baseline: $self $parent"),
            "the baseline is folded, not listed:\n{t}"
        );
        assert!(
            !t.contains("    $self"),
            "the baseline is never a grant line:\n{t}"
        );
        assert!(
            !t.contains("sealed —"),
            "an open agent is never called sealed:\n{t}"
        );
    }

    #[test]
    fn a_sealed_agent_says_so_from_its_own_reflect() {
        let r = json!({"sealed": true, "callable_by": {}, "can_call": {}});
        let p = GenericPanel::new("leg".into());
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let a: AgentId = "leg".into();
        let cx = Cx {
            agent: &a,
            store: &store,
            tasks: &[],
            seen: &seen,
            reflect: Some(&r),
        };
        let t = text(&p.lines(&cx));
        assert!(
            t.contains("○ sealed") && t.contains("sealed — only reflect and boot answer"),
            "{t}"
        );
    }

    #[test]
    fn selecting_the_agent_asks_for_its_reflect() {
        let mut p = GenericPanel::new("rest".into());
        let store = crate::store::Store::default();
        let seen = std::collections::HashSet::new();
        let a: AgentId = "rest".into();
        let cx = Cx {
            agent: &a,
            store: &store,
            tasks: &[],
            seen: &seen,
            reflect: None,
        };
        let mut out = Vec::new();
        p.on_select(&cx, &mut out);
        assert!(matches!(&out[0], Cmd::Reflect { agent } if agent == "rest"));
    }
}
