//! THE PANEL FOR `ram_store` AND `sqlite_store`: who owns rows, how many, how
//! many folds, and a search box over the caller's own text index.
//!
//! A COUNTS VIEW. `dump` answers every row of every owner; this panel keeps only
//! the owner name and two numbers, so no row body can ever reach a line. The
//! search verb answers the caller's OWN lines only, which is all it shows.
//! Read-only: only `dump` and `search` are ever sent.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode as K, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;
use serde_json::{json, Value};

use super::readonly as ro;
use crate::ev::Cmd;
use crate::kern::model::AgentId;
use crate::panel::{Cx, Handled, Needs, Panel, PanelKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hits {
    pub query: String,
    /// (id, line, text)
    pub matches: Vec<(i64, i64, String)>,
    pub total: u64,
    pub truncated: bool,
}

pub struct StorePanel {
    agent: AgentId,
    scroll: u16,
    cursor: usize,
    /// owner -> (rows, folds). Counts only.
    pub owners: BTreeMap<String, (usize, usize)>,
    pub total: usize,
    pub loaded: bool,
    pub dump_error: Option<String>,
    pub hits: Option<Hits>,
    pub search_error: Option<String>,
    /// The search field, while open.
    pub input: Option<String>,
    asked: String,
    search_pending: bool,
}

impl StorePanel {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            scroll: 0,
            cursor: 0,
            owners: BTreeMap::new(),
            total: 0,
            loaded: false,
            dump_error: None,
            hits: None,
            search_error: None,
            input: None,
            asked: String::new(),
            search_pending: false,
        }
    }

    fn dump(&self, out: &mut Vec<Cmd>) {
        out.push(Cmd::Send {
            target: self.agent.clone(),
            payload: json!({"type": "dump"}),
        });
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Some(inp) = &self.input {
            out.push(ro::plain(format!("  search: {inp}▏")));
            out.push(Line::from(""));
        }
        if let Some(e) = &self.dump_error {
            out.push(ro::bad(format!("  owners not readable: {e}")));
        } else if !self.loaded {
            out.push(ro::dim("  reading…".into()));
        } else {
            out.push(ro::head(&format!(
                "OWNERS · {} · {} rows",
                self.owners.len(),
                self.total
            )));
            if self.owners.is_empty() {
                out.push(ro::dim("    no rows yet".into()));
            }
            for (o, (rows, folds)) in &self.owners {
                out.push(ro::plain(format!("    {o}  {rows} rows  {folds} folds")));
            }
        }
        out.push(Line::from(""));
        out.push(ro::head("SEARCH"));
        if let Some(e) = &self.search_error {
            out.push(ro::bad(format!("    refused: {e}")));
        }
        match &self.hits {
            None if self.search_error.is_none() => {
                out.push(ro::dim("    / to search your own rows".into()))
            }
            None => {}
            Some(h) => {
                out.push(ro::dim(format!(
                    "    \"{}\" · {} hits{}",
                    h.query,
                    h.total,
                    if h.truncated { " (truncated)" } else { "" }
                )));
                for (id, line, text) in &h.matches {
                    out.push(ro::plain(format!("    {id}:{line}  {text}")));
                }
            }
        }
        out
    }

    fn key_in_field(&mut self, k: crossterm::event::KeyEvent, out: &mut Vec<Cmd>) -> Handled {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return Handled::No;
        }
        let Some(inp) = self.input.as_mut() else {
            return Handled::No;
        };
        match k.code {
            K::Esc => self.input = None,
            K::Backspace => {
                inp.pop();
            }
            K::Char(c) => inp.push(c),
            K::Enter => {
                let q = inp.trim().to_string();
                self.input = None;
                if !q.is_empty() {
                    self.asked = q.clone();
                    self.search_pending = true;
                    out.push(Cmd::Send {
                        target: self.agent.clone(),
                        payload: json!({"type": "search", "query": q}),
                    });
                }
            }
            _ => {}
        }
        Handled::Yes
    }
}

