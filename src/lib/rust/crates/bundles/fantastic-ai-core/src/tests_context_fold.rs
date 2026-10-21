//! `context_fold`: the operator folds a chosen range of the view into one fold row.
//! The view before the range stays byte-identical, the fold sits where the range
//! began, the rows after stay in place; the fold is the automatic fold's own shape,
//! reachable by `history {fold}` and `grep`; every refusal names its reason.
//!
//! A child of `tests`, so it drives the same harness: a real `ram_store` under the
//! root, the mock provider, and `mk_kernel` backends.

use super::*;
use fantastic_kernel::test_support::as_agent;

const DONE: &str = "<call name=\"done\"></call>";

/// One verb sent to the agent; its result.
async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
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

/// The persisted view's ids, in order.
async fn view_ids(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<i64> {
    as_agent(agent.clone(), crate::memory::load_view(agent, kernel))
        .await
        .expect("store read")
        .expect("a view is persisted")["ids"]
        .as_array()
        .expect("ids")
        .iter()
        .filter_map(|e| e["id"].as_i64())
        .collect()
}

/// The message list the next turn would send, as the model view assembles it.
async fn messages(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
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
    msgs
}

/// Alternating user/assistant rows, each with its own text; their ids in order.
async fn turns(kernel: &Arc<Kernel>, agent: &AgentId, texts: &[&str]) -> Vec<i64> {
    let mut ids = Vec::new();
    for (i, t) in texts.iter().enumerate() {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        ids.push(put_row(kernel, agent, json!({"role": role, "content": t})).await);
    }
    ids
}

/// The fold row with this id, from the store.
fn fold_of(rows: &[Value], fid: i64) -> Value {
    rows.iter()
        .find(|r| r["id"].as_i64() == Some(fid) && r["kind"] == "fold")
        .cloned()
        .unwrap_or_else(|| panic!("no fold {fid} in {rows:?}"))
}

fn refs(row: &Value) -> Vec<i64> {
    row["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .filter_map(Value::as_i64)
        .collect()
}

const SIX: [&str; 6] = [
    "question one about the parser",
    "answer one, the parser reads tokens",
    "question two about the lexer",
    "answer two, the lexer splits lines",
    "question three about the printer",
    "answer three, the printer writes bytes",
];

#[tokio::test]
async fn the_default_range_is_the_rows_after_the_last_clear() {
    // No `from`/`to`: the range starts after the clear and ends at the last row.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_default").await;
    turns(&kernel, &backend, &["old one", "old two"]).await;
    let clear = verb(
        &kernel,
        &backend,
        json!({"type": "context_clear", "why": "next task"}),
    )
    .await;
    let cleared = clear["cleared"].as_i64().expect("the clear row");
    let ids = turns(&kernel, &backend, &["task one", "task two", "task three"]).await;

    let out = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "summary": "the first task\n\nDONE: all three"}),
    )
    .await;
    assert_eq!(out["rows"], 3, "{out}");
    assert_eq!(out["summarized"], true, "{out}");
    let fid = out["folded"].as_i64().expect("the fold's id");

    let rows = store(&kernel, &backend).await;
    let fold = fold_of(&rows, fid);
    assert_eq!(refs(&fold), ids, "the members are the rows after the clear");
    assert_eq!(fold["kind"], "fold");
    assert_eq!(fold["level"], 1);
    assert_eq!(
        fold["content"].as_str().unwrap(),
        format!("[fold {fid} · 3 refs · L1] \"the first task\"\nthe first task\n\nDONE: all three"),
    );
    // the view: the clear, then the fold — nothing after the fold
    assert_eq!(view_ids(&kernel, &backend).await, vec![cleared, fid]);
    let msgs = messages(&kernel, &backend).await;
    assert!(msgs[2]["content"].as_str().unwrap().starts_with("[fold "));
    assert_eq!(
        msgs.len(),
        4,
        "system, clear, fold, the next turn: {msgs:?}"
    );
}

