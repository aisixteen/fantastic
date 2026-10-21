//! A fold carries its own place (`anchor`): it stays in the view when its members are
//! not in the rows being rendered (a body rebuilt from the view array, a store
//! compacted by `context_forget`), and a store written before the field existed
//! renders as it did. The twin of python `ai_core/tests/test_fold_anchors.py`.
//!
//! A child of `tests`, so the kernel half drives the same harness: a real `ram_store`
//! named `memory` under the root, every backend wired to it both ways.

use super::*;
use crate::folds::{
    anchors_of, covered, expand_ids, is_fold, make_fold, member_anchor, refs_of, render, row_id,
    unanchored,
};
use fantastic_kernel::test_support::as_agent;
use std::collections::{HashMap, HashSet};

const DONE: &str = "<call name=\"done\"></call>";
const CALL: &str = "<call name=\"send\"><target_id>notes</target_id><type>read</type></call>";
const DUMP: &str = include_str!("../../../../../../../contract/ai_core/dump_before_anchors.json");

/// `render` as it was before anchors: each fold at the position of the first of its
/// refs present in `rows`. The oracle the new render must match while every member
/// is present.
fn old_render(rows: &[Value]) -> Vec<Value> {
    let cov = covered(rows);
    let mut first_pos: HashMap<i64, usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(id) = row_id(r).filter(|id| cov.contains(id)) {
            first_pos.entry(id).or_insert(i);
        }
    }
    let mut place: HashMap<usize, Vec<&Value>> = HashMap::new();
    for r in rows.iter().filter(|r| is_fold(r)) {
        let at = refs_of(r)
            .iter()
            .filter_map(|x| first_pos.get(x))
            .min()
            .copied();
        if let Some(at) = at {
            if !row_id(r).is_some_and(|id| cov.contains(&id)) {
                place.entry(at).or_default().push(r);
            }
        }
    }
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        for f in place.get(&i).into_iter().flatten() {
            out.push((*f).clone());
        }
        if row_id(r).is_some_and(|id| cov.contains(&id)) || is_fold(r) {
            continue;
        }
        out.push(r.clone());
    }
    out
}

fn new_render(rows: &[Value]) -> Vec<Value> {
    render(rows, &anchors_of(rows))
}

fn ids(rows: &[Value]) -> Vec<Option<i64>> {
    rows.iter().map(row_id).collect()
}

/// What the store's `prune` does: the rows go, and each leaves every fold's refs.
fn cascade(rows: &[Value], gone: &HashSet<i64>) -> Vec<Value> {
    rows.iter()
        .filter(|r| !row_id(r).is_some_and(|id| gone.contains(&id)))
        .map(|r| {
            let mut r = r.clone();
            if is_fold(&r) {
                let left: Vec<i64> = refs_of(&r)
                    .into_iter()
                    .filter(|m| !gone.contains(m))
                    .collect();
                r["refs"] = json!(left);
            }
            r
        })
        .collect()
}

/// Ids in arrival order, and each fold's anchor as the strategy writes it.
struct Ids {
    next: i64,
    anchors: HashMap<i64, i64>,
    levels: HashMap<i64, i64>,
}

impl Ids {
    fn new() -> Self {
        Self {
            next: 1,
            anchors: HashMap::new(),
            levels: HashMap::new(),
        }
    }

    fn mint(&mut self) -> i64 {
        self.next += 1;
        self.next - 1
    }

    fn row(&mut self, role: &str, content: &str, parent: Option<i64>) -> Value {
        json!({"role": role, "id": self.mint(), "parent": parent, "content": content})
    }

    /// Closed chains, their rows interleaved round-robin, ids minted in arrival order.
    fn chains(&mut self, tags: &[&str]) -> (Vec<Value>, Vec<Vec<Value>>) {
        let mut per: Vec<Vec<Value>> = vec![Vec::new(); tags.len()];
        let mut all = Vec::new();
        for step in 0..4 {
            for (c, tag) in tags.iter().enumerate() {
                let parent = per[c].last().and_then(row_id);
                let (role, content) = match step {
                    0 => ("user", format!("task {tag}")),
                    1 => ("assistant", format!("{tag} {CALL}")),
                    2 => (
                        "user",
                        format!("[reply to 9 · notes read · \"{tag}\"]\n{tag} body"),
                    ),
                    _ => ("assistant", format!("ok {DONE}")),
                };
                let r = self.row(role, &content, parent);
                per[c].push(r.clone());
                all.push(r);
            }
        }
        (all, per)
    }

