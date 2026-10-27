//! `fork`, `mark`, `fork_points` (#425): a new agent from the first N rows of one.
//! The copy keeps the ids, the card and grants match, the prefix assembles to the
//! same prompt; every refusal names its reason and leaves nothing behind.
//!
//! A child of `tests`, so it drives the same harness: a real `ram_store` under the
//! root, the mock provider, and `mk_kernel` backends.

use super::*;
use crate::helpers::agent_meta;
use fantastic_kernel::test_support::as_agent;

const DONE: &str = "<call name=\"done\"></call>";

async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// Every row `agent` holds in its store (read AS the agent), bookkeeping included.
async fn store(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

fn ids(rows: &[Value]) -> Vec<i64> {
    rows.iter().filter_map(|r| r["id"].as_i64()).collect()
}

/// A finished turn: a user row and the assistant's answer, linked by `parent`.
async fn turn(kernel: &Arc<Kernel>, agent: &AgentId, q: &str, a: &str) -> (i64, i64) {
    let u = put_row(kernel, agent, json!({"role": "user", "content": q})).await;
    let r = put_row(
        kernel,
        agent,
        json!({"role": "assistant", "content": a, "parent": u}),
    )
    .await;
    (u, r)
}

/// Three finished turns; the ids of every row, in order.
async fn three_turns(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<i64> {
    let mut all = Vec::new();
    for (q, a) in [
        ("question one about the parser", "answer one, tokens"),
        ("question two about the lexer", "answer two, lines"),
        ("question three about the printer", "answer three, bytes"),
    ] {
        let (u, r) = turn(kernel, agent, q, a).await;
        all.extend([u, r]);
    }
    all
}

async fn fold_over(kernel: &Arc<Kernel>, agent: &AgentId, members: &[i64]) -> i64 {
    let fid = kernel.mint_id();
    put_row(
        kernel,
        agent,
        crate::folds::make_fold(
            members, "the span", "summary", false, fid, members[0], 1, None,
        ),
    )
    .await
}

/// The prompt the agent's next turn would send.
async fn messages(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let state = crate::state::state_for(agent);
    as_agent(
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
    .expect("assemble")
    .0
}

fn exists(kernel: &Arc<Kernel>, id: &str) -> bool {
    kernel.agents.contains_key(&AgentId::from(id))
}

#[tokio::test]
async fn fork_copies_rows_up_to_at_with_their_ids() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_copy").await;
    let all = three_turns(&kernel, &parent).await;
    let at = all[3];
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": at})).await;
    assert_eq!(out["copied"], 4, "{out}");
    assert_eq!(out["at"], at, "{out}");
    let fork = AgentId::from(out["id"].as_str().expect("the fork's id"));
    let theirs = store(&kernel, &fork).await;
    assert_eq!(
        ids(&theirs),
        all[..4].to_vec(),
        "ids kept, in order, none above"
    );
    assert_eq!(
        theirs,
        store(&kernel, &parent).await[..4].to_vec(),
        "the rows are the parent's, field for field"
    );
}

#[tokio::test]
async fn fork_copies_a_whole_fold_with_its_members() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_fold").await;
    let all = three_turns(&kernel, &parent).await;
    // the first turn is folded AFTER the second one ran: the fold's id is above `at`
    let fid = fold_over(&kernel, &parent, &all[..2]).await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": all[1]})).await;
    assert_eq!(out["copied"], 3, "{out}");
    let fork = AgentId::from(out["id"].as_str().unwrap());
    let theirs = store(&kernel, &fork).await;
    assert_eq!(
        ids(&theirs),
        vec![all[0], all[1], fid],
        "members, then the fold"
    );
    assert_eq!(theirs[2]["refs"], json!([all[0], all[1]]), "{theirs:?}");
}

#[tokio::test]
async fn fork_refuses_an_at_that_cuts_a_fold() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_cutfold").await;
    let all = three_turns(&kernel, &parent).await;
    fold_over(&kernel, &parent, &all[..4]).await;
    // `at` ends turn one: two of the fold's four members are before it, two after
    let out = verb(
        &kernel,
        &parent,
        json!({"type": "fork", "at": all[1], "id": "fk_cutfold_new"}),
    )
    .await;
    assert_eq!(out["reason"], "cuts_fold", "{out}");
    assert_eq!(out["at"], all[1], "{out}");
    assert!(!exists(&kernel, "fk_cutfold_new"), "no agent is created");
}