#[tokio::test]
async fn an_explicit_middle_range_leaves_the_rows_around_it_in_place() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_middle").await;
    let ids = turns(&kernel, &backend, &SIX).await;
    let before = messages(&kernel, &backend).await;

    let out = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": ids[2], "to": ids[3],
               "summary": "the lexer work\n\nthe lexer splits lines"}),
    )
    .await;
    assert_eq!(out["rows"], 2, "{out}");
    let fid = out["folded"].as_i64().unwrap();

    let after = messages(&kernel, &backend).await;
    // system, and the two rows before the range: byte-identical
    assert_eq!(
        after[..3],
        before[..3],
        "the prefix before the fold is unchanged"
    );
    // the fold, where the range began
    assert!(after[3]["content"]
        .as_str()
        .unwrap()
        .starts_with(&format!("[fold {fid} ·")));
    // the rows after the range, then the next turn, in place
    assert_eq!(
        after[4..],
        before[5..],
        "the rows after the range stay in place"
    );

    assert_eq!(
        view_ids(&kernel, &backend).await,
        vec![ids[0], ids[1], fid, ids[4], ids[5]],
    );
    let rows = store(&kernel, &backend).await;
    assert_eq!(refs(&fold_of(&rows, fid)), vec![ids[2], ids[3]]);
    // nothing deleted: the members are still rows of the store
    for id in &ids {
        assert!(
            rows.iter().any(|r| r["id"].as_i64() == Some(*id)),
            "row {id} kept"
        );
    }
}

#[tokio::test]
async fn a_given_summary_is_used_verbatim_with_no_provider_call() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_verbatim").await;
    let ids = turns(&kernel, &backend, &SIX[..2]).await;
    let s = script(&backend, vec![], Duration::ZERO);
    let summary = "  Verbatim title \n\n  exact body, kept as given  ";

    let out = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "summary": summary}),
    )
    .await;
    assert_eq!(out["summarized"], true, "{out}");
    assert_eq!(s.passes_served(), 0, "the summarizer was not asked");
    let fid = out["folded"].as_i64().unwrap();
    let fold = fold_of(&store(&kernel, &backend).await, fid);
    assert_eq!(
        fold["body"].as_str(),
        Some(summary),
        "the body is the summary"
    );
    assert_eq!(refs(&fold), ids);
    assert_eq!(
        fold["content"].as_str().unwrap(),
        format!("[fold {fid} · 2 refs · L1] \"Verbatim title\"\n{summary}"),
    );
}

#[tokio::test]
async fn an_absent_summary_calls_the_summarizer_once() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_absent").await;
    let ids = turns(&kernel, &backend, &SIX[..2]).await;
    let s = script(
        &backend,
        vec![pass("Summarised title\n\nASKED: the parser\nDONE: read it")],
        Duration::ZERO,
    );

    let out = verb(&kernel, &backend, json!({"type": "context_fold"})).await;
    assert_eq!(s.passes_served(), 1, "the summarizer ran once: {out}");
    assert_eq!(out["summarized"], true, "{out}");
    assert_eq!(out["rows"], 2);
    let fid = out["folded"].as_i64().unwrap();
    let fold = fold_of(&store(&kernel, &backend).await, fid);
    assert_eq!(refs(&fold), ids);
    assert!(
        fold["content"].as_str().unwrap().starts_with(&format!(
            "[fold {fid} · 2 refs · L1] \"Summarised title\"\nASKED: the parser"
        )),
        "{fold}"
    );
}

#[tokio::test]
async fn a_summarizer_that_gives_nothing_folds_the_titles_and_says_so() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_degraded").await;
    turns(&kernel, &backend, &SIX[..2]).await;
    script(&backend, vec![pass("   ")], Duration::ZERO);

    let out = verb(&kernel, &backend, json!({"type": "context_fold"})).await;
    assert_eq!(out["summarized"], false, "{out}");
    let fid = out["folded"].as_i64().unwrap();
    let fold = fold_of(&store(&kernel, &backend).await, fid);
    assert!(
        fold["content"]
            .as_str()
            .unwrap()
            .contains("(summary unavailable"),
        "{fold}"
    );
}

#[tokio::test]
async fn history_fold_returns_the_members_of_an_operator_fold() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_history").await;
    let ids = turns(&kernel, &backend, &SIX[..4]).await;
    let out = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": ids[1], "to": ids[2], "summary": "middle"}),
    )
    .await;
    let fid = out["folded"].as_i64().unwrap();

    let h = verb(&kernel, &backend, json!({"type": "history", "fold": fid})).await;
    assert_eq!(h["fold"]["id"], fid, "{h}");
    assert_eq!(h["fold"]["members"], 2, "{h}");
    assert_eq!(h["fold"]["summary"], "middle", "{h}");
    let member_ids: Vec<i64> = h["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_i64())
        .collect();
    assert_eq!(member_ids, vec![ids[1], ids[2]]);
}