    fn fold(&mut self, refs: &[i64], title: &str) -> Value {
        let anchor = refs
            .iter()
            .map(|x| self.anchors.get(x).copied().unwrap_or(*x))
            .min()
            .expect("a fold has members");
        let lvl = 1 + refs
            .iter()
            .filter_map(|x| self.levels.get(x))
            .max()
            .copied()
            .unwrap_or(0);
        let id = self.mint();
        self.anchors.insert(id, anchor);
        self.levels.insert(id, lvl);
        make_fold(refs, title, "b", false, id, anchor, lvl, None)
    }
}

fn idv(rows: &[Value]) -> Vec<i64> {
    rows.iter().filter_map(row_id).collect()
}

fn without_anchor(mut f: Value) -> Value {
    f.as_object_mut().unwrap().remove("anchor");
    f
}

// ── the fold fixtures, members present ──────────────────────────────

fn fixtures() -> Vec<(&'static str, Vec<Value>)> {
    let mut out = Vec::new();
    let mut k = Ids::new();
    let (mut rows, _) = k.chains(&["a", "b"]);
    rows.push(k.row("user", "later", None));
    let f = k.fold(&idv(&rows[..5]), "a");
    rows.push(f);
    out.push(("one fold", rows));

    let mut k = Ids::new();
    let (mut rows, _) = k.chains(&["a"]);
    let inner = k.fold(&idv(&rows), "inner");
    let outer = k.fold(&[row_id(&inner).unwrap()], "outer");
    rows.extend([inner, outer]);
    out.push(("fold of folds", rows));

    let mut k = Ids::new();
    let (mut rows, _) = k.chains(&["a", "b"]);
    let part = k.fold(&idv(&rows[..3]), "part");
    let whole = k.fold(&idv(&rows), "whole");
    rows.extend([part, whole]);
    out.push(("overlapping", rows));

    let mut k = Ids::new();
    let (mut rows, _) = k.chains(&["a"]);
    let mut refs = idv(&rows);
    refs.push(9999);
    let f = k.fold(&refs, "done");
    rows.push(f);
    out.push(("a member gone", rows));

    let mut k = Ids::new();
    let (mut rows, _) = k.chains(&["a"]);
    let f = k.fold(&idv(&rows), "done");
    let first = row_id(&rows[0]);
    rows.push(f);
    let late = k.row("user", "[reply to 7 · notes read]\nlate", first);
    rows.push(late);
    out.push(("a late reply", rows));

    let mut k = Ids::new();
    let goal = k.row("user", "GOAL", None);
    let (body, per) = k.chains(&["a", "b", "c", "d", "e", "f"]);
    let mut rows = vec![goal];
    rows.extend(body);
    for c in &per {
        let f = k.fold(&idv(c), "d");
        rows.push(f);
    }
    rows.push(k.row("user", "now?", None));
    out.push(("interleaved chains", rows));

    let mut k = Ids::new();
    let goal = k.row("user", "GOAL", None);
    let (body, per) = k.chains(&["a", "b"]);
    let mut rows = vec![goal];
    rows.extend(body);
    let fa = k.fold(&idv(&per[0]), "a");
    let fb = k.fold(&idv(&per[1]), "b");
    rows.extend([fa, fb]);
    for i in 0..3 {
        rows.push(k.row("user", &format!("t{i}"), None));
    }
    out.push(("two folds and a tail", rows));
    out
}

#[test]
fn render_is_unchanged_when_members_are_present() {
    for (name, rows) in fixtures() {
        assert_eq!(new_render(&rows), old_render(&rows), "{name}");
    }
}

fn dump() -> Value {
    serde_json::from_str(DUMP).expect("the dump is JSON")
}

#[test]
fn render_is_unchanged_for_a_store_written_before_anchors() {
    // The committed dump, made on the code before anchors: every member present.
    let d = dump();
    let refs: HashMap<i64, Value> = d["tables"]["fold_refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["fold"].as_i64().unwrap(), f["members"].clone()))
        .collect();
    let rows: Vec<Value> = d["tables"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut r = t["row"].clone();
            if let Some(m) = row_id(&r).and_then(|id| refs.get(&id)) {
                r["refs"] = m.clone();
            }
            r
        })
        .collect();
    let last = rows.iter().rposition(|r| r["kind"] == "clear").unwrap();
    let head: Vec<Value> = rows[last..]
        .iter()
        .filter(|r| r["kind"] != "view" && r["kind"] != "anchors")
        .cloned()
        .collect();
    assert_eq!(new_render(&head), old_render(&head));
}

// ── a fold without its members ──────────────────────────────────────

