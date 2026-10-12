//! Everything derived from the kernel, and nothing else.
//!
//! The store is append-mostly and holds **no view state**: no selection, no
//! scroll, no pins. Those belong to the panel, because they are the human's,
//! not the kernel's.
//!
//! THE SORT KEY IS DELIVERY ORDER. `order[agent]` is `Host::rows`' own sequence:
//! oldest id first, and ids are minted AT APPEND — by the kernel for an
//! execution row, by the AI agent for a row it appends to its memory store — so
//! that is append order for every kind the outline renders.

// `row`, `index_of` and `invalidate_cursors` complete the store's read surface.
// The outline reaches rows through `conversation()` today; these are what a pager
// and a jump-to-id use, and splitting the surface by current caller would make it
// unreadable.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::kern::model::{AgentId, Outcome, Row, RowId, Task};

/// An agent's newest `view` row, projected: what the MODEL was last given.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    pub row_id: RowId,
    pub ts: f64,
    pub budget: i64,
    pub why: String,
    /// Already in PROMPT order.
    pub order: Vec<RowId>,
    pub res: HashMap<RowId, String>,
    /// Index, in the owning agent's delivery order, of the newest id this view
    /// names. Rows after it ride in the prompt IN FULL — the view array is the
    /// head as of its write, and the log keeps growing.
    pub head_idx: usize,
}

#[derive(Debug, Default)]
pub struct Store {
    pub rows: HashMap<RowId, Row>,
    /// Per agent, the kernel's append order. THE sort key.
    pub order: HashMap<AgentId, Vec<RowId>>,
    /// pid → outcome, from a `settle` row or from the live stamp on a call row.
    pub outcome: HashMap<RowId, Outcome>,
    /// pid → the ts its outcome landed (settle row ts, else the call's own).
    pub settle_ts: HashMap<RowId, f64>,
    /// row → parent, from `row.parent` only. Immutable; never `ps.parent`.
    pub parent: HashMap<RowId, RowId>,
    /// pid → the id of its `body` row.
    pub body_of: HashMap<RowId, RowId>,
    pub views: HashMap<AgentId, View>,
    /// The id of the LAST row seen for that agent. NOT a high-water mark: ids
    /// are minted at turn start and the row lands at turn end, so a turn's row
    /// carries a LOWER id than rows appended before it.
    pub cursor: HashMap<AgentId, RowId>,
    /// AI agents whose `rows` said `loaded:false`: they hold no history yet
    /// (boot is lazy — it loads on the agent's next turn).
    pub unloaded: std::collections::HashSet<AgentId>,
    /// Bumped on every delivery: what a projection is keyed on, so a view
    /// re-derives only when the rows actually changed.
    pub rev: u64,
}

impl Store {
    /// A page delivered WITH `after`: upsert each row, append ids that are new.
    ///
    /// A later copy of a row wins: a re-read carries the `outcome` stamp the
    /// first delivery lacked.
    pub fn apply_page(&mut self, agent: &AgentId, rows: Vec<Row>) {
        self.rev += 1;
        if rows.is_empty() {
            return;
        }
        let known: HashSet<RowId> = self
            .order
            .get(agent)
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();
        let ord = self.order.entry(agent.clone()).or_default();
        for r in &rows {
            if !known.contains(&r.id) {
                ord.push(r.id);
            }
        }
        let last = rows.last().map(|r| r.id);
        self.absorb(rows);
        if let Some(id) = last {
            self.cursor.insert(agent.clone(), id);
        }
        self.reindex_view(agent);
    }

    /// A page delivered WITHOUT `after`: the order becomes the delivered
    /// sequence. This is what makes a re-read canonical rather than appended,
    /// and it is why the outline is identical after a restart.
    pub fn apply_full(&mut self, agent: &AgentId, rows: Vec<Row>) {
        self.rev += 1;
        let mut seen = HashSet::new();
        let mut ord = Vec::with_capacity(rows.len());
        for r in &rows {
            if seen.insert(r.id) {
                ord.push(r.id);
            }
        }
        let last = ord.last().copied();
        self.order.insert(agent.clone(), ord);
        self.absorb(rows);
        match last {
            Some(id) => {
                self.cursor.insert(agent.clone(), id);
            }
            None => {
                self.cursor.remove(agent);
            }
        }
        self.reindex_view(agent);
    }