#[tokio::test]
async fn grep_reaches_the_rows_an_operator_fold_hides() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_grep").await;
    let ids = turns(&kernel, &backend, &SIX).await;
    verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": ids[2], "to": ids[3], "summary": "lexer"}),
    )
    .await;
    let g = verb(
        &kernel,
        &backend,
        json!({"type": "grep", "query": "lexer splits lines"}),
    )
    .await;
    assert_eq!(g["total"], 1, "{g}");
    assert_eq!(g["matches"][0]["id"], ids[3], "{g}");
}

#[tokio::test]
async fn a_range_refuses_an_empty_range_and_an_id_outside_the_view() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_refuse").await;
    let ids = turns(&kernel, &backend, &SIX[..4]).await;

    let empty = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": ids[3], "to": ids[1], "summary": "x"}),
    )
    .await;
    assert_eq!(empty["reason"], "bad_arg", "{empty}");
    assert_eq!(empty["arg"], "to", "{empty}");

    let unknown_from = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": 999_999, "summary": "x"}),
    )
    .await;
    assert_eq!(unknown_from["reason"], "bad_arg", "{unknown_from}");
    assert_eq!(unknown_from["arg"], "from", "{unknown_from}");

    let unknown_to = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "to": 999_999, "summary": "x"}),
    )
    .await;
    assert_eq!(unknown_to["arg"], "to", "{unknown_to}");

    // nothing was written by any refusal
    assert!(store(&kernel, &backend)
        .await
        .iter()
        .all(|r| r["kind"] != "fold"));
}

#[tokio::test]
async fn a_range_refuses_to_cross_a_fold_or_a_clear() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_cross").await;
    let ids = turns(&kernel, &backend, &SIX[..5]).await;
    verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": ids[2], "to": ids[3], "summary": "inner"}),
    )
    .await;

    // the view is [ids0, ids1, fold, ids4]: this range crosses the fold
    let crosses_fold = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": ids[0], "to": ids[4], "summary": "x"}),
    )
    .await;
    assert_eq!(crosses_fold["reason"], "bad_arg", "{crosses_fold}");
    assert!(crosses_fold["error"]
        .as_str()
        .unwrap()
        .contains("crosses a fold or clear"));

    let clear = verb(&kernel, &backend, json!({"type": "context_clear"})).await;
    let cleared = clear["cleared"].as_i64().unwrap();
    let after = turns(&kernel, &backend, &["after one", "after two"]).await;
    // the view is [clear, after one, after two]: this range starts on the clear
    let crosses_clear = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": cleared, "to": after[1], "summary": "x"}),
    )
    .await;
    assert_eq!(crosses_clear["reason"], "bad_arg", "{crosses_clear}");
    assert!(crosses_clear["error"]
        .as_str()
        .unwrap()
        .contains("crosses a fold or clear"));
}

#[tokio::test]
async fn a_default_range_after_a_fold_that_ends_the_view_is_empty() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_empty_default").await;
    turns(&kernel, &backend, &SIX[..2]).await;
    let whole = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "summary": "all of it"}),
    )
    .await;
    assert!(whole["folded"].as_i64().is_some(), "{whole}");

    // the view is now the one fold: nothing lies after it
    let again = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "summary": "nothing"}),
    )
    .await;
    assert_eq!(again["reason"], "bad_arg", "{again}");
    assert_eq!(again["arg"], "to", "{again}");
}

#[tokio::test]
async fn a_bad_argument_is_refused_by_name() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_bad_args").await;
    turns(&kernel, &backend, &SIX[..2]).await;

    for (args, arg) in [
        (json!({"from": "not an id"}), "from"),
        (json!({"to": true}), "to"),
        (json!({"summary": 5}), "summary"),
        (json!({"summary": "   "}), "summary"),
    ] {
        let mut payload = json!({"type": "context_fold"});
        for (k, v) in args.as_object().unwrap() {
            payload[k] = v.clone();
        }
        let out = verb(&kernel, &backend, payload).await;
        assert_eq!(out["reason"], "bad_arg", "{out}");
        assert_eq!(out["arg"], arg, "{out}");
    }
    assert!(store(&kernel, &backend)
        .await
        .iter()
        .all(|r| r["kind"] != "fold"));
}