impl Panel for StorePanel {
    fn kind(&self) -> PanelKind {
        PanelKind::Store
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
        if self.input.is_some() {
            return self.key_in_field(k, out);
        }
        match k.code {
            K::Char('/') => {
                self.input = Some(String::new());
                Handled::Yes
            }
            K::Char('r') => {
                self.dump(out);
                Handled::Yes
            }
            _ => {
                let n = self.owners.len();
                if ro::nav(k, &mut self.cursor, &mut self.scroll, n) {
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
        }
    }

    fn on_event(&mut self, _payload: &Value, out: &mut Vec<Cmd>) -> bool {
        self.dump(out);
        true
    }

    fn on_select(&mut self, _cx: &Cx<'_>, out: &mut Vec<Cmd>) {
        out.push(Cmd::Reflect {
            agent: self.agent.clone(),
        });
        self.dump(out);
    }

    fn on_reply(&mut self, reply: &Value, _out: &mut Vec<Cmd>) -> bool {
        if reply.get("verbs").is_some() && reply.get("sentence").is_some() {
            return false;
        }
        if let Some(e) = ro::refusal(reply) {
            if self.search_pending {
                self.search_pending = false;
                self.search_error = Some(e);
            } else {
                self.dump_error = Some(e);
            }
            return true;
        }
        if let (Some(rows), Some(folds)) = (
            reply.get("rows").and_then(Value::as_array),
            reply.get("fold_refs").and_then(Value::as_array),
        ) {
            // counts only: the row object is never kept
            let mut owners: BTreeMap<String, (usize, usize)> = BTreeMap::new();
            for r in rows {
                owners.entry(ro::st(r, "owner")).or_default().0 += 1;
            }
            for f in folds {
                owners.entry(ro::st(f, "owner")).or_default().1 += 1;
            }
            self.total = rows.len();
            self.owners = owners;
            self.loaded = true;
            self.dump_error = None;
            return true;
        }
        if let Some(m) = reply.get("matches").and_then(Value::as_array) {
            self.search_pending = false;
            self.search_error = None;
            self.hits = Some(Hits {
                query: self.asked.clone(),
                matches: m
                    .iter()
                    .map(|x| {
                        (
                            x.get("id").and_then(Value::as_i64).unwrap_or(0),
                            x.get("line").and_then(Value::as_i64).unwrap_or(0),
                            ro::st(x, "text"),
                        )
                    })
                    .collect(),
                total: reply
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or(m.len() as u64),
                truncated: reply.get("truncated").and_then(Value::as_bool) == Some(true),
            });
            return true;
        }
        false
    }

    fn text_input(&self) -> bool {
        self.input.is_some()
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn render(&self, f: &mut Frame<'_>, area: Rect, _cx: &Cx<'_>) {
        ro::draw(f, area, self.lines(), self.scroll);
    }

    fn status(&self, _cx: &Cx<'_>) -> Option<String> {
        Some(format!("{} · {} owners", self.agent, self.owners.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::ro::testing::*;
    use super::*;

    const BODY: &str = "SENTINEL-ROW-BODY";

    fn dump() -> Value {
        json!({
            "rows":[
                {"owner":"alice","row":{"id":1,"content":BODY}},
                {"owner":"alice","row":{"id":2,"content":BODY}},
                {"owner":"alice","row":{"id":3,"content":BODY}},
                {"owner":"bob","row":{"id":1,"content":BODY}},
            ],
            "fold_refs":[{"owner":"alice","fold":3,"members":[1,2]}]
        })
    }

    #[test]
    fn owners_show_row_and_fold_counts() {
        let mut p = StorePanel::new("ram".into());
        assert!(p.on_reply(&dump(), &mut Vec::new()));
        let t = text(&p.lines());
        assert!(
            t.contains("alice  3 rows  1 folds") && t.contains("bob  1 rows  0 folds"),
            "{t}"
        );
        assert!(t.contains("4 rows"), "{t}");
    }

    #[test]
    fn no_row_body_appears_in_the_output() {
        let mut p = StorePanel::new("ram".into());
        p.on_reply(&dump(), &mut Vec::new());
        assert!(!text(&p.lines()).contains(BODY));
        assert!(!format!("{:?}", p.owners).contains(BODY));
    }

    #[test]
    fn search_box_captures_text_and_sends_search() {
        let mut p = StorePanel::new("ram".into());
        let mut out = Vec::new();
        with_cx("ram", None, |cx| {
            assert!(!p.text_input());
            p.on_key(key(K::Char('/')), cx, &mut out);
            assert!(p.text_input());
            // `r` and `q` are global keys elsewhere; here they are text
            for c in ['r', 'q', 'x'] {
                assert_eq!(p.on_key(key(K::Char(c)), cx, &mut out), Handled::Yes);
            }
            p.on_key(key(K::Backspace), cx, &mut out);
            assert!(text(&p.lines()).contains("search: rq"));
            assert!(out.is_empty());
            p.on_key(key(K::Enter), cx, &mut out);
        });
        assert!(!p.text_input());
        match &out[..] {
            [Cmd::Send { payload, .. }] => {
                assert_eq!(payload["type"], "search");
                assert_eq!(payload["query"], "rq");
            }
            o => panic!("{o:?}"),
        }
        // Esc closes without sending
        let mut out = Vec::new();
        with_cx("ram", None, |cx| {
            p.on_key(key(K::Char('/')), cx, &mut out);
            p.on_key(key(K::Esc), cx, &mut out);
        });
        assert!(!p.text_input() && out.is_empty());
    }

    #[test]
    fn search_hits_render_and_a_refused_dump_still_leaves_search_usable() {
        let mut p = StorePanel::new("ram".into());
        p.on_reply(&json!({"error":"not_allowed"}), &mut Vec::new());
        assert!(text(&p.lines()).contains("owners not readable: not_allowed"));
        let mut out = Vec::new();
        with_cx("ram", None, |cx| {
            p.on_key(key(K::Char('/')), cx, &mut out);
            p.on_key(key(K::Char('a')), cx, &mut out);
            p.on_key(key(K::Enter), cx, &mut out);
        });
        p.on_reply(
            &json!({"matches":[{"id":7,"line":2,"text":"my own line"}],"total":1,"truncated":false}),
            &mut out,
        );
        let t = text(&p.lines());
        assert!(
            t.contains("owners not readable")
                && t.contains("7:2  my own line")
                && t.contains("1 hits"),
            "{t}"
        );
        // a refused search is shown on the search section
        p.input = Some("z".into());
        with_cx("ram", None, |cx| p.on_key(key(K::Enter), cx, &mut out));
        p.on_reply(&json!({"error":"missing"}), &mut out);
        assert!(text(&p.lines()).contains("refused: missing"));
    }

    #[test]
    fn both_store_modules_get_the_same_panel() {
        assert_eq!(
            PanelKind::of("a", "ram_store.tools"),
            PanelKind::of("b", "sqlite_store.tools")
        );
        assert_eq!(PanelKind::of("a", "ram_store.tools"), PanelKind::Store);
    }

    #[test]
    fn on_select_asks_dump_and_only_reads_are_sent() {
        let mut p = StorePanel::new("ram".into());
        let mut out = Vec::new();
        with_cx("ram", None, |cx| p.on_select(cx, &mut out));
        assert_eq!(sent(&out), vec!["dump"]);
    }
}