    /// Index one delivery's worth of rows. Shared by both apply paths.
    fn absorb(&mut self, rows: Vec<Row>) {
        for r in rows {
            if let Some(p) = r.parent {
                self.parent.insert(r.id, p);
            }
            // Two shapes carry an outcome, and both are in the wild: a durable
            // `settle` row owned by the root, and the live stamp on the call row.
            if r.kind_is("settle") {
                if let (Some(pid), Some(o)) = (r.parent, r.outcome.clone()) {
                    self.settle_ts.insert(pid, r.ts);
                    self.outcome.insert(pid, o);
                }
            } else if let Some(o) = r.outcome.clone() {
                self.settle_ts.entry(r.id).or_insert(r.ts);
                self.outcome.insert(r.id, o);
            }
            if r.kind_is("body") {
                if let Some(pid) = r.parent {
                    self.body_of.insert(pid, r.id);
                }
            }
            self.rows.insert(r.id, r);
        }
    }

    /// Recompute the agent's current view projection: newest `view` row wins.
    fn reindex_view(&mut self, agent: &AgentId) {
        let Some(ord) = self.order.get(agent) else {
            return;
        };
        let newest = ord
            .iter()
            .rev()
            .filter_map(|id| self.rows.get(id))
            .find(|r| r.kind_is("view"));
        let Some(row) = newest else { return };
        let entries = row.ids.clone().unwrap_or_default();
        let mut res = HashMap::with_capacity(entries.len());
        let mut order = Vec::with_capacity(entries.len());
        for e in &entries {
            order.push(e.id);
            res.insert(e.id, e.res.clone());
        }
        // The head is the LAST position (in delivery order) that this view
        // names. Everything after it was appended since the projection was
        // written and rides in the prompt in full.
        let pos: HashMap<RowId, usize> = ord.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let head_idx = order
            .iter()
            .filter_map(|id| pos.get(id))
            .copied()
            .max()
            .unwrap_or(0);
        let view = View {
            row_id: row.id,
            ts: row.ts,
            budget: row.budget.unwrap_or(0),
            why: row.why.clone().unwrap_or_default(),
            order,
            res,
            head_idx,
        };
        self.views.insert(agent.clone(), view);
    }

    pub fn index_of(&self, agent: &AgentId, id: RowId) -> Option<usize> {
        self.order.get(agent)?.iter().position(|r| *r == id)
    }

    pub fn row(&self, id: RowId) -> Option<&Row> {
        self.rows.get(&id)
    }

    /// Walk `row.parent` to the top. Cycle-safe: a malformed chain terminates
    /// at the first id it has already seen rather than hanging the frame.
    pub fn root_of(&self, id: RowId) -> RowId {
        let mut cur = id;
        let mut seen = HashSet::new();
        while let Some(&p) = self.parent.get(&cur) {
            if !seen.insert(cur) || !self.rows.contains_key(&p) {
                break;
            }
            cur = p;
        }
        cur
    }