#[tokio::test]
async fn fork_refuses_an_at_that_cuts_a_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_cutturn").await;
    let u = put_row(&kernel, &parent, json!({"role": "user", "content": "go"})).await;
    let call = call_envelope("kernel", json!({"type": "reflect"}), Some("self"));
    let a = put_row(
        &kernel,
        &parent,
        json!({"role": "assistant", "content": call, "parent": u}),
    )
    .await;
    let r = put_row(
        &kernel,
        &parent,
        json!({"role": "user", "content": "[reply]", "parent": a}),
    )
    .await;
    let done = put_row(
        &kernel,
        &parent,
        json!({"role": "assistant", "content": "fine", "parent": r}),
    )
    .await;
    // between the call and its reply, and between the question and its answer
    for at in [a, u, r] {
        let out = verb(
            &kernel,
            &parent,
            json!({"type": "fork", "at": at, "id": "fk_cutturn_new"}),
        )
        .await;
        assert_eq!(out["reason"], "cuts_turn", "at {at}: {out}");
        assert!(!exists(&kernel, "fk_cutturn_new"));
    }
    // the end of the finished turn is a boundary
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": done})).await;
    assert_eq!(out["copied"], 4, "{out}");
}

#[tokio::test]
async fn fork_refuses_an_open_chain() {
    // a call whose reply is still owed: the turn is not finished, wherever `at` is
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_open").await;
    let u = put_row(&kernel, &parent, json!({"role": "user", "content": "go"})).await;
    let call = call_envelope("kernel", json!({"type": "reflect"}), Some("self"));
    let a = put_row(
        &kernel,
        &parent,
        json!({"role": "assistant", "content": call, "parent": u}),
    )
    .await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": a})).await;
    assert_eq!(out["reason"], "cuts_turn", "{out}");
}

#[tokio::test]
async fn fork_refuses_an_at_that_names_no_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_norow").await;
    three_turns(&kernel, &parent).await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": 999_999})).await;
    assert_eq!(out["reason"], "no_such_row", "{out}");
    assert_eq!(out["at"], 999_999, "{out}");
    // a view row is no conversation row either
    let view = put_row(&kernel, &parent, json!({"kind": "view", "ids": []})).await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": view})).await;
    assert_eq!(out["reason"], "no_such_row", "{out}");
    for bad in [json!(null), json!("x"), json!(-1)] {
        let out = verb(&kernel, &parent, json!({"type": "fork", "at": bad})).await;
        assert_eq!(out["reason"], "bad_arg", "{out}");
        assert_eq!(out["arg"], "at", "{out}");
    }
}

#[tokio::test]
async fn fork_refuses_while_the_parent_is_mid_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_busy").await;
    let all = three_turns(&kernel, &parent).await;
    let lock = Arc::clone(&crate::state::state_for(&parent).lock);
    let _turn = lock.lock().await;
    let out = verb(
        &kernel,
        &parent,
        json!({"type": "fork", "at": all[1], "id": "fk_busy_new"}),
    )
    .await;
    assert_eq!(out["reason"], "parent_mid_turn", "{out}");
    assert!(!exists(&kernel, "fk_busy_new"));
}

#[tokio::test]
async fn the_parents_later_rows_folds_and_forgets_do_not_reach_the_fork() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_later").await;
    let all = three_turns(&kernel, &parent).await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": all[3]})).await;
    let fork = AgentId::from(out["id"].as_str().unwrap());
    let before = store(&kernel, &fork).await;
    // the parent goes on: a new turn, a fold of the whole history, a forget
    turn(&kernel, &parent, "question four", "answer four").await;
    let folded = verb(
        &kernel,
        &parent,
        json!({"type": "context_fold", "summary": "everything so far"}),
    )
    .await;
    assert!(folded["folded"].is_i64(), "{folded}");
    let forgot = verb(&kernel, &parent, json!({"type": "context_forget"})).await;
    assert!(forgot["pruned"].as_i64().unwrap() > 0, "{forgot}");
    assert_eq!(store(&kernel, &fork).await, before, "the fork is as it was");
}