#[test]
fn a_fold_without_members_keeps_its_place() {
    let mut k = Ids::new();
    let goal = k.row("user", "GOAL", None);
    let (ab, per) = k.chains(&["a", "b"]);
    let (c, _) = k.chains(&["c"]);
    let fa = k.fold(&idv(&per[0]), "a");
    let fb = k.fold(&idv(&per[1]), "b");
    let fc = k.fold(&idv(&c), "c");
    let l2 = k.fold(&[row_id(&fa).unwrap(), row_id(&fb).unwrap()], "a and b");
    let tail: Vec<Value> = (0..2)
        .map(|i| k.row("user", &format!("t{i}"), None))
        .collect();
    let mut rows = vec![goal.clone()];
    rows.extend(ab.iter().cloned());
    rows.extend(c.iter().cloned());
    rows.extend([fa, fb, fc.clone(), l2.clone()]);
    rows.extend(tail.iter().cloned());
    let before = ids(&new_render(&rows));
    let mut want = vec![row_id(&goal), row_id(&l2), row_id(&fc)];
    want.extend(ids(&tail));
    assert_eq!(before, want);
    // the leaves of a and b go: the L2 keeps its place, ahead of c's fold
    let gone: HashSet<i64> = idv(&ab).into_iter().collect();
    let after = cascade(&rows, &gone);
    assert_eq!(ids(&new_render(&after)), before);
    // c's leaves go too, all but its last: c's fold stays where its first leaf was
    let gone: HashSet<i64> = idv(&c[..c.len() - 1]).into_iter().collect();
    let after = cascade(&after, &gone);
    assert_eq!(ids(&new_render(&after)), before);
    // a fold whose first member is gone, others left: it does not move to a survivor
    let x = k.row("user", "x", None);
    let y = k.row("assistant", "y", None);
    let z = k.row("user", "z", None);
    let fx = k.fold(&[row_id(&x).unwrap(), row_id(&z).unwrap()], "x and z");
    let rows = vec![x.clone(), y.clone(), z, fx.clone()];
    assert_eq!(ids(&new_render(&rows)), vec![row_id(&fx), row_id(&y)]);
    let gone: HashSet<i64> = [row_id(&x).unwrap()].into_iter().collect();
    assert_eq!(
        ids(&new_render(&cascade(&rows, &gone))),
        vec![row_id(&fx), row_id(&y)]
    );
}

#[test]
fn an_unanchored_fold_without_members_is_counted_not_placed() {
    let mut k = Ids::new();
    let (a, _) = k.chains(&["a"]);
    let old = without_anchor(k.fold(&idv(&a), "done")); // written before the field
    let oid = row_id(&old).unwrap();
    let tail = vec![k.row("user", "now", None)];
    let mut all = a.clone();
    all.push(old);
    all.extend(tail.iter().cloned());
    let rows = cascade(&all, &idv(&a).into_iter().collect());
    let anchors = anchors_of(&rows);
    assert!(!anchors.contains_key(&oid));
    assert_eq!(ids(&render(&rows, &anchors)), ids(&tail));
    assert_eq!(unanchored(&rows, &anchors), 1);
    // an `anchors` row recorded for it places it again
    let mut with = rows.clone();
    with.push(json!({"kind": "anchors", "v": 1, "anchors": {oid.to_string(): row_id(&a[0])}}));
    let anchors = anchors_of(&with);
    let mut want = vec![Some(oid)];
    want.extend(ids(&tail));
    assert_eq!(ids(&render(&rows, &anchors)), want);
}

// ── through a real agent and its ram_store ──────────────────────────

/// A closed chain written through the agent: the task, a send, its reply, done.
async fn closed(kernel: &Arc<Kernel>, agent: &AgentId, tag: &str, big: usize) -> Vec<i64> {
    let u = put_row(
        kernel,
        agent,
        json!({"role": "user", "parent": null, "content": format!("task {tag}")}),
    )
    .await;
    let a = put_row(
        kernel,
        agent,
        json!({"role": "assistant", "parent": u, "content": format!("{tag} {CALL}")}),
    )
    .await;
    let pid = kernel.mint_id();
    // DISTINCT bodies: identical ones are stubbed by the repeat-body dedupe
    let words: Vec<String> = (0..big / 4).map(|i| format!("{tag}{i}")).collect();
    let r = put_row(
        kernel,
        agent,
        json!({"role": "user", "parent": a,
               "content": format!("[reply to {pid} · notes read · \"{tag}\"]\n{}", words.join(" "))}),
    )
    .await;
    let d = put_row(
        kernel,
        agent,
        json!({"role": "assistant", "parent": r, "content": format!("ok {DONE}")}),
    )
    .await;
    vec![u, a, r, d]
}

async fn call(kernel: &Arc<Kernel>, target: &AgentId, payload: Value) -> Value {
    let out = kernel
        .send(vec![Item::new(target.clone(), payload)])
        .await
        .remove(0);
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    out.result
}

