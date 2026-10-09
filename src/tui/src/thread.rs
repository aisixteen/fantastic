//! The outline: rows → the threaded, folded view a human reads.
//!
//! PURE. No clock, no I/O, recomputed from scratch on every dirty frame. A
//! 400-row conversation costs microseconds over two hash maps, so there is no
//! incremental path to get subtly wrong — and this is exactly what makes
//! CONTINUOUS FOLDING fall out: a fold row arriving from a poll collapses its
//! members immediately, because the outline *is* the current fold tree rather
//! than a thing that gets patched toward it.
//!
//! Two invariants the tests pin:
//!
//! * **The model never sees threads.** Every link here is read back out of rows
//!   the kernel already wrote — `parent`, the `re` in a vouched header, the `↑`
//!   in a reply tag. The CLI infers the shape; the kernel is not asked to keep
//!   one.
//! * **The gutter is the view row and nothing else.** What the model can see is
//!   a statement the kernel made, never a guess this file assembles.

// The outline's vocabulary is declared WHOLE — `FoldMark`, `Mark::glyph`,
// `reconcile_open` — so a re-skin or a new zoom has one place to read. The AI
// panel consumes it as agents that write folds appear in a roster; nothing here
// is forgotten code.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::kern::model::{AgentId, Row, RowId, Task};
use crate::store::Store;
use crate::tags;

/// WHAT THE MODEL SEES — read off the agent's newest `view` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// In the prompt, whole.
    Full,
    /// In the prompt as its tag line; the body was stripped.
    Stub,
    /// In the prompt as a fold.
    Fold,
    /// NOT in the prompt. You expanded past it; the model would have to
    /// `recall` or `pull` to see it.
    NotInView,
    /// A `res` value this build has never heard of. Shown, never guessed at.
    Unknown,
}

impl Mark {
    pub fn glyph(self) -> char {
        match self {
            Mark::Full => crate::theme::MARK_FULL,
            Mark::Stub => crate::theme::MARK_STUB,
            Mark::Fold => crate::theme::MARK_FOLD,
            Mark::NotInView => crate::theme::MARK_OUT,
            Mark::Unknown => '?',
        }
    }
}

/// A root's state, from the task table and settle rows — never from prose.
///
/// There is NO "blocked": a question to the human never stops a conversation
/// (design decision — inquiries are non-blocking cards). A thread with an open
/// question keeps whatever state its work is in, and carries `asking` beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closure {
    Running,
    Failed,
    Closed,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zoom {
    #[default]
    All,
    Decisions,
    Roots,
}