#[tokio::test]
async fn the_forks_rows_do_not_reach_the_parent() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_back").await;
    let all = three_turns(&kernel, &parent).await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": all[3]})).await;
    let fork = AgentId::from(out["id"].as_str().unwrap());
    let before = store(&kernel, &parent).await;
    turn(
        &kernel,
        &fork,
        "only the fork asks",
        "only the fork answers",
    )
    .await;
    assert_eq!(
        store(&kernel, &parent).await,
        before,
        "the parent is as it was"
    );
    assert_eq!(store(&kernel, &fork).await.len(), 6);
}

#[tokio::test]
async fn the_fork_has_the_parents_card_and_grants() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_card").await;
    let mut patch = Map::new();
    patch.insert("system_prompt".into(), json!("you are the parser agent"));
    patch.insert("endpoint".into(), json!("http://127.0.0.1:1"));
    patch.insert("output_reserve".into(), json!(512));
    kernel.update(&parent, patch);
    let all = three_turns(&kernel, &parent).await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": all[1]})).await;
    let fork = AgentId::from(out["id"].as_str().unwrap());
    assert_ne!(fork, parent);
    let (p, f) = (agent_meta(&parent, &kernel), agent_meta(&fork, &kernel));
    for key in [
        "model",
        "endpoint",
        "system_prompt",
        "memory_store_id",
        "context_window",
        "output_reserve",
        "can_call",
    ] {
        assert_eq!(f.get(key), p.get(key), "{key}");
        assert!(f.get(key).is_some(), "{key} is on the card");
    }
    let (pa, fa) = (
        kernel.agents.get(&parent).unwrap().handler_module.clone(),
        kernel.agents.get(&fork).unwrap().handler_module.clone(),
    );
    assert_eq!(pa, fa, "the same handler");
    let store_meta = agent_meta(&AgentId::from(MEMORY), &kernel);
    assert!(
        store_meta["callable_by"].get(fork.as_str()).is_some(),
        "the store admits the fork: {store_meta:?}"
    );
    // a name and an id are taken as given
    let named = verb(
        &kernel,
        &parent,
        json!({"type": "fork", "at": all[1], "name": "the twin", "id": "fk_card_twin"}),
    )
    .await;
    assert_eq!(named["id"], "fk_card_twin", "{named}");
    assert_eq!(
        agent_meta(&AgentId::from("fk_card_twin"), &kernel)["display_name"],
        "the twin"
    );
}

#[tokio::test]
async fn fork_is_all_or_nothing_on_a_store_refusal() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_refused").await;
    let all = three_turns(&kernel, &parent).await;
    // the store lets the parent read and prune, not append: the fork inherits that
    kernel
        .edit_callable_by(&AgentId::from(MEMORY), |cb| {
            cb.insert(parent.as_str().to_string(), json!(["read", "prune"]));
            true
        })
        .unwrap();
    let out = verb(
        &kernel,
        &parent,
        json!({"type": "fork", "at": all[3], "id": "fk_refused_new"}),
    )
    .await;
    assert_eq!(out["reason"], "store_refused", "{out}");
    assert!(!exists(&kernel, "fk_refused_new"), "the agent is deleted");
    let dump = verb(&kernel, &AgentId::from(MEMORY), json!({"type": "dump"})).await;
    let rows = dump["tables"]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        rows.iter().all(|r| !r["owner"]
            .as_str()
            .unwrap_or("")
            .ends_with("fk_refused_new")),
        "the store holds none of its rows"
    );
    let cb = agent_meta(&AgentId::from(MEMORY), &kernel)["callable_by"].clone();
    assert!(
        cb.get("fk_refused_new").is_none(),
        "the grant is taken back: {cb}"
    );
}