async fn set(kernel: &Arc<Kernel>, agent: &AgentId, fields: Value) {
    let mut p = json!({"type": "update_agent", "id": agent.as_str()});
    for (k, v) in fields.as_object().unwrap() {
        p[k] = v.clone();
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), p)])
        .await;
}

/// Every row of the agent in its store, bookkeeping included.
async fn store(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

/// The next prompt as the model reads it.
async fn prompt(kernel: &Arc<Kernel>, agent: &AgentId) -> String {
    let state = crate::state::state_for(agent);
    let (msgs, _) = as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            agent,
            &state,
            "next",
            kernel,
            "c",
            Some("sys"),
            crate::caching::Caching::Cleanup,
        ),
    )
    .await
    .expect("assemble");
    Value::Array(crate::agent_loop::render_for_model(&msgs)).to_string()
}

/// One projection, as a turn runs it: the message list, then the strategy over it.
async fn project(kernel: &Arc<Kernel>, agent: &AgentId) {
    script(
        agent,
        (0..40)
            .map(|_| pass("the work, folded\n\nwhat was read and how it ended."))
            .collect(),
        Duration::ZERO,
    );
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        script: script_for(agent),
    });
    let state = crate::state::state_for(agent);
    let (messages, store_len) = as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            agent,
            &state,
            "next",
            kernel,
            "cli",
            Some("sys"),
            crate::caching::Caching::Cleanup,
        ),
    )
    .await
    .expect("store read");
    as_agent(
        agent.clone(),
        crate::projection::project_context(
            &provider,
            agent,
            &state,
            kernel,
            "cli",
            "mock",
            crate::constants::DEFAULT_CONTEXT_WINDOW,
            messages,
            store_len,
        ),
    )
    .await
    .expect("projection");
}

fn views(rows: &[Value]) -> Vec<&Value> {
    rows.iter().filter(|r| r["kind"] == "view").collect()
}

/// Every fold named in `ids`, and every fold under one of them.
fn reaches(rows: &[Value], start: &[i64]) -> HashSet<i64> {
    let by_id: HashMap<i64, &Value> = rows
        .iter()
        .filter_map(|r| row_id(r).map(|i| (i, r)))
        .collect();
    let mut out = HashSet::new();
    let mut stack = start.to_vec();
    while let Some(x) = stack.pop() {
        let Some(r) = by_id.get(&x) else { continue };
        if !is_fold(r) || !out.insert(x) {
            continue;
        }
        stack.extend(refs_of(r));
    }
    out
}

fn fold_ids(rows: &[Value]) -> Vec<i64> {
    rows.iter()
        .filter(|r| is_fold(r))
        .filter_map(row_id)
        .collect()
}

/// A backend on the fold strategy over a small window, with a goal and six closed
/// chains, projected once.
async fn folded_once(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let (kernel, backend) = mk_kernel(tmp, tag).await;
    set(
        &kernel,
        &backend,
        json!({"context_strategy": "fold", "context_window": 4000, "output_reserve": 0}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "parent": null, "content": "GOAL"}),
    )
    .await;
    for t in ["a", "b", "c", "d", "e", "f"] {
        closed(&kernel, &backend, t, 2000).await;
    }
    project(&kernel, &backend).await;
    (kernel, backend)
}