impl Zoom {
    pub fn next(self) -> Self {
        match self {
            Zoom::All => Zoom::Decisions,
            Zoom::Decisions => Zoom::Roots,
            Zoom::Roots => Zoom::All,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Zoom::All => "all",
            Zoom::Decisions => "decisions",
            Zoom::Roots => "roots",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    Turn {
        assistant: bool,
    },
    Fold {
        level: u32,
        refs: usize,
    },
    Recall,
    Late,
    /// A message from another agent that nobody has answered yet.
    Waiting,
    /// An `exec_bridge` monitor's delivery: what a watched job printed that
    /// matched. A notification, never a question — nothing answers it.
    Monitor {
        id: RowId,
        job: RowId,
        label: Option<String>,
        ended: bool,
    },
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutlineLine {
    pub id: RowId,
    pub agent: AgentId,
    pub depth: u8,
    pub kind: LineKind,
    /// One line, unclipped — width is a RENDER concern.
    pub head: String,
    /// The row's whole text as the transcript shows it (`tags::body_of`):
    /// wrapped by the renderer, never clipped.
    pub body: String,
    /// A TOOL REPLY row (`[reply to …]` blocks) — what a call brought back.
    pub reply: bool,
    pub mark: Mark,
    pub closure: Closure,
    /// An open question to the human sits in this line's thread. A badge, not a
    /// state: the thread goes on either way.
    pub asking: bool,
    pub expandable: bool,
    pub expanded: bool,
    pub pinned: bool,
    /// The live fold that hides this row, when the user has opened past one.
    pub covered_by: Option<RowId>,
    pub from: Option<AgentId>,
    pub msg: Option<RowId>,
    pub re: Option<RowId>,
    pub bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewHeader {
    pub rows: usize,
    pub budget: i64,
    pub why: String,
    pub ts: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outline {
    pub lines: Vec<OutlineLine>,
    pub view: Option<ViewHeader>,
}

impl Outline {
    pub fn position(&self, id: RowId) -> Option<usize> {
        self.lines.iter().position(|l| l.id == id)
    }
}

/// The human's own state. The ONLY piece that survives a refold is `pins` —
/// everything else is re-derived, because everything else is the kernel's.
#[derive(Debug, Clone, Default)]
pub struct OutlineState {
    pub pins: HashSet<RowId>,
    pub open: HashSet<RowId>,
    pub zoom: Zoom,
    pub model_view_only: bool,
    pub narrow: Option<RowId>,
    /// A ROW ID, never a line index: after a refold an index points at an
    /// unrelated row.
    pub cursor: Option<RowId>,
}

/// A message addressed to this agent that nobody has answered — the whole of
/// "waiting on you", derived rather than tracked.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    pub row: RowId,
    pub from: AgentId,
    pub head: String,
    pub ts: f64,
}

/// Open questions for `agent`: rows that arrived from someone else and have no
/// answer pointing back at them. The HUMAN's own messages are never questions
/// to the human: a line you typed to an agent is waiting on that agent, not on
/// you (measured: every message typed to `ai` showed as `✉ asks you`).
///
/// Nothing is parked anywhere — an `inquiry` settles the moment it is sent, and
/// the question is just a row. So "who is waiting on me" is a QUERY, and this
/// is it.
pub fn waiting_for(store: &Store, agent: &AgentId, human: &str) -> Vec<Waiting> {
    // Every `re` anyone has written, across every agent we have read.
    let answered: HashSet<RowId> = store
        .rows
        .values()
        .filter_map(|r| r.content.as_deref())
        .filter_map(tags::vouched)
        .filter_map(|v| v.re)
        .collect();
    store
        .conversation(agent)
        .filter_map(|r| {
            let content = r.content.as_deref()?;
            let v = tags::vouched(content)?;
            if &v.from == agent || v.from == human || answered.contains(&r.id) {
                return None;
            }
            // A monitor's delivery tells; it asks nothing.
            if tags::monitor(content).is_some() {
                return None;
            }
            Some(Waiting {
                row: r.id,
                from: v.from,
                head: tags::head_of(content),
                ts: r.ts,
            })
        })
        .collect()
}

pub struct Args<'a> {
    pub store: &'a Store,
    pub agent: &'a AgentId,
    pub tasks: &'a [Task],
    pub waiting: &'a [Waiting],
    pub seen: &'a HashSet<RowId>,
    pub st: &'a OutlineState,
}

/// The whole projection. See the module docs for the two invariants.
pub fn outline(a: Args<'_>) -> Outline {
    let mut conv: Vec<&Row> = a.store.conversation(a.agent).collect();
    if conv.is_empty() {
        return Outline::default();
    }
    // ── 1. THE CLEAR BOUNDARY. `context_clear` writes a `clear` row, and the
    // model's thread starts there. So does the drawn one: everything before the
    // NEWEST clear (in append order — ids are not monotonic) is behind it. It
    // is not deleted — the clear row is the handle: open or pin it and the rows
    // behind come back, exactly like opening any thread.
    if let Some(at) = conv.iter().rposition(|r| r.kind_is("clear")) {
        let clear = conv[at].id;
        if !a.st.open.contains(&clear) && !a.st.pins.contains(&clear) {
            conv.drain(..at);
        }
    }
    // NOTICES are not drawn: `kind: "notice"` rows are the AI layer's own turns
    // to its model (the no-call reminder, a malformed-call report, a refused
    // await) — the model reads them, a person need not. A link INTO one lifts to
    // the notice's own parent, so the reply after it stays in its thread.
    let notice_up: HashMap<RowId, Option<RowId>> =
        conv.iter().filter(|r| r.kind_is("notice")).map(|r| (r.id, r.parent)).collect();
    conv.retain(|r| !r.kind_is("notice"));
    let past_notices = |mut id: RowId| -> Option<RowId> {
        for _ in 0..64 {
            match notice_up.get(&id) {
                Some(up) => id = (*up)?,
                None => return Some(id),
            }
        }
        None
    };
    if conv.is_empty() {
        return Outline::default();
    }
    let pos: HashMap<RowId, usize> = conv.iter().enumerate().map(|(i, r)| (r.id, i)).collect();

    // ── 2. Fold cover. A FLAT union over every fold present, which is what the
    // kernel's own `covered()` computes — not a transitive closure from the
    // live folds. A level-2 fold's refs are level-1 fold ids, so those fall
    // into `covered` and surface their parent for free.
    let folds: Vec<&Row> = conv.iter().copied().filter(|r| r.kind_is("fold")).collect();
    let covered: HashSet<RowId> = folds
        .iter()
        .flat_map(|f| f.refs.clone().unwrap_or_default())
        .collect();

    // ── 3-4. Visible set, and where each fold sits: the earliest position of
    // any row it directly covers. A fold whose refs are all themselves hidden
    // has no slot and is emitted NOWHERE — the kernel drops it too.
    let mut slotted: Vec<(usize, &Row)> = Vec::with_capacity(conv.len());
    for r in conv.iter().copied() {
        if covered.contains(&r.id) {
            continue;
        }
        if r.kind_is("fold") {
            let slot = r
                .refs
                .iter()
                .flatten()
                .filter(|id| covered.contains(id))
                .filter_map(|id| pos.get(id))
                .copied()
                .min();
            match slot {
                Some(s) => slotted.push((s, r)),
                None => continue,
            }
        } else {
            slotted.push((pos[&r.id], r));
        }
    }
    // Stable by slot: two rows can share one only if a fold lands on a row's
    // own position, and then the fold (added later) sorts after it.
    slotted.sort_by_key(|(s, _)| *s);
    let visible: Vec<&Row> = slotted.iter().map(|(_, r)| *r).collect();
    let vis_ids: HashSet<RowId> = visible.iter().map(|r| r.id).collect();

    // Which live fold hides a given row, for the "you opened past it" case.
    let mut hidden_by: HashMap<RowId, RowId> = HashMap::new();
    for f in &folds {
        if vis_ids.contains(&f.id) {
            for id in f.refs.iter().flatten() {
                hidden_by.insert(*id, f.id);
            }
        }
    }

    // ── 5. Links. A hidden row's link lifts to the fold that hides it, so a
    // reply into a folded turn hangs off the fold rather than disappearing.
    let lift = |id: RowId| -> Option<RowId> {
        let id = past_notices(id)?;
        if vis_ids.contains(&id) {
            Some(id)
        } else {
            hidden_by.get(&id).copied()
        }
    };
    let mut parent_of: HashMap<RowId, RowId> = HashMap::new();
    let mut vouch: HashMap<RowId, tags::Vouched> = HashMap::new();
    for r in &visible {
        let content = r.content.as_deref().unwrap_or("");
        let v = tags::vouched(content);
        let up = tags::reply(content).and_then(|t| t.up);
        let cand = r
            .parent
            .and_then(lift)
            .or_else(|| v.as_ref().and_then(|v| v.re).and_then(lift))
            .or_else(|| up.and_then(lift));
        if let Some(p) = cand {
            if p != r.id {
                parent_of.insert(r.id, p);
            }
        }
        if let Some(v) = v {
            vouch.insert(r.id, v);
        }
    }

    // ── 6-7. Roots and their closure.
    let kids: HashMap<RowId, Vec<RowId>> = {
        let mut m: HashMap<RowId, Vec<RowId>> = HashMap::new();
        for r in &visible {
            if let Some(p) = parent_of.get(&r.id) {
                m.entry(*p).or_default().push(r.id);
            }
        }
        m
    };
    let waiting_ids: HashSet<RowId> = a.waiting.iter().map(|w| w.row).collect();
    let running_roots: HashSet<RowId> = a
        .tasks
        .iter()
        .filter(|t| crate::presence::is_work(t))
        .map(|t| a.store.task_root(t))
        .collect();

    let subtree = |id: RowId| -> Vec<RowId> {
        let mut out = vec![id];
        let mut i = 0;
        let mut guard = HashSet::new();
        while i < out.len() {
            let cur = out[i];
            i += 1;
            if !guard.insert(cur) {
                continue;
            }
            if let Some(cs) = kids.get(&cur) {
                out.extend(cs.iter().copied());
            }
        }
        out
    };

    let asking_of = |id: RowId| -> bool { subtree(id).iter().any(|i| waiting_ids.contains(i)) };

    let closure_of = |id: RowId| -> Closure {
        let sub = subtree(id);
        if sub.iter().any(|i| running_roots.contains(i))
            || running_roots.contains(&a.store.root_of(id))
        {
            return Closure::Running;
        }
        if sub
            .iter()
            .any(|i| a.store.outcome.get(i).is_some_and(|o| o.failed()))
        {
            return Closure::Failed;
        }
        // CLOSED means the kernel settled it. A conversation the kernel never
        // dispatched (a plain exchange) is `Plain` and stays open — there is no
        // outcome to hide behind a summary.
        if sub.iter().any(|i| a.store.settle_ts.contains_key(i)) {
            Closure::Closed
        } else {
            Closure::Plain
        }
    };

    // ── the gutter: the view row, and only the view row.
    let view = a.store.views.get(a.agent);
    let mark_of = |r: &Row| -> Mark {
        let Some(v) = view else { return Mark::Full };
        match v.res.get(&r.id).map(String::as_str) {
            Some("full") => Mark::Full,
            Some("stub") => Mark::Stub,
            Some("fold") => Mark::Fold,
            Some(_) => Mark::Unknown,
            // Load-bearing: the view array is the head AS OF ITS WRITE and the
            // log keeps growing. Every row appended since rides in the prompt
            // in full, and marking those `·` would lie about the model's
            // context in the commonest case there is.
            None => match pos.get(&r.id) {
                Some(i) if *i > v.head_idx => Mark::Full,
                _ => Mark::NotInView,
            },
        }
    };

    let kind_of = |r: &Row| -> LineKind {
        if waiting_ids.contains(&r.id) {
            return LineKind::Waiting;
        }
        if let Some(m) = r.content.as_deref().and_then(tags::monitor) {
            return LineKind::Monitor { id: m.id, job: m.job, label: m.label, ended: m.ended };
        }
        match r.kind.as_deref() {
            Some("fold") => LineKind::Fold {
                level: r.level.unwrap_or(1),
                refs: r.refs.as_ref().map_or(0, Vec::len),
            },
            Some("recall") => LineKind::Recall,
            Some("late") => LineKind::Late,
            Some(other) => LineKind::Other(other.to_string()),
            None => LineKind::Turn {
                assistant: r.role.as_deref() == Some("assistant"),
            },
        }
    };

    let line_for = |r: &Row, depth: u8, closure: Closure, asking: bool, expanded: bool| -> OutlineLine {
        let content = r.content.as_deref().unwrap_or("");
        let v = vouch.get(&r.id);
        OutlineLine {
            id: r.id,
            agent: r.agent.clone(),
            depth,
            kind: kind_of(r),
            // A monitor's delivery reads as the lines it brought; its header is
            // the line's tag (`LineKind::Monitor`), not its text.
            head: match tags::monitor(content) {
                Some(m) => tags::head_of(&content[m.end..]),
                None => tags::head_of(content),
            },
            body: match tags::monitor(content) {
                Some(m) => content[m.end..].trim().to_string(),
                None => tags::body_of(content),
            },
            reply: tags::is_reply(content),
            mark: mark_of(r),
            closure,
            asking,
            // A clear is expandable: opening it is how the rows behind come back.
            // One child is not a nesting (see 8-9), so only a BRANCH opens.
            expandable: kids.get(&r.id).is_some_and(|k| k.len() > 1)
                || r.kind_is("fold")
                || r.kind_is("clear"),
            expanded,
            pinned: a.st.pins.contains(&r.id),
            covered_by: hidden_by.get(&r.id).copied(),
            from: v.map(|v| v.from.clone()),
            msg: v.and_then(|v| v.msg),
            re: v.and_then(|v| v.re),
            bytes: r.bytes(),
        }
    };

    // ── 8-9. Emit. Folded by default: a CLOSED root is one line unless the
    // human opened or pinned it. Anything that is still running stays open,
    // because that is the work in front of you.
    let roots: Vec<&Row> = visible
        .iter()
        .copied()
        .filter(|r| !parent_of.contains_key(&r.id))
        .filter(|r| match a.st.narrow {
            Some(n) => a.store.root_of(r.id) == a.store.root_of(n) || r.id == n,
            None => true,
        })
        .collect();

    let by_id: HashMap<RowId, &Row> = visible.iter().map(|r| (r.id, *r)).collect();
    let mut lines = Vec::with_capacity(visible.len());
    for root in roots {
        let closure = closure_of(root.id);
        let open = a.st.pins.contains(&root.id)
            || a.st.open.contains(&root.id)
            || closure != Closure::Closed;
        lines.push(line_for(root, 0, closure, asking_of(root.id), open));
        if !open {
            continue;
        }
        // Depth 1 comes with an open root; deeper only where the human asked.
        let mut stack: Vec<(RowId, u8)> = kids
            .get(&root.id)
            .map(|k| k.iter().rev().map(|id| (*id, 1u8)).collect())
            .unwrap_or_default();
        // INDENT ONLY WHERE THE THREAD BRANCHES. A turn is a causal chain —
        // step → its tool reply → the next step → … — and drawing every parent
        // link as one more level turned a ten-step turn into a ten-level
        // staircase. So a node with exactly ONE child keeps that child at its
        // own depth and always shows it: a linear run reads flat. Only a node
        // with several children (a batch, a reply to an older row) nests them
        // one level deeper, and only when opened.
        while let Some((id, depth)) = stack.pop() {
            let Some(r) = by_id.get(&id) else { continue };
            let cs = kids.get(&id).map(Vec::as_slice).unwrap_or(&[]);
            let deeper = cs.len() > 1 && (a.st.pins.contains(&id) || a.st.open.contains(&id));
            lines.push(line_for(r, depth, closure, waiting_ids.contains(&id), deeper));
            if cs.len() == 1 {
                stack.push((cs[0], depth));
            } else if deeper {
                for c in cs.iter().rev() {
                    stack.push((*c, depth.saturating_add(1)));
                }
            }
        }
    }

    // ── 12. Zoom: three filters over the same lines, never three renderers.
    let keep = |l: &OutlineLine| -> bool {
        match a.st.zoom {
            Zoom::All => true,
            Zoom::Decisions => {
                l.depth == 0
                    || matches!(l.kind, LineKind::Waiting | LineKind::Fold { .. })
                    || l.closure == Closure::Failed
            }
            Zoom::Roots => l.depth == 0,
        }
    };
    lines.retain(keep);
    // Strict model view: hide exactly what the model cannot see. The tail —
    // rows appended since the projection — stays, because it is in the prompt.
    if a.st.model_view_only {
        lines.retain(|l| l.mark != Mark::NotInView);
    }

    let _ = a.seen;
    Outline {
        lines,
        view: view.map(|v| ViewHeader {
            rows: v.order.len(),
            budget: v.budget,
            why: v.why.clone(),
            ts: v.ts,
        }),
    }
}

/// Keep the cursor on a row that still exists. After a refold the row under it
/// may have been swallowed by a fold; the cursor lifts to whatever is visible
/// rather than jumping to an unrelated line.
pub fn repair_cursor(o: &Outline, want: Option<RowId>) -> Option<RowId> {
    let want = want?;
    if o.position(want).is_some() {
        return Some(want);
    }
    o.lines.first().map(|l| l.id)
}

/// Drop `open` entries a new fold has swallowed. A PIN survives — that is the
/// whole of its meaning.
pub fn reconcile_open(st: &mut OutlineState, covered: &HashSet<RowId>) {
    st.open
        .retain(|id| !covered.contains(id) || st.pins.contains(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: serde_json::Value) -> Row {
        serde_json::from_value(v).unwrap()
    }
    fn ai() -> AgentId {
        "ai".to_string()
    }
    fn store_of(rows: Vec<serde_json::Value>) -> Store {
        let mut s = Store::default();
        s.apply_full(&ai(), rows.into_iter().map(row).collect());
        s
    }
    fn run<'a>(
        store: &'a Store,
        st: &'a OutlineState,
        tasks: &'a [Task],
        waiting: &'a [Waiting],
    ) -> Outline {
        let seen = HashSet::new();
        outline(Args {
            store,
            agent: &ai(),
            tasks,
            waiting,
            seen: &seen,
            st,
        })
    }
    fn plain(store: &Store) -> Outline {
        let st = OutlineState::default();
        run(store, &st, &[], &[])
    }
    fn ids(o: &Outline) -> Vec<RowId> {
        o.lines.iter().map(|l| l.id).collect()
    }

    /// A LONG LOG, built here rather than recorded: `turns` human turns on
    /// `ai_b`, each a burst of calls, tasks, tool replies and assistant steps
    /// hanging off its root — a long first turn, short later ones. With `folds`, the first turn's steps are covered
    /// by four level-1 folds, one level-2 fold over those four, a view row and
    /// a recall — the shapes a real compaction writes.
    fn long_log(turns: usize, folds: bool) -> Store {
        let mut rows = Vec::new();
        let mut id = 0i64;
        let mut next = || {
            id += 1;
            id
        };
        let mut first_steps = Vec::new();
        let mut first_root = 0;
        for t in 0..turns {
            let root = next();
            if t == 0 {
                first_root = root;
            }
            rows.push(json!({"id":root,"agent":"ai_b","role":"user","ts":root,
                             "content":format!("[from user · msg {root}]\nturn {t}")}));
            // The first turn is the long one a compaction folds; later ones are short.
            for k in 0..(if t == 0 { 30 } else { 2 }) {
                let call = next();
                let task = next();
                let reply = next();
                let step = next();
                rows.push(json!({"id":call,"agent":"ai_b","kind":"call","verb":"read_stream","parent":root,"ts":call}));
                rows.push(json!({"id":task,"agent":"ai_b","kind":"task","verb":"read_stream","parent":call,"ts":task}));
                rows.push(json!({"id":reply,"agent":"ai_b","role":"user","parent":root,"ts":reply,
                                 "content":format!("[from files · re {call}]\nchunk {k}")}));
                rows.push(json!({"id":step,"agent":"ai_b","role":"assistant","parent":root,"ts":step,
                                 "content":format!("step {k} of turn {t}")}));
                if t == 0 {
                    first_steps.extend([call, task, reply, step]);
                }
            }
        }
        if folds {
            let mut l1 = Vec::new();
            for chunk in first_steps.chunks(first_steps.len().div_ceil(4)) {
                let f = next();
                rows.push(json!({"id":f,"agent":"ai_b","role":"user","kind":"fold","level":1,
                                 "parent":first_root,"refs":chunk,"title":format!("fold {f}"),"ts":f,
                                 "content":format!("[fold {f} · {} refs · L1]", chunk.len())}));
                l1.push(f);
            }
            let f2 = next();
            rows.push(json!({"id":f2,"agent":"ai_b","role":"user","kind":"fold","level":2,
                             "parent":first_root,"refs":l1,"title":"the whole search","ts":f2,
                             "content":format!("[fold {f2} · 4 refs · L2]")}));
            let v = next();
            rows.push(json!({"id":v,"agent":"ai_b","kind":"view","v":1,"budget":14976,"why":"projection: fold",
                             "ids":[{"id":first_root,"res":"full"},{"id":f2,"res":"full"}],"ts":v}));
            let r = next();
            rows.push(json!({"id":r,"agent":"ai_b","role":"user","kind":"recall","ts":r,
                             "title":"the turn-0 instruction","content":format!("[recall {r} · kept 2 rows]")}));
        }
        let mut s = Store::default();
        s.apply_full(&"ai_b".to_string(), rows.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect());
        s
    }
    fn fixture_outline(s: &Store, agent: &str, st: &OutlineState) -> Outline {
        let seen = HashSet::new();
        let a = agent.to_string();
        outline(Args {
            store: s,
            agent: &a,
            tasks: &[],
            waiting: &[],
            seen: &seen,
            st,
        })
    }

    // ── structure ────────────────────────────────────────────────────────

    #[test]
    fn roots_are_parentless() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"do it"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"ok"}),
            json!({"id":3,"agent":"ai","role":"user","content":"and this"}),
        ]);
        let o = plain(&s);
        let roots: Vec<_> = o
            .lines
            .iter()
            .filter(|l| l.depth == 0)
            .map(|l| l.id)
            .collect();
        assert_eq!(roots, vec![1, 3]);
    }

    #[test]
    fn notices_are_hidden_and_the_reply_after_one_keeps_its_thread() {
        // a call-less answer (3), the layer's reminder to the model (4, a
        // notice), and the answer that followed it (5, parented on the notice)
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"do it"}),
            json!({"id":3,"agent":"ai","role":"assistant","parent":1,"content":"thinking out loud"}),
            json!({"id":4,"agent":"ai","role":"user","kind":"notice","parent":1,
                   "content":"[no call was dispatched] Your answer emitted no <call> element"}),
            json!({"id":5,"agent":"ai","role":"assistant","parent":4,"content":"done"}),
        ]);
        let o = plain(&s);
        assert_eq!(ids(&o), vec![1, 3, 5], "the notice is not drawn");
        assert_eq!(o.lines[2].depth, 1, "the reply after the notice hangs off the turn, not off the hidden row");
    }

    #[test]
    fn a_monitor_delivery_is_not_a_question_and_reads_as_its_lines() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user",
                   "content":"[from runner]\n[monitor 787 · job 514 · from your call 783 · \"w5\"]\nFATAL w5: loss diverged"}),
            json!({"id":2,"agent":"ai","role":"user",
                   "content":"[from solver]\nwhich port?"}),
        ]);
        let w = waiting_for(&s, &ai(), crate::env::HUMAN);
        assert_eq!(w.iter().map(|w| w.row).collect::<Vec<_>>(), vec![2], "a delivery asks nothing");
        let o = plain(&s);
        let m = &o.lines[o.position(1).unwrap()];
        assert_eq!(
            m.kind,
            LineKind::Monitor { id: 787, job: 514, label: Some("w5".into()), ended: false }
        );
        assert_eq!(m.body, "FATAL w5: loss diverged");
        assert_eq!(m.head, "FATAL w5: loss diverged");
        assert_eq!(m.from.as_deref(), Some("runner"));
    }

    #[test]
    fn children_group_by_row_parent() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"root"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"a"}),
            json!({"id":3,"agent":"ai","role":"assistant","parent":1,"content":"b"}),
        ]);
        let o = plain(&s);
        assert_eq!(ids(&o), vec![1, 2, 3]);
        assert_eq!(o.lines[1].depth, 1);
        assert_eq!(o.lines[2].depth, 1);
    }

    #[test]
    fn re_link_makes_a_child() {
        // No row parent at all: the link lives in the vouched header.
        let s = store_of(vec![
            json!({"id":10,"agent":"ai","role":"user","content":"[from solver_1 · msg 10]\nquestion"}),
            json!({"id":11,"agent":"ai","role":"user","content":"[from solver_2 · re 10]\nanswer"}),
        ]);
        let o = plain(&s);
        assert_eq!(o.lines.iter().filter(|l| l.depth == 0).count(), 1);
        assert_eq!(o.lines[1].id, 11);
        assert_eq!(o.lines[1].depth, 1);
        assert_eq!(o.lines[1].re, Some(10));
    }

    #[test]
    fn up_arrow_link_makes_a_child() {
        let s = store_of(vec![
            json!({"id":20,"agent":"ai","role":"user","content":"task"}),
            json!({"id":21,"agent":"ai","role":"user","content":"[reply to 99 · notes read · \"f0\" · ↑20]\nbody"}),
        ]);
        let o = plain(&s);
        assert_eq!(
            o.lines[1].depth, 1,
            "the ↑ turn hangs off the turn it answers"
        );
    }

    // ── folds ────────────────────────────────────────────────────────────

    #[test]
    fn cover_is_the_flat_union_of_every_folds_refs() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":2,"agent":"ai","content":"b"}),
            json!({"id":3,"agent":"ai","content":"c"}),
            json!({"id":4,"agent":"ai","kind":"fold","level":1,"refs":[1,2],"title":"f1",
                   "content":"[fold 4 · 2 refs · L1] \"f1\""}),
        ]);
        let o = plain(&s);
        assert_eq!(
            ids(&o),
            vec![4, 3],
            "the fold takes the slot of its earliest member"
        );
    }

    #[test]
    fn level2_fold_hides_level1_and_its_members() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":2,"agent":"ai","content":"b"}),
            json!({"id":3,"agent":"ai","kind":"fold","level":1,"refs":[1],"title":"f1",
                   "content":"[fold 3 · 1 refs · L1] \"f1\""}),
            json!({"id":4,"agent":"ai","kind":"fold","level":1,"refs":[2],"title":"f2",
                   "content":"[fold 4 · 1 refs · L1] \"f2\""}),
            json!({"id":5,"agent":"ai","kind":"fold","level":2,"refs":[3,4],"title":"F",
                   "content":"[fold 5 · 2 refs · L2] \"F\""}),
        ]);
        let o = plain(&s);
        assert_eq!(ids(&o), vec![5], "one top line: the level-2 fold");
        assert!(matches!(
            o.lines[0].kind,
            LineKind::Fold { level: 2, refs: 2 }
        ));
    }

    #[test]
    fn fold_places_at_min_index_of_its_direct_refs() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"first"}),
            json!({"id":2,"agent":"ai","content":"second"}),
            json!({"id":3,"agent":"ai","content":"third"}),
            json!({"id":4,"agent":"ai","kind":"fold","level":1,"refs":[3],"title":"late",
                   "content":"[fold 4 · 1 refs · L1] \"late\""}),
        ]);
        let o = plain(&s);
        assert_eq!(
            ids(&o),
            vec![1, 2, 4],
            "it sits where its earliest member sat, not at the end"
        );
    }

    #[test]
    fn unplaceable_fold_is_emitted_nowhere() {
        // Every ref is unknown, so there is no position to claim. The kernel
        // drops such a fold from its own render; so do we.
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":9,"agent":"ai","kind":"fold","level":1,"refs":[404],"title":"ghost",
                   "content":"[fold 9 · 1 refs · L1] \"ghost\""}),
        ]);
        let o = plain(&s);
        assert_eq!(ids(&o), vec![1]);
    }

    // ── expansion, pins ──────────────────────────────────────────────────

    fn closed_root() -> Store {
        let mut s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"task"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"step"}),
        ]);
        // A settle row on the root agent closes it.
        s.apply_full(
            &"kernel_state".to_string(),
            vec![row(
                json!({"id":90,"agent":"kernel_state","kind":"settle","parent":1,"ts":9.0,
                            "outcome":{"status":"completed"}}),
            )],
        );
        s
    }

    #[test]
    fn closed_roots_fold_to_one_line() {
        let s = closed_root();
        let o = plain(&s);
        assert_eq!(
            ids(&o),
            vec![1],
            "a finished root is one line until you open it"
        );
        assert_eq!(o.lines[0].closure, Closure::Closed);
        assert!(!o.lines[0].expanded);
    }

    #[test]
    fn opening_a_closed_root_shows_depth_one() {
        let s = closed_root();
        let mut st = OutlineState::default();
        st.open.insert(1);
        let o = run(&s, &st, &[], &[]);
        assert_eq!(ids(&o), vec![1, 2]);
    }

    #[test]
    fn running_root_opens_to_depth_one() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"task"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"working"}),
        ]);
        let tasks = vec![Task {
            id: 2,
            agent: ai(),
            parent: None,
            verb: "send".into(),
            started_at: 0.0,
        }];
        let st = OutlineState::default();
        let o = run(&s, &st, &tasks, &[]);
        assert_eq!(o.lines[0].closure, Closure::Running);
        assert_eq!(ids(&o), vec![1, 2], "live work is never hidden from you");
    }

    #[test]
    fn pin_survives_a_new_fold_and_open_does_not() {
        let mut st = OutlineState::default();
        st.open.insert(1);
        st.open.insert(2);
        st.pins.insert(2);
        let covered: HashSet<RowId> = [1, 2].into_iter().collect();
        reconcile_open(&mut st, &covered);
        assert!(
            !st.open.contains(&1),
            "an unpinned open row is swallowed by the fold"
        );
        assert!(
            st.open.contains(&2),
            "a pinned row stays open across a refold"
        );
    }

    #[test]
    fn cursor_lifts_when_its_row_disappears() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":2,"agent":"ai","kind":"fold","level":1,"refs":[1],"title":"f",
                   "content":"[fold 2 · 1 refs · L1] \"f\""}),
        ]);
        let o = plain(&s);
        assert_eq!(
            repair_cursor(&o, Some(1)),
            Some(2),
            "the cursor rises to what covers it"
        );
        assert_eq!(repair_cursor(&o, Some(2)), Some(2));
    }

    // ── the gutter ───────────────────────────────────────────────────────

    #[test]
    fn gutter_comes_only_from_the_view_row() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":2,"agent":"ai","content":"b"}),
            json!({"id":3,"agent":"ai","kind":"view","v":1,"budget":100,"why":"projection: fold",
                   "ids":[{"id":1,"res":"stub"},{"id":2,"res":"full"}]}),
        ]);
        let o = plain(&s);
        assert_eq!(o.lines[0].mark, Mark::Stub);
        assert_eq!(o.lines[1].mark, Mark::Full);
        let h = o.view.unwrap();
        assert_eq!(
            (h.rows, h.budget, h.why.as_str()),
            (2, 100, "projection: fold")
        );
    }

    #[test]
    fn rows_behind_the_view_head_are_full_not_dot() {
        // THE common case: the projection was written, then work continued.
        // Those rows ride in the prompt whole; calling them invisible would be
        // the biggest lie the gutter could tell.
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":2,"agent":"ai","kind":"view","v":1,"ids":[{"id":1,"res":"full"}]}),
            json!({"id":3,"agent":"ai","content":"appended after the projection"}),
        ]);
        let o = plain(&s);
        assert_eq!(o.lines.iter().find(|l| l.id == 3).unwrap().mark, Mark::Full);
    }

    #[test]
    fn a_row_before_the_head_that_the_view_omits_is_not_in_view() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"dropped from the prompt"}),
            json!({"id":2,"agent":"ai","content":"kept"}),
            json!({"id":3,"agent":"ai","kind":"view","v":1,"ids":[{"id":2,"res":"full"}]}),
        ]);
        let o = plain(&s);
        assert_eq!(o.lines[0].mark, Mark::NotInView);
    }

    #[test]
    fn an_unknown_res_is_shown_not_guessed() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":2,"agent":"ai","kind":"view","v":1,"ids":[{"id":1,"res":"hologram"}]}),
        ]);
        assert_eq!(plain(&s).lines[0].mark, Mark::Unknown);
    }

    #[test]
    fn model_view_hides_dots_and_keeps_the_tail() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"dropped"}),
            json!({"id":2,"agent":"ai","content":"kept"}),
            json!({"id":3,"agent":"ai","kind":"view","v":1,"ids":[{"id":2,"res":"full"}]}),
            json!({"id":4,"agent":"ai","content":"tail"}),
        ]);
        let st = OutlineState {
            model_view_only: true,
            ..Default::default()
        };
        let o = run(&s, &st, &[], &[]);
        let got = ids(&o);
        assert!(!got.contains(&1), "the row the model cannot see is hidden");
        assert!(
            got.contains(&2) && got.contains(&4),
            "the tail is in the prompt and stays"
        );
    }

    #[test]
    fn no_view_row_means_everything_is_full() {
        let s = store_of(vec![json!({"id":1,"agent":"ai","content":"a"})]);
        let o = plain(&s);
        assert_eq!(o.lines[0].mark, Mark::Full);
        assert!(o.view.is_none());
    }

    // ── closure, waiting, zoom ───────────────────────────────────────────

    #[test]
    fn failure_is_a_status_not_prose() {
        let mut s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"task"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,
                   "content":"everything is fine, no errors at all"}),
        ]);
        s.apply_full(
            &"kernel_state".to_string(),
            vec![row(
                json!({"id":90,"agent":"kernel_state","kind":"settle","parent":2,"ts":9.0,
                            "outcome":{"status":"errored","error":"no such path"}}),
            )],
        );
        let mut st = OutlineState::default();
        st.open.insert(1);
        assert_eq!(run(&s, &st, &[], &[]).lines[0].closure, Closure::Failed);
    }

    #[test]
    fn a_clear_restarts_the_thread_and_opening_it_brings_the_rows_back() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"old question"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"old answer"}),
            json!({"id":3,"agent":"ai","kind":"clear","content":"[cleared 3 · 2 rows behind] fresh start"}),
            json!({"id":4,"agent":"ai","role":"user","content":"new question"}),
        ]);
        let closed = run(&s, &OutlineState::default(), &[], &[]);
        let heads: Vec<&str> = closed.lines.iter().map(|l| l.head.as_str()).collect();
        assert!(heads.iter().all(|h| !h.contains("old")), "rows behind the clear are not drawn: {heads:?}");
        assert!(heads[0].contains("cleared 3"), "the clear row starts the thread: {heads:?}");
        assert!(heads.iter().any(|h| h.contains("new question")));
        assert!(closed.lines[0].expandable, "the clear is the handle to what is behind it");

        let mut st = OutlineState::default();
        st.open.insert(3);
        let opened = run(&s, &st, &[], &[]);
        assert!(opened.lines.iter().any(|l| l.head.contains("old question")), "opening the clear brings them back");
    }

    #[test]
    fn only_the_newest_clear_is_the_boundary() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"a"}),
            json!({"id":2,"agent":"ai","kind":"clear","content":"[cleared 2 · 1 rows behind]"}),
            json!({"id":3,"agent":"ai","content":"b"}),
            json!({"id":4,"agent":"ai","kind":"clear","content":"[cleared 4 · 3 rows behind]"}),
            json!({"id":5,"agent":"ai","content":"c"}),
        ]);
        let o = run(&s, &OutlineState::default(), &[], &[]);
        let heads: Vec<&str> = o.lines.iter().map(|l| l.head.as_str()).collect();
        assert_eq!(heads.len(), 2, "{heads:?}");
        assert!(heads[0].contains("cleared 4") && heads[1] == "c", "{heads:?}");
    }

    #[test]
    fn waiting_is_derived_and_marks_the_root_asking_without_blocking_it() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"[from other · msg 1]\nwhich port?"}),
        ]);
        let w = waiting_for(&s, &ai(), crate::env::HUMAN);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].from, "other");
        assert_eq!(w[0].head, "which port?");
        let o = run(&s, &OutlineState::default(), &[], &w);
        assert!(o.lines[0].asking, "an open question is a badge on its thread");
        assert_ne!(o.lines[0].closure, Closure::Running, "and it is not work in flight");
        assert_eq!(o.lines[0].kind, LineKind::Waiting);
    }

    #[test]
    fn the_humans_own_message_is_not_a_question_to_the_human() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"[from user · to ai]\nscan it"}),
        ]);
        assert!(waiting_for(&s, &ai(), crate::env::HUMAN).is_empty());
    }

    #[test]
    fn an_answered_message_is_no_longer_waiting() {
        let mut s = store_of(vec![
            json!({"id":1,"agent":"ai","content":"[from other · msg 1]\nwhich port?"}),
        ]);
        // The answer lives in the ASKER's conversation and points back with `re`.
        s.apply_full(
            &"other".to_string(),
            vec![row(
                json!({"id":2,"agent":"other","content":"[from ai · re 1]\n8080"}),
            )],
        );
        assert!(waiting_for(&s, &ai(), crate::env::HUMAN).is_empty());
    }

    #[test]
    fn zoom_decisions_keeps_roots_folds_and_failures() {
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"root"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"a step"}),
            json!({"id":3,"agent":"ai","content":"b"}),
            json!({"id":4,"agent":"ai","kind":"fold","level":1,"refs":[3],"title":"f",
                   "content":"[fold 4 · 1 refs · L1] \"f\""}),
        ]);
        let mut st = OutlineState {
            zoom: Zoom::Decisions,
            ..Default::default()
        };
        let o = run(&s, &st, &[], &[]);
        let got = ids(&o);
        assert!(got.contains(&1) && got.contains(&4));
        assert!(!got.contains(&2), "an ordinary step is not a decision");

        st.zoom = Zoom::Roots;
        assert!(run(&s, &st, &[], &[]).lines.iter().all(|l| l.depth == 0));
    }

    #[test]
    fn a_linear_chain_stays_flat_and_only_a_branch_indents() {
        // root → step → reply → step → a batch of two replies → …
        let s = store_of(vec![
            json!({"id":1,"agent":"ai","role":"user","content":"scan it"}),
            json!({"id":2,"agent":"ai","role":"assistant","parent":1,"content":"step a"}),
            json!({"id":3,"agent":"ai","role":"user","parent":2,"content":"reply a"}),
            json!({"id":4,"agent":"ai","role":"assistant","parent":3,"content":"step b"}),
            json!({"id":5,"agent":"ai","role":"user","parent":4,"content":"reply b1"}),
            json!({"id":6,"agent":"ai","role":"user","parent":4,"content":"reply b2"}),
        ]);
        let mut st = OutlineState::default();
        st.open.insert(1);
        let o = run(&s, &st, &[], &[]);
        let at: Vec<(RowId, u8)> = o.lines.iter().map(|l| (l.id, l.depth)).collect();
        assert_eq!(at, vec![(1, 0), (2, 1), (3, 1), (4, 1)], "a chain is flat; the branch is closed");
        assert!(!o.lines[1].expandable, "one child is not a nesting");
        assert!(o.lines[3].expandable, "two children are a branch");
        st.open.insert(4);
        let o = run(&s, &st, &[], &[]);
        let at: Vec<(RowId, u8)> = o.lines.iter().map(|l| (l.id, l.depth)).collect();
        assert_eq!(at, vec![(1, 0), (2, 1), (3, 1), (4, 1), (5, 2), (6, 2)], "opened, the branch nests once");
    }

    // ── determinism + a long log ───────────────────────────────────────

    #[test]
    fn outline_is_deterministic() {
        let s = long_log(6, true);
        let st = OutlineState::default();
        let first = fixture_outline(&s, "ai_b", &st);
        for _ in 0..20 {
            assert_eq!(fixture_outline(&s, "ai_b", &st), first);
        }
    }

    #[test]
    fn a_long_log_collapses_to_its_live_folds() {
        // 173 rows on ai_b: four level-1 folds and one level-2 fold over them.
        // The level-2 fold must swallow the other four and everything they hold.
        let s = long_log(6, true);
        // The root is one live turn with everything hanging off it, so open it:
        // folded-by-default is the point, and this test is about what is inside.
        let mut st = OutlineState::default();
        let root = fixture_outline(&s, "ai_b", &st).lines[0].id;
        st.open.insert(root);
        let o = fixture_outline(&s, "ai_b", &st);
        let folds: Vec<_> = o
            .lines
            .iter()
            .filter_map(|l| match l.kind {
                LineKind::Fold { level, .. } => Some((l.id, level)),
                _ => None,
            })
            .collect();
        assert_eq!(
            folds.len(),
            1,
            "one live fold survives, not five: {folds:?}"
        );
        assert_eq!(folds[0].1, 2, "and it is the level-2 one");
        assert!(
            o.lines.len() < 60,
            "a long log read as {} lines",
            o.lines.len()
        );
        assert!(
            o.view.is_some(),
            "the agent wrote a view row, so the gutter is live"
        );
    }

    #[test]
    fn a_long_log_has_one_root_per_human_turn() {
        let s = long_log(6, true);
        let o = fixture_outline(&s, "ai_b", &OutlineState::default());
        let roots = o.lines.iter().filter(|l| l.depth == 0).count();
        assert!((1..20).contains(&roots), "roots: {roots}");
    }

    #[test]
    fn a_log_without_folds_still_outlines() {
        let s = long_log(6, false);
        let o = fixture_outline(&s, "ai_b", &OutlineState::default());
        assert!(!o.lines.is_empty());
        assert!(o
            .lines
            .iter()
            .all(|l| !matches!(l.kind, LineKind::Fold { .. })));
    }
}