#[tokio::test]
async fn the_forks_prompt_equals_the_parents_for_the_same_prefix() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_prompt").await;
    // the reference: the same record, only the rows up to `at` — the prompt the
    // parent sent for that prefix
    let file = format!("ff_{}", id_for(&tmp, "fk_prompt_ref"));
    add_file_bridge(
        &kernel,
        &file,
        &tmp.path().join(".fantastic").join("ref"),
        &id_for(&tmp, "fk_prompt_ref"),
    )
    .await;
    let reference = add_backend(&kernel, &id_for(&tmp, "fk_prompt_ref"), &file).await;

    // turn one, a fold of it, turn two with the view row written before its answer
    // (the fold and the view are both at or before `at`), then a third turn
    let (u1, a1) = turn(
        &kernel,
        &parent,
        "question one about the parser",
        "answer one",
    )
    .await;
    let fid = fold_over(&kernel, &parent, &[u1, a1]).await;
    let u2 = put_row(
        &kernel,
        &parent,
        json!({"role": "user", "content": "question two"}),
    )
    .await;
    let view = json!({"kind": "view", "v": 1, "budget": 1000, "why": "t",
        "ids": [{"id": fid, "res": "fold"}, {"id": u2, "res": "full"}]});
    let view_id = put_row(&kernel, &parent, view).await;
    let at = put_row(
        &kernel,
        &parent,
        json!({"role": "assistant", "content": "answer two", "parent": u2}),
    )
    .await;
    turn(&kernel, &parent, "question three", "answer three").await;

    let out = verb(&kernel, &parent, json!({"type": "fork", "at": at})).await;
    let fork = AgentId::from(out["id"].as_str().unwrap_or_else(|| panic!("{out}")));
    let copied = store(&kernel, &fork).await;
    assert_eq!(ids(&copied), vec![u1, a1, fid, u2, view_id, at]);
    // plant the reference with exactly the parent's rows up to `at` (same ids), by hand
    let mine: Vec<Value> = store(&kernel, &parent)
        .await
        .into_iter()
        .filter(|r| r["id"].as_i64().unwrap() <= at)
        .collect();
    as_agent(
        reference.clone(),
        crate::memory::save_history(&reference, &kernel, &mine),
    )
    .await
    .expect("plant the reference");
    // what a provider receives: the rows without the store's `agent` stamp
    let plain = |mut m: Vec<Value>| {
        m.iter_mut().for_each(|x| {
            x.as_object_mut().map(|o| o.remove("agent"));
        });
        m
    };
    let theirs = plain(messages(&kernel, &fork).await);
    let wanted = plain(messages(&kernel, &reference).await);
    assert!(theirs.len() > 3, "a real prompt: {theirs:?}");
    assert_eq!(
        serde_json::to_string(&theirs).unwrap(),
        serde_json::to_string(&wanted).unwrap(),
        "byte for byte"
    );
    assert!(
        serde_json::to_string(&theirs).unwrap().contains("[fold "),
        "the fold is in the prompt"
    );
}

#[tokio::test]
async fn mark_lands_on_a_turn_boundary_outside_a_fold() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "mk_edge").await;
    let all = three_turns(&kernel, &parent).await;
    // mid-turn: refused
    {
        let lock = Arc::clone(&crate::state::state_for(&parent).lock);
        let _turn = lock.lock().await;
        let out = verb(&kernel, &parent, json!({"type": "mark", "label": "busy"})).await;
        assert_eq!(out["reason"], "mid_turn", "{out}");
    }
    // a label is required
    for bad in [json!(null), json!(""), json!("  "), json!(3)] {
        let out = verb(&kernel, &parent, json!({"type": "mark", "label": bad})).await;
        assert_eq!(out["reason"], "bad_arg", "{out}");
        assert_eq!(out["arg"], "label", "{out}");
    }
    // inside an open chain: refused
    let u = put_row(&kernel, &parent, json!({"role": "user", "content": "go"})).await;
    let call = call_envelope("kernel", json!({"type": "reflect"}), Some("self"));
    put_row(
        &kernel,
        &parent,
        json!({"role": "assistant", "content": call, "parent": u}),
    )
    .await;
    let out = verb(&kernel, &parent, json!({"type": "mark", "label": "open"})).await;
    assert_eq!(out["reason"], "open_chain", "{out}");

    // a head that ends in a fold is a valid place, and `fork` accepts its `at`
    let (kernel, parent) = mk_kernel(&tmp, "mk_edge_fold").await;
    let all2 = three_turns(&kernel, &parent).await;
    let fid = fold_over(&kernel, &parent, &all2).await;
    let out = verb(
        &kernel,
        &parent,
        json!({"type": "mark", "label": "after the fold", "note": "n", "version": "abc123"}),
    )
    .await;
    assert_eq!(out["at"], fid, "{out}");
    assert_eq!(out["label"], "after the fold", "{out}");
    assert!(out["marked"].as_i64().unwrap() > fid, "{out}");
    let forked = verb(&kernel, &parent, json!({"type": "fork", "at": out["at"]})).await;
    assert_eq!(forked["copied"], 7, "{forked}");
    let _ = all;
}