#[tokio::test]
async fn folds_survive_a_projection_over_the_view_array() {
    // The bug: a turn rebuilt from the view array holds folds without their members,
    // and the next projection past TRIGGER rendered them nowhere.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = folded_once(&tmp, "fv").await;
    let rows = store(&kernel, &backend).await;
    let first = fold_ids(&rows);
    assert!(!first.is_empty(), "the first projection folded");
    assert_eq!(views(&rows).last().unwrap()["why"], "projection: fold");
    assert!(
        rows.iter()
            .filter(|r| is_fold(r))
            .all(|f| f["anchor"].is_i64()),
        "a new fold carries its anchor"
    );
    // the next prompt is rebuilt from that view array, and is pushed past TRIGGER
    // by what lands after it
    for t in ["g", "h", "i", "j"] {
        closed(&kernel, &backend, t, 2000).await;
    }
    project(&kernel, &backend).await;
    let source = crate::state::state_for(&backend)
        .view_source
        .lock()
        .unwrap()
        .clone();
    assert_eq!(source.as_deref(), Some("array"));
    let rows = store(&kernel, &backend).await;
    assert!(fold_ids(&rows).len() > first.len(), "it folded again");
    let shown: Vec<i64> = views(&rows).last().unwrap()["ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_i64())
        .collect();
    let reached = reaches(&rows, &shown);
    let missing: Vec<i64> = first
        .iter()
        .copied()
        .filter(|f| !reached.contains(f))
        .collect();
    assert!(
        missing.is_empty(),
        "folds {missing:?} fell out of the view {shown:?}"
    );
}

#[tokio::test]
async fn context_forget_prunes_covered_rows_and_old_views() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = folded_once(&tmp, "ff").await;
    for t in ["g", "h", "i", "j"] {
        closed(&kernel, &backend, t, 2000).await;
    }
    project(&kernel, &backend).await;
    let rows = store(&kernel, &backend).await;
    let folds = fold_ids(&rows);
    assert!(!folds.is_empty());
    let cov: HashSet<i64> = expand_ids(&rows, &folds).into_iter().collect();
    let vs: Vec<i64> = views(&rows).iter().filter_map(|r| row_id(r)).collect();
    assert!(vs.len() >= 2);
    let before = prompt(&kernel, &backend).await;
    let out = call(&kernel, &backend, json!({"type": "context_forget"})).await;
    assert_eq!(
        out,
        json!({"pruned": cov.len() + vs.len() - 1, "kept_folds": folds.len(), "unanchored": 0})
    );
    let after = store(&kernel, &backend).await;
    let left: HashSet<i64> = after.iter().filter_map(row_id).collect();
    assert!(
        cov.is_disjoint(&left),
        "every row a fold summarises is gone"
    );
    assert!(folds.iter().all(|f| left.contains(f)), "every fold is kept");
    let after_views: Vec<i64> = views(&after).iter().filter_map(|r| row_id(r)).collect();
    assert_eq!(after_views, vs[vs.len() - 1..].to_vec());
    assert!(
        !after.iter().any(|r| r["kind"] == "anchors"),
        "folds that carry their anchor need no anchors row"
    );
    assert_eq!(
        prompt(&kernel, &backend).await,
        before,
        "the next prompt is the same bytes"
    );
    let again = call(&kernel, &backend, json!({"type": "context_forget"})).await;
    assert_eq!(again["pruned"], 0, "{again}");
    // a pruned id is gone for history; the fold still says what it was
    let gone = *cov.iter().min().unwrap();
    let h = call(
        &kernel,
        &backend,
        json!({"type": "history", "fold": folds[0]}),
    )
    .await;
    assert!(
        h["fold"]["summary"].as_str().is_some_and(|s| !s.is_empty()),
        "{h}"
    );
    assert!(
        !h["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| row_id(r) == Some(gone)),
        "{h}"
    );
}

#[tokio::test]
async fn context_forget_keeps_open_chains_and_unanchored_folds() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "fo").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "parent": null, "content": "GOAL"}),
    )
    .await;
    let done = closed(&kernel, &backend, "done", 10).await;
    // an open chain: its last row is a send still owed a reply; its head is folded
    let mut o = closed(&kernel, &backend, "open", 10).await;
    o.pop();
    let last = put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "parent": o[2], "content": CALL}),
    )
    .await;
    o.push(last);
    // a fold written before the field, with its members: anchored by deriving
    let legacy_m = closed(&kernel, &backend, "legacy", 10).await;
    let f_done = make_fold(&done, "d", "b", false, kernel.mint_id(), done[0], 1, None);
    let f_open = make_fold(&o[..2], "o", "b", false, kernel.mint_id(), o[0], 1, None);
    let legacy = without_anchor(make_fold(
        &legacy_m,
        "l",
        "b",
        false,
        kernel.mint_id(),
        0,
        1,
        None,
    ));
    // and one with neither field nor members: its place cannot be known
    let lost = without_anchor(make_fold(
        &[],
        "lost",
        "b",
        false,
        kernel.mint_id(),
        0,
        1,
        None,
    ));
    let kept: Vec<i64> = [&f_done, &f_open, &legacy, &lost]
        .iter()
        .filter_map(|f| row_id(f))
        .collect();
    for f in [f_done, f_open, legacy.clone(), lost] {
        put_row(&kernel, &backend, f).await;
    }
    let out = call(&kernel, &backend, json!({"type": "context_forget"})).await;
    assert_eq!(
        out,
        json!({"pruned": done.len() + legacy_m.len(), "kept_folds": 4, "unanchored": 1})
    );
    let left = store(&kernel, &backend).await;
    let ids_left: HashSet<i64> = left.iter().filter_map(row_id).collect();
    // the row the open chain closed was replaced above; every row it has now stays
    assert!(
        o.iter().all(|r| ids_left.contains(r)),
        "an open chain keeps every row"
    );
    assert!(kept.iter().all(|f| ids_left.contains(f)));
    let recorded: Vec<Value> = left
        .iter()
        .filter(|r| r["kind"] == "anchors")
        .map(|r| r["anchors"].clone())
        .collect();
    assert_eq!(
        recorded,
        vec![json!({row_id(&legacy).unwrap().to_string(): legacy_m[0]})],
        "the legacy fold's place is recorded before its members go"
    );
}