#[tokio::test]
async fn a_turn_running_for_the_agent_refuses_the_fold() {
    // The FIFO lock is held for the whole turn: a fold now would move the view under it.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_running").await;
    turns(&kernel, &backend, &SIX[..2]).await;
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

    let out = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "summary": "mid-turn"}),
    )
    .await;
    assert_eq!(out["reason"], "locked", "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("a turn is running"),
        "{out}"
    );

    turn.await.unwrap();
    assert!(
        store(&kernel, &backend)
            .await
            .iter()
            .all(|r| r["kind"] != "fold"),
        "the refused fold wrote nothing"
    );
}

#[tokio::test]
async fn a_view_array_naming_a_missing_row_is_no_view() {
    // A stale array (a row the store no longer has) is not the view: the head is.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_stale").await;
    let ids = turns(&kernel, &backend, &SIX[..3]).await;
    as_agent(
        backend.clone(),
        crate::memory::save_view(
            &backend,
            &kernel,
            &[json!({"id": 999_999, "res": "full"})],
            1000,
            "test",
        ),
    )
    .await
    .expect("view saved");

    let out = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "summary": "all three"}),
    )
    .await;
    assert_eq!(out["rows"], 3, "{out}");
    let fid = out["folded"].as_i64().unwrap();
    assert_eq!(refs(&fold_of(&store(&kernel, &backend).await, fid)), ids);
}

#[tokio::test]
async fn the_prefix_keeps_the_trimmed_bytes_the_model_saw_before_the_fold() {
    // The model's view trims the store: a whole-state read a later one supersedes is
    // dropped, and a body repeated from an earlier reply is stubbed. A fold must not
    // bring either back: the messages before the range stay byte-identical.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cf_trim").await;
    let long = "the same long reply body, repeated word for word. ".repeat(10);
    let superseded = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "[reply to 1 · notes reflect]\nthe old state"}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "content": "noted"}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "[reply to 2 · notes reflect]\nthe new state"}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "content": "noted again"}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": format!("[reply to 3 · notes read]\n{long}")}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "content": "read it"}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": format!("[reply to 4 · notes read]\n{long}")}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "content": "and again"}),
    )
    .await;
    let ids = turns(
        &kernel,
        &backend,
        &[
            "question about the range",
            "answer about the range",
            "question after the range",
            "answer after the range",
        ],
    )
    .await;
    let before = messages(&kernel, &backend).await;
    // the trimmers did their work: the superseded read is gone, the repeat is a stub
    assert!(!before
        .iter()
        .any(|m| m["content"] == "[reply to 1 · notes reflect]\nthe old state"));
    assert!(before.iter().any(|m| m["content"]
        .as_str()
        .unwrap_or("")
        .contains("[identical to the reply in row")));
    let k = before
        .iter()
        .position(|m| m["content"] == "question about the range")
        .expect("the range's first row");

    let out = verb(
        &kernel,
        &backend,
        json!({"type": "context_fold", "from": ids[0], "to": ids[1],
               "summary": "the range, folded"}),
    )
    .await;
    assert_eq!(out["rows"], 2, "{out}");
    let fid = out["folded"].as_i64().unwrap();

    let after = messages(&kernel, &backend).await;
    let f = after
        .iter()
        .position(|m| {
            m["content"]
                .as_str()
                .unwrap_or("")
                .starts_with(&format!("[fold {fid} ·"))
        })
        .expect("the fold message");
    assert_eq!(f, k, "the fold sits where the range began");
    assert_eq!(
        serde_json::to_string(&after[..f]).unwrap(),
        serde_json::to_string(&before[..k]).unwrap(),
        "the messages before the fold are byte-identical",
    );
    assert_eq!(
        serde_json::to_string(&after[f + 1..]).unwrap(),
        serde_json::to_string(&before[k + 2..]).unwrap(),
        "the rows after the range are byte-identical",
    );
    assert!(
        !view_ids(&kernel, &backend).await.contains(&superseded),
        "a row the view dropped stays out of the persisted view"
    );
}