#[tokio::test]
async fn mark_keeps_only_the_last_n_marks() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "mk_last").await;
    three_turns(&kernel, &parent).await;
    let n = crate::constants::MAX_MARKS;
    for i in 0..n + 2 {
        let out = verb(
            &kernel,
            &parent,
            json!({"type": "mark", "label": format!("m{i}")}),
        )
        .await;
        assert!(out["marked"].is_i64(), "{out}");
    }
    let marks: Vec<Value> = store(&kernel, &parent)
        .await
        .into_iter()
        .filter(|r| r["kind"] == "mark")
        .collect();
    assert_eq!(marks.len(), n);
    assert_eq!(marks[0]["label"], "m2", "the two oldest are gone");
    assert_eq!(marks[n - 1]["label"], format!("m{}", n + 1));
}

#[tokio::test]
async fn mark_rows_never_reach_the_prompt() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "mk_hidden").await;
    three_turns(&kernel, &parent).await;
    let before = serde_json::to_string(&messages(&kernel, &parent).await).unwrap();
    let out = verb(
        &kernel,
        &parent,
        json!({"type": "mark", "label": "SECRETLABEL", "note": "SECRETNOTE"}),
    )
    .await;
    assert!(out["marked"].is_i64(), "{out}");
    let after = serde_json::to_string(&messages(&kernel, &parent).await).unwrap();
    assert!(!after.contains("SECRETLABEL") && !after.contains("SECRETNOTE"));
    assert_eq!(before, after, "the prompt is the same with the mark");
}

#[tokio::test]
async fn fork_points_lists_this_agents_marks_with_rows_after_model_and_warm() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fp_list").await;
    let (_, a1) = turn(&kernel, &parent, "one", "ok one").await;
    let first = verb(
        &kernel,
        &parent,
        json!({"type": "mark", "label": "context read", "note": "before edits", "version": "c0ffee"}),
    )
    .await;
    assert_eq!(first["at"], a1, "{first}");
    turn(&kernel, &parent, "two", "ok two").await;
    let (_, a3) = turn(&kernel, &parent, "three", "ok three").await;
    verb(
        &kernel,
        &parent,
        json!({"type": "mark", "label": "issue 7 merged", "version": "deadbe"}),
    )
    .await;

    let out = verb(&kernel, &parent, json!({"type": "fork_points"})).await;
    let points = out["points"].as_array().expect("points");
    assert_eq!(points.len(), 2, "{out}");
    assert_eq!(
        points[0],
        json!({"agent": parent.as_str(), "at": a1, "label": "context read",
            "note": "before edits", "version": "c0ffee", "rows_after": 4,
            "model": "test-model", "warm": false}),
    );
    assert_eq!(points[1]["at"], a3);
    assert_eq!(points[1]["rows_after"], 0);
    assert_eq!(points[1]["note"], Value::Null);

    // WARM while the last send ended inside the cache lifetime, cold after
    let state = crate::state::state_for(&parent);
    let now = crate::helpers::now_secs();
    *state.last_send_secs.lock().unwrap() = Some(now - 10.0);
    let out = verb(&kernel, &parent, json!({"type": "fork_points"})).await;
    assert_eq!(out["points"][0]["warm"], true, "{out}");
    *state.last_send_secs.lock().unwrap() = Some(now - crate::constants::CACHE_LIFETIME_SECS - 1.0);
    let out = verb(&kernel, &parent, json!({"type": "fork_points"})).await;
    assert_eq!(out["points"][0]["warm"], false, "{out}");

    // the filter: a substring of the label, the exact version
    let by_label = verb(
        &kernel,
        &parent,
        json!({"type": "fork_points", "filter": {"label": "merged"}}),
    )
    .await;
    assert_eq!(by_label["points"].as_array().unwrap().len(), 1);
    assert_eq!(by_label["points"][0]["label"], "issue 7 merged");
    let by_version = verb(
        &kernel,
        &parent,
        json!({"type": "fork_points", "filter": {"version": "c0ffee"}}),
    )
    .await;
    assert_eq!(by_version["points"][0]["label"], "context read");
    let none = verb(
        &kernel,
        &parent,
        json!({"type": "fork_points", "filter": {"version": "c0ff"}}),
    )
    .await;
    assert_eq!(none["points"], json!([]), "a version is matched whole");
}