#[tokio::test]
async fn a_dump_from_before_anchors_restores_with_the_same_view() {
    let d = dump();
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |_| {}).await;
    assert_eq!(
        kernel.root().unwrap().meta.read().unwrap()["guid"],
        d["guid"],
        "the dump was made under the test guid"
    );
    let aid = d["agent"].as_str().unwrap();
    let top = d["tables"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| row_id(&t["row"]))
        .max()
        .unwrap();
    while kernel.mint_id() <= top {} // the kernel's counter is past every stored id
    let file_id = format!("ff_{aid}");
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic"), aid).await;
    let agent = add_backend(&kernel, aid, &file_id).await;
    call(
        &kernel,
        &AgentId::from(MEMORY),
        json!({"type": "inject", "tables": d["tables"]}),
    )
    .await;

    async fn rendered(kernel: &Arc<Kernel>, agent: &AgentId) -> Value {
        let head = as_agent(agent.clone(), crate::memory::head(agent, kernel))
            .await
            .unwrap();
        let hist = as_agent(
            agent.clone(),
            crate::memory::load_history(agent, kernel, false),
        )
        .await
        .unwrap();
        json!(ids(&render(&hist, &anchors_of(&head))))
    }

    assert_eq!(
        rendered(&kernel, &agent).await,
        d["render_ids"],
        "the view main rendered"
    );
    let state = crate::state::state_for(&agent);
    let (msgs, _) = as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            &agent,
            &state,
            "next",
            &kernel,
            "c",
            Some("sys"),
            crate::caching::Caching::Cleanup,
        ),
    )
    .await
    .unwrap();
    let prompt_ids: Vec<Value> = msgs[1..msgs.len() - 1]
        .iter()
        .map(|m| m.get("id").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(json!(prompt_ids), d["prompt_ids"]);
    let before = prompt(&kernel, &agent).await;
    let rows = store(&kernel, &agent).await;
    let last_clear = rows
        .iter()
        .filter(|r| r["kind"] == "clear")
        .filter_map(row_id)
        .max()
        .unwrap();
    let head_folds: Vec<&Value> = rows
        .iter()
        .filter(|r| is_fold(r) && row_id(r).is_some_and(|i| i > last_clear))
        .collect();
    assert!(!head_folds.is_empty() && head_folds.iter().all(|f| f.get("anchor").is_none()));
    let hf: Vec<i64> = head_folds.iter().filter_map(|f| row_id(f)).collect();
    let members: HashSet<i64> = expand_ids(&rows, &hf).into_iter().collect();
    let all_anchors = anchors_of(&rows);
    let out = call(&kernel, &agent, json!({"type": "context_forget"})).await;
    assert_eq!(out["unanchored"], 0, "{out}");
    assert_eq!(out["kept_folds"], hf.len(), "{out}");
    assert_eq!(
        prompt(&kernel, &agent).await,
        before,
        "the next prompt is the same bytes"
    );
    let left = store(&kernel, &agent).await;
    assert!(
        members.is_disjoint(&left.iter().filter_map(row_id).collect()),
        "the old folds' members are gone"
    );
    let recorded: Vec<&Value> = left.iter().filter(|r| r["kind"] == "anchors").collect();
    assert_eq!(recorded.len(), 1);
    let want: Map<String, Value> = hf
        .iter()
        .map(|f| (f.to_string(), json!(all_anchors[f])))
        .collect();
    assert_eq!(recorded[0]["anchors"], Value::Object(want));
    assert_eq!(rendered(&kernel, &agent).await, d["render_ids"]);
    // dump → inject into a fresh store under the same guid: still the same view
    let tables = call(&kernel, &AgentId::from(MEMORY), json!({"type": "dump"})).await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": fantastic_ram_store::HANDLER_MODULE, "id": "memory2",
                   "callable_by": {"$self": "*", aid: "*"}}),
        )])
        .await;
    let mut can_call = kernel.agents.get(&agent).unwrap().meta.read().unwrap()["can_call"].clone();
    can_call["memory2"] = json!("*");
    set(
        &kernel,
        &agent,
        json!({"memory_store_id": "memory2", "can_call": can_call}),
    )
    .await;
    call(
        &kernel,
        &AgentId::from("memory2"),
        json!({"type": "inject", "tables": tables["tables"]}),
    )
    .await;
    crate::memory::forget_head(&agent, &kernel);
    assert_eq!(rendered(&kernel, &agent).await, d["render_ids"]);
    assert_eq!(prompt(&kernel, &agent).await, before);
}