    /// Every row of this agent, in delivery order.
    pub fn rows_of<'a>(&'a self, agent: &AgentId) -> impl Iterator<Item = &'a Row> + 'a {
        self.order
            .get(agent)
            .into_iter()
            .flatten()
            .filter_map(move |id| self.rows.get(id))
    }

    /// The conversation: what the model is shown, bookkeeping dropped.
    pub fn conversation<'a>(&'a self, agent: &AgentId) -> impl Iterator<Item = &'a Row> + 'a {
        self.rows_of(agent).filter(|r| r.is_conversation())
    }

    /// Clear every cursor so the next read of each agent is a full one.
    pub fn invalidate_cursors(&mut self) {
        self.cursor.clear();
    }

    /// A task's root, walked through ROW parents — the task table re-parents on
    /// forget, so its own `parent` cannot be trusted for attribution.
    pub fn task_root(&self, t: &Task) -> RowId {
        self.root_of(t.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: serde_json::Value) -> Row {
        serde_json::from_value(v).unwrap()
    }
    fn a() -> AgentId {
        "ai".to_string()
    }

    #[test]
    fn apply_page_appends_and_upserts() {
        let mut s = Store::default();
        s.apply_page(
            &a(),
            vec![row(json!({"id":1,"agent":"ai","content":"one"}))],
        );
        s.apply_page(
            &a(),
            vec![row(json!({"id":2,"agent":"ai","content":"two"}))],
        );
        assert_eq!(s.order[&a()], vec![1, 2]);
        assert_eq!(s.cursor[&a()], 2);
        // A re-delivery of 1 carrying the outcome it lacked: upsert, no dup.
        s.apply_page(
            &a(),
            vec![row(
                json!({"id":1,"agent":"ai","kind":"call","outcome":{"status":"errored"}}),
            )],
        );
        assert_eq!(
            s.order[&a()],
            vec![1, 2],
            "a known id is never appended twice"
        );
        assert!(s.outcome[&1].failed());
    }

    #[test]
    fn apply_full_rebuilds_order() {
        let mut s = Store::default();
        s.apply_page(&a(), vec![row(json!({"id":9,"agent":"ai"}))]);
        s.apply_full(
            &a(),
            vec![
                row(json!({"id":1,"agent":"ai"})),
                row(json!({"id":2,"agent":"ai"})),
            ],
        );
        assert_eq!(
            s.order[&a()],
            vec![1, 2],
            "a full read is canonical, not appended"
        );
        assert_eq!(s.cursor[&a()], 2);
    }

    #[test]
    fn order_is_delivery_not_id() {
        // A long turn mints 10 early and settles late: delivery order is the truth.
        let mut s = Store::default();
        s.apply_full(
            &a(),
            vec![
                row(json!({"id":20,"agent":"ai","content":"finished first"})),
                row(json!({"id":10,"agent":"ai","content":"started first, finished last"})),
            ],
        );
        assert_eq!(s.order[&a()], vec![20, 10]);
        assert_eq!(s.index_of(&a(), 10), Some(1));
    }

    #[test]
    fn settle_rows_on_the_root_index_outcomes() {
        let mut s = Store::default();
        let root: AgentId = "kernel_state".into();
        s.apply_full(
            &root,
            vec![row(
                json!({"id":99,"agent":"kernel_state","kind":"settle","parent":4711,"settled":"ai",
                       "ts":7.0,"outcome":{"status":"errored","error":"nope"}}),
            )],
        );
        assert!(s.outcome[&4711].failed());
        assert_eq!(s.settle_ts[&4711], 7.0);
    }

    #[test]
    fn body_row_is_indexed_by_its_call() {
        let mut s = Store::default();
        s.apply_full(
            &a(),
            vec![row(
                json!({"id":5018,"agent":"ai","kind":"body","parent":5013,"content":"…"}),
            )],
        );
        assert_eq!(s.body_of[&5013], 5018);
    }

    #[test]
    fn root_of_is_cycle_safe() {
        let mut s = Store::default();
        s.apply_full(
            &a(),
            vec![
                row(json!({"id":1,"agent":"ai","parent":2})),
                row(json!({"id":2,"agent":"ai","parent":1})),
            ],
        );
        // Terminates rather than hanging the frame.
        let _ = s.root_of(1);
    }

    #[test]
    fn root_of_walks_to_the_parentless_row() {
        let mut s = Store::default();
        s.apply_full(
            &a(),
            vec![
                row(json!({"id":100,"agent":"ai"})),
                row(json!({"id":101,"agent":"ai","parent":100})),
                row(json!({"id":102,"agent":"ai","parent":101})),
            ],
        );
        assert_eq!(s.root_of(102), 100);
    }

    #[test]
    fn view_head_is_the_newest_named_position() {
        let mut s = Store::default();
        s.apply_full(
            &a(),
            vec![
                row(json!({"id":1,"agent":"ai","content":"a"})),
                row(json!({"id":2,"agent":"ai","content":"b"})),
                row(json!({"id":3,"agent":"ai","kind":"view","v":1,"budget":100,"why":"projection: fold",
                           "ids":[{"id":1,"res":"stub"},{"id":2,"res":"full"}]})),
                row(json!({"id":4,"agent":"ai","content":"appended since"})),
            ],
        );
        let v = &s.views[&a()];
        assert_eq!(v.res[&1], "stub");
        assert_eq!(v.budget, 100);
        assert_eq!(v.why, "projection: fold");
        assert_eq!(v.head_idx, 1, "the newest named id is 2, at index 1");
        assert!(
            s.index_of(&a(), 4).unwrap() > v.head_idx,
            "row 4 rides behind the head"
        );
    }

    #[test]
    fn newest_view_wins() {
        let mut s = Store::default();
        s.apply_full(
            &a(),
            vec![
                row(json!({"id":1,"agent":"ai","kind":"view","v":1,"why":"old","ids":[]})),
                row(json!({"id":2,"agent":"ai","kind":"view","v":1,"why":"new","ids":[]})),
            ],
        );
        assert_eq!(s.views[&a()].why, "new");
    }

    #[test]
    fn conversation_drops_exactly_the_bookkeeping_kinds() {
        let mut s = Store::default();
        s.apply_full(
            &a(),
            vec![
                row(json!({"id":1,"agent":"ai","content":"turn"})),
                row(json!({"id":2,"agent":"ai","kind":"call"})),
                row(json!({"id":3,"agent":"ai","kind":"task"})),
                row(json!({"id":4,"agent":"ai","kind":"body"})),
                row(json!({"id":5,"agent":"ai","kind":"view","ids":[]})),
                row(json!({"id":6,"agent":"ai","kind":"settle"})),
                row(json!({"id":7,"agent":"ai","kind":"fold","refs":[1]})),
                row(json!({"id":8,"agent":"ai","kind":"recall"})),
            ],
        );
        let ids: Vec<_> = s.conversation(&a()).map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 7, 8]);
    }
}