#[tokio::test]
async fn a_finished_send_makes_the_agent_warm() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fp_warm").await;
    turn(&kernel, &parent, "one", "ok one").await;
    verb(&kernel, &parent, json!({"type": "mark", "label": "m"})).await;
    let cold = verb(&kernel, &parent, json!({"type": "fork_points"})).await;
    assert_eq!(cold["points"][0]["warm"], false, "{cold}");
    let s = script(&parent, vec![pass(DONE)], Duration::ZERO);
    verb(&kernel, &parent, json!({"type": "send", "text": "go"})).await;
    assert!(until_passes(&s, 1).await);
    let warm = verb(&kernel, &parent, json!({"type": "fork_points"})).await;
    assert_eq!(warm["points"][0]["warm"], true, "{warm}");
}

#[tokio::test]
async fn two_forks_of_one_mark_start_from_the_same_rows() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fp_twins").await;
    three_turns(&kernel, &parent).await;
    let mark = verb(&kernel, &parent, json!({"type": "mark", "label": "base"})).await;
    turn(&kernel, &parent, "later", "ok later").await;
    let point = verb(&kernel, &parent, json!({"type": "fork_points"})).await["points"][0].clone();
    assert_eq!(point["at"], mark["at"]);
    let a = verb(&kernel, &parent, json!({"type": "fork", "at": point["at"]})).await;
    let b = verb(&kernel, &parent, json!({"type": "fork", "at": point["at"]})).await;
    let (a, b) = (
        AgentId::from(a["id"].as_str().unwrap()),
        AgentId::from(b["id"].as_str().unwrap()),
    );
    assert_ne!(a, b);
    let rows = store(&kernel, &a).await;
    assert_eq!(rows.len(), 6);
    assert_eq!(rows, store(&kernel, &b).await, "ids and content alike");
}

#[tokio::test]
async fn fork_from_forks_the_named_agent() {
    // `fork {from:{agent, at}}` on a peer: the same verb, run on the agent named
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_from").await;
    let peer = add_peer(&kernel, &tmp, &parent).await;
    let all = three_turns(&kernel, &parent).await;
    let out = verb(
        &kernel,
        &peer,
        json!({"type": "fork", "from": {"agent": parent.as_str(), "at": all[1]}}),
    )
    .await;
    assert_eq!(out["copied"], 2, "{out}");
    let fork = AgentId::from(out["id"].as_str().unwrap());
    assert_eq!(ids(&store(&kernel, &fork).await), all[..2].to_vec());
    let bad = verb(
        &kernel,
        &peer,
        json!({"type": "fork", "from": {"agent": 3}}),
    )
    .await;
    assert_eq!(bad["reason"], "bad_arg", "{bad}");
}

/// `fork` and `mark` already answer in the kernel's shape: the sentence in
/// `error`, the code in `reason`. Pinned so they stay that way (#451).
#[tokio::test]
async fn fork_and_mark_refusals_put_sentence_in_error_code_in_reason() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "fk_shape").await;
    let u = put_row(&kernel, &parent, json!({"role": "user", "content": "go"})).await;
    let call = call_envelope("kernel", json!({"type": "reflect"}), Some("self"));
    let a = put_row(
        &kernel,
        &parent,
        json!({"role": "assistant", "content": call, "parent": u}),
    )
    .await;
    let r = put_row(
        &kernel,
        &parent,
        json!({"role": "user", "content": "[reply]", "parent": a}),
    )
    .await;
    put_row(
        &kernel,
        &parent,
        json!({"role": "assistant", "content": "fine", "parent": r}),
    )
    .await;
    let out = verb(&kernel, &parent, json!({"type": "fork", "at": a})).await;
    assert_eq!(out["reason"], "cuts_turn", "{out}");
    assert_ne!(out["error"], out["reason"], "{out}");
    assert!(out["error"].as_str().unwrap().contains(' '), "{out}");
    assert!(out["hint"].is_string(), "{out}");
    assert!(out.get("at").is_some(), "{out}");
    let lock = Arc::clone(&crate::state::state_for(&parent).lock);
    let _turn = lock.lock().await;
    let out = verb(&kernel, &parent, json!({"type": "mark", "label": "busy"})).await;
    assert_eq!(out["reason"], "mid_turn", "{out}");
    assert_ne!(out["error"], out["reason"], "{out}");
}