#[tokio::test]
async fn context_forget_include_cleared() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "fc").await;
    let old = closed(&kernel, &backend, "old", 10).await;
    let cleared = call(
        &kernel,
        &backend,
        json!({"type": "context_clear", "why": "next"}),
    )
    .await;
    let new = closed(&kernel, &backend, "new", 10).await;
    let kept = call(&kernel, &backend, json!({"type": "context_forget"})).await;
    let left: HashSet<i64> = store(&kernel, &backend)
        .await
        .iter()
        .filter_map(row_id)
        .collect();
    assert!(
        old.iter().all(|r| left.contains(r)),
        "without include_cleared nothing behind"
    );
    let out = call(
        &kernel,
        &backend,
        json!({"type": "context_forget", "include_cleared": true}),
    )
    .await;
    let left: HashSet<i64> = store(&kernel, &backend)
        .await
        .iter()
        .filter_map(row_id)
        .collect();
    assert!(
        !old.iter().any(|r| left.contains(r)),
        "the rows before the clear go"
    );
    assert!(
        left.contains(&cleared["cleared"].as_i64().unwrap()),
        "the clear row stays"
    );
    assert!(new.iter().all(|r| left.contains(r)));
    assert_eq!(out["pruned"], old.len(), "{out}");
    assert_eq!(kept["pruned"], 0, "{kept}");
    let bad = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_forget", "include_cleared": "yes"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
    assert_eq!(bad["arg"], "include_cleared", "{bad}");
}

#[tokio::test]
async fn context_forget_waits_for_a_running_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "fw").await;
    let s = script(&backend, vec![pass(DONE)], Duration::from_millis(400));
    let k2 = Arc::clone(&kernel);
    let b2 = backend.clone();
    let turn = tokio::spawn(async move {
        k2.send(vec![Item::new(b2, json!({"type": "send", "text": "go"}))])
            .await
    });
    for _ in 0..250 {
        if !s.requests.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !s.requests.lock().unwrap().is_empty(),
        "the turn reached the model"
    );
    let k3 = Arc::clone(&kernel);
    let b3 = backend.clone();
    let forget = tokio::spawn(async move {
        k3.send(vec![Item::new(b3, json!({"type": "context_forget"}))])
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!forget.is_finished(), "it waits for the turn");
    turn.await.unwrap();
    let out = tokio::time::timeout(Duration::from_secs(5), forget)
        .await
        .expect("forget ends once the turn does")
        .unwrap()
        .remove(0);
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert!(out.result["pruned"].as_i64().is_some());
}

// ── the canvas AI cell's S8: folds at a small window, and every turn answers ──

/// What the fold's summary side call answers (e2e `ai_cell` S8's responder).
const S8_SUMMARY: &str =
    "Filler turns about lorem ipsum\n\nASKED: filler turns\nDONE: answered each\n\
                          BINDING: none\nERRORS: none\nOPEN: none";

/// e2e `canvas.ai_cell` S8 as full turns: an AI at a 12000 window on the fold
/// strategy (async + titled), eight long answers, each followed by the runtime's
/// no-call reminder answered `done`. The model's two SIDE calls are answered in
/// their own shapes — the fold's summary, and the `context_recall` a fold batch
/// makes due — so neither takes a turn's queued answer. Every turn answers, the
/// folds land, and the turn after them answers too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folds_at_a_small_window_and_every_turn_answers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "s8").await;
    set(
        &kernel,
        &backend,
        json!({"context_strategy": "fold", "context_window": 12000, "max_tokens": 12000,
               "context_async": true, "context_titles": true}),
    )
    .await;
    let s = script(&backend, Vec::new(), Duration::ZERO);
    let recalls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&recalls);
    *s.responder.lock().unwrap() = Some(Box::new(move |msgs: &[Value]| {
        let all = Value::Array(msgs.to_vec()).to_string();
        let last = msgs
            .last()
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .trim_start()
            .to_string();
        if all.contains("FOLD this finished piece of work") {
            Some(S8_SUMMARY.to_string())
        } else if last.starts_with("You are rebuilding your own working context") {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(r#"{"expand": [], "why": "the filler is folded"}"#.to_string())
        } else if last.starts_with("[no call was dispatched]") || last.starts_with("[no answer]") {
            Some(DONE.to_string())
        } else {
            None
        }
    }));
    let long = "lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(120);
    let answered = |want: String| {
        let kernel = Arc::clone(&kernel);
        let backend = backend.clone();
        async move {
            for _ in 0..100 {
                let rows = store(&kernel, &backend).await;
                if rows.iter().any(|r| {
                    r["role"] == "assistant"
                        && r["content"].as_str().is_some_and(|c| c.starts_with(&want))
                }) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }
    };
    let turn = |text: String, reply: String| {
        let kernel = Arc::clone(&kernel);
        let backend = backend.clone();
        let s = Arc::clone(&s);
        async move {
            s.passes.lock().unwrap().push_back(pass(reply));
            let _ = kernel
                .send(vec![Item::new(
                    backend,
                    json!({"type": "send", "text": text, "client_id": "cell"}),
                )])
                .await;
        }
    };
    for k in 0..8 {
        turn(
            format!("turn {k} {}", &long[..300]),
            format!("S8-turn-{k} {long}"),
        )
        .await;
        assert!(
            answered(format!("S8-turn-{k}")).await,
            "turn {k} was never answered"
        );
    }
    let rows = store(&kernel, &backend).await;
    assert!(
        rows.iter().any(is_fold),
        "eight long turns at a 12000 window fold"
    );
    assert!(
        recalls.load(Ordering::SeqCst) >= 1,
        "the folds made a context_recall due, and it ran inside a turn"
    );
    turn("still there?".to_string(), "S8-after-fold".to_string()).await;
    assert!(
        answered("S8-after-fold".to_string()).await,
        "the turn after the folds was never answered"
    );
}

// ── a new fold's anchor: known for every member, or the fold fails loud ──

#[test]
fn a_new_folds_anchor_is_its_least_leaf() {
    let rows = vec![
        json!({"role": "user", "id": 7, "content": "a"}),
        json!({"role": "user", "id": 30, "kind": "fold", "refs": [3, 4], "anchor": 3, "content": "f"}),
        json!({"role": "user", "id": 31, "kind": "fold", "refs": [1], "content": "legacy"}),
    ];
    // a member fold places by its own anchor, a legacy one by the head's anchors
    let anchors = HashMap::from([(31, 1)]);
    assert_eq!(member_anchor(&rows, &[0], &anchors).unwrap(), 7);
    assert_eq!(member_anchor(&rows, &[0, 1], &anchors).unwrap(), 3);
    assert_eq!(member_anchor(&rows, &[0, 1, 2], &anchors).unwrap(), 1);
}

#[test]
fn a_fold_that_cannot_be_placed_is_an_error_not_a_panic() {
    let rows = vec![
        json!({"role": "user", "id": 31, "kind": "fold", "refs": [1], "content": "legacy"}),
        json!({"role": "user", "id": "x", "content": "a row with no int id"}),
        json!({"role": "user", "content": "a row with no id"}),
    ];
    let none = HashMap::new();
    let err = |idxs: &[usize]| member_anchor(&rows, idxs, &none).unwrap_err().to_string();
    assert_eq!(err(&[0]), "ValueError: fold: member fold 31 has no anchor");
    assert_eq!(err(&[1]), "ValueError: fold: member row 'x' has no int id");
    assert_eq!(err(&[2]), "ValueError: fold: member row None has no int id");
    assert_eq!(
        err(&[]),
        "ValueError: fold: a fold needs at least one member"
    );
}

#[tokio::test]
async fn write_fold_reports_an_unplaceable_span_before_it_summarizes() {
    // NO PANIC IN A TURN: a span with a member that has no known place is
    // `FoldFail::Anchor` — the turn's error — and nothing is asked or written.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "fa").await;
    let s = script(&backend, Vec::new(), Duration::ZERO);
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        script: Arc::clone(&s),
    });
    let rows = vec![
        json!({"role": "user", "id": 31, "kind": "fold", "refs": [1], "content": "legacy"}),
        json!({"role": "user", "id": 32, "content": "a"}),
    ];
    let anchors = HashMap::new();
    let ctx = crate::strategies::ProjectionCtx {
        budget: 4000,
        recent_n: 6,
        provider: &provider,
        self_id: &backend,
        kernel: &kernel,
        client_id: "cli",
        anchors: &anchors,
    };
    let out = as_agent(
        backend.clone(),
        crate::strategies::fold::write_fold(&ctx, &rows, &[0, 1], &anchors),
    )
    .await;
    match out {
        Err(crate::strategies::fold::FoldFail::Anchor(e)) => {
            assert_eq!(e.0, "fold: member fold 31 has no anchor");
        }
        other => panic!("expected FoldFail::Anchor, got {other:?}"),
    }
    assert_eq!(s.passes_served(), 0, "no summary was asked for");
    assert!(
        !store(&kernel, &backend).await.iter().any(is_fold),
        "no fold row was written"
    );
}
