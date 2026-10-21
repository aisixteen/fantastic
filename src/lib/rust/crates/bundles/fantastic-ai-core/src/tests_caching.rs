//! The record's `caching` modes: `cleanup` trims the view, `none` and `provider_cache`
//! keep it append-only, and `provider_cache` flags the primer's end and the last
//! fold's. The primer pins its prefix against `context_fold`.
//!
//! A child of `tests`, so it drives the same harness: a real `ram_store` under the
//! root and `mk_kernel` backends (the mock bundle, which accepts any record).

use super::*;
use crate::caching::Caching;
use fantastic_kernel::test_support::as_agent;

/// One verb sent to the agent; its result.
async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// Set record fields on the agent (the mock accepts any record).
async fn set(kernel: &Arc<Kernel>, agent: &AgentId, fields: Value) {
    let mut payload = json!({"type": "update_agent", "id": agent.as_str()});
    payload
        .as_object_mut()
        .expect("object")
        .extend(fields.as_object().expect("object").clone());
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), payload)])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
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

/// The model view the next turn assembles, in the given caching mode.
async fn view_with(kernel: &Arc<Kernel>, agent: &AgentId, caching: Caching) -> Vec<Value> {
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
            caching,
        ),
    )
    .await
    .expect("assemble");
    msgs
}

/// The history part of a view as `(role, content)`: the system block and the live
/// user turn are left out.
fn history(msgs: &[Value]) -> Vec<(String, String)> {
    msgs[1..msgs.len() - 1]
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or("").to_string(),
                m["content"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// The stored turns (user and assistant rows) as `(role, content)`, in store order.
async fn stored_turns(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<(String, String)> {
    store(kernel, agent)
        .await
        .iter()
        .filter(|r| matches!(r["role"].as_str(), Some("user" | "assistant")))
        .map(|r| {
            (
                r["role"].as_str().unwrap_or("").to_string(),
                r["content"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// A user row's text, with the id it was stored under.
async fn put(kernel: &Arc<Kernel>, agent: &AgentId, role: &str, content: &str) -> i64 {
    put_row(kernel, agent, json!({"role": role, "content": content})).await
}

/// A reply row's text: the tag, then the body.
fn reply(pid: i64, verb: &str, body: &str) -> String {
    format!("[reply to {pid} · notes {verb}]\n{body}")
}

/// A body long enough to be stubbed when it repeats (400 characters and up).
fn big() -> String {
    "the same long listing, line after line, ".repeat(14)
}

/// A docs reply the model has answered: the body names its verb in `<verbs>`.
fn docs(pid: i64) -> String {
    format!(
        "[reply to {pid} · kernel docs]\n<of>runner</of>\n<bundle>exec_bridge.tools</bundle>\
         \n<tier>oneliners</tier>\n<verbs>\n<start>args: command:str</start>\n</verbs>"
    )
}

/// The `[fold …]` row's id, the fold a `context_fold` made.
async fn fold(kernel: &Arc<Kernel>, agent: &AgentId, from: i64, to: i64, summary: &str) -> i64 {
    let out = verb(
        kernel,
        agent,
        json!({"type": "context_fold", "from": from, "to": to, "summary": summary}),
    )
    .await;
    out["folded"]
        .as_i64()
        .unwrap_or_else(|| panic!("context_fold refused: {out}"))
}

/// The module the caching mock is registered under: a backend that declares
/// provider caching, as the anthropic backend does.
const CACHING_MODULE: &str = "mock_caching.tools";
const CACHING_CFG: BackendConfig = BackendConfig {
    name: "mock_caching",
    model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
    provider_cache: true,
};

/// [`mk_kernel`] on the caching mock, so a `provider_cache` record is accepted and a
/// turn reaches the provider with its cache flags.
async fn mk_caching_kernel(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let kernel = base_kernel(tmp, |k| {
        k.bundles
            .register(CACHING_MODULE, MockBundle { cfg: CACHING_CFG });
    })
    .await;
    let backend_id = id_for(tmp, tag);
    let file_id = format!("ff_{}", backend_id);
    add_file_bridge(
        &kernel,
        &file_id,
        &tmp.path().join(".fantastic"),
        &backend_id,
    )
    .await;
    let backend = add_backend_on(&kernel, &backend_id, &file_id, CACHING_MODULE).await;
    (kernel, backend)
}

/// One `send` turn on the agent: the messages the provider was handed for the turn
/// itself, as it received them (cache flags included). An owed recall is a provider
/// pass of its own, served first, so the turn is the pass whose last message is the
/// live text.
async fn turn_messages(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) -> Vec<Value> {
    let s = script(
        agent,
        vec![pass("ok"), pass("ok"), pass("ok")],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(kernel, agent);
    let reply = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": text, "client_id": "c"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        reply.get("error").is_none() && reply.get("raised").is_none(),
        "{reply}"
    );
    assert!(until_passes(&s, 1).await, "the provider was never called");
    (0..s.passes_served())
        .map(|n| s.request_messages(n))
        .find(|msgs| msgs.last().and_then(|m| m["content"].as_str()) == Some(text))
        .unwrap_or_else(|| panic!("no provider pass carried the turn {text:?}"))
}

/// The contents of the messages that carry `key` set to true.
fn flagged_contents(msgs: &[Value], key: &str) -> Vec<String> {
    msgs.iter()
        .filter(|m| m.get(key) == Some(&json!(true)))
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect()
}

// ─── the record's values ────────────────────────────────────────────────────

#[test]
fn caching_is_an_enum_that_defaults_to_cleanup_and_refuses_anything_else() {
    let sch = crate::schema::schema(vec![]);
    let field = sch["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "caching")
        .expect("caching is a shared field")
        .clone();
    assert_eq!(field["type"], "enum");
    assert_eq!(field["enum"], json!(["none", "cleanup", "provider_cache"]));
    assert_eq!(field["default"], "cleanup");
    let rec = json!({"endpoint": "e", "memory_store_id": "m"});
    let rec = rec.as_object().unwrap();
    let check = |meta: Value| crate::schema::validate(&sch, "x", meta.as_object().unwrap(), rec);
    for legal in ["none", "cleanup", "provider_cache"] {
        assert_eq!(check(json!({"caching": legal})), None, "{legal}");
    }
    assert_eq!(
        check(json!({"caching": "always"})).as_deref(),
        Some("caching='always' is not one of the legal values: none, cleanup, provider_cache")
    );
    let primer = sch["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "cache_primer_until")
        .expect("the primer is a shared field");
    assert_eq!(primer["type"], "int");
}

// ─── the three modes over one history ───────────────────────────────────────

#[tokio::test]
async fn cleanup_trims_the_view_and_none_and_provider_cache_keep_every_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_modes").await;
    // a whole-state read superseded by a later one
    put(
        &kernel,
        &agent,
        "user",
        "[reply to 11 · notes reflect]\nold state",
    )
    .await;
    put(&kernel, &agent, "assistant", "ok one").await;
    put(
        &kernel,
        &agent,
        "user",
        "[reply to 14 · notes reflect]\nnew state",
    )
    .await;
    put(&kernel, &agent, "assistant", "ok two").await;
    // the same long body read twice
    put(&kernel, &agent, "user", &reply(12, "read", &big())).await;
    put(&kernel, &agent, "assistant", "ok three").await;
    put(&kernel, &agent, "user", &reply(21, "read", &big())).await;
    put(&kernel, &agent, "assistant", "ok four").await;
    // a docs reply, answered
    put(&kernel, &agent, "user", &docs(30)).await;
    put(&kernel, &agent, "assistant", "ok five").await;

    let stored = stored_turns(&kernel, &agent).await;
    let cleaned = history(&view_with(&kernel, &agent, Caching::Cleanup).await);
    let kept = history(&view_with(&kernel, &agent, Caching::None).await);
    let marked = history(&view_with(&kernel, &agent, Caching::ProviderCache).await);

    // cleanup: the superseded read, the repeated body and the used docs are trimmed
    let text: String = cleaned
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("old state"),
        "a superseded read drops: {text}"
    );
    assert!(text.contains("[identical to the reply in row"), "{text}");
    assert!(
        !text.contains("<start>args"),
        "the answered docs are cut: {text}"
    );
    assert_eq!(
        cleaned.len(),
        stored.len() - 1,
        "one superseded read dropped"
    );

    // none and provider_cache: every stored row, in order, verbatim
    assert_eq!(kept, stored, "none renders the stored rows, as stored");
    assert_eq!(
        marked, stored,
        "provider_cache renders the stored rows, as stored"
    );
}

#[tokio::test]
async fn none_and_provider_cache_are_append_only_each_turn_a_prefix_of_the_next() {
    for mode in [Caching::None, Caching::ProviderCache] {
        let tmp = TempDir::new().unwrap();
        let (kernel, agent) = mk_kernel(&tmp, &format!("cm_prefix_{}", mode.name())).await;
        let mut before: Vec<(String, String)> = Vec::new();
        for n in 0..3 {
            let now = history(&view_with(&kernel, &agent, mode).await);
            assert!(
                now.starts_with(&before),
                "{}: turn {n} is a prefix of turn {}: {now:?}",
                mode.name(),
                n + 1
            );
            before = now;
            // the rows a turn stores: the question, then the answer. A repeated long
            // body and a stale read are in the history, and neither is trimmed.
            put(&kernel, &agent, "user", &reply(40 + n, "read", &big())).await;
            put(&kernel, &agent, "user", &format!("question {n}")).await;
            put(&kernel, &agent, "assistant", &format!("answer {n}")).await;
        }
        let last = history(&view_with(&kernel, &agent, mode).await);
        assert_eq!(last.len(), 9, "{}: every row of three turns", mode.name());
        assert!(
            last.iter()
                .filter(|(_, c)| c.starts_with("[reply to "))
                .count()
                == 3,
            "{}: the three long reads are kept whole",
            mode.name()
        );
    }
}

// ─── the markers: which messages get flagged ────────────────────────────────

#[tokio::test]
async fn provider_cache_flags_the_primer_end_and_the_last_fold_and_the_fold_moves() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_flags").await;
    set(&kernel, &agent, json!({"caching": "provider_cache"})).await;
    put(&kernel, &agent, "user", "u1").await;
    let a1 = put(&kernel, &agent, "assistant", "a1").await;
    let u2 = put(&kernel, &agent, "user", "u2").await;
    let a2 = put(&kernel, &agent, "assistant", "a2").await;
    let u3 = put(&kernel, &agent, "user", "u3").await;
    let a3 = put(&kernel, &agent, "assistant", "a3").await;
    set(&kernel, &agent, json!({"cache_primer_until": a1})).await;
    let f1 = fold(&kernel, &agent, u2, a2, "fold one").await;
    let f2 = fold(&kernel, &agent, u3, a3, "fold two").await;

    // [u1, a1*primer, F1, F2*last fold], then the live turn
    let mut msgs = view_with(&kernel, &agent, Caching::ProviderCache).await;
    crate::caching::mark(&mut msgs, Some(a1));
    let flagged = |msgs: &[Value], key: &str| -> Vec<i64> {
        msgs.iter()
            .filter(|m| m.get(key) == Some(&json!(true)))
            .filter_map(|m| m["id"].as_i64())
            .collect()
    };
    assert_eq!(
        flagged(&msgs, "cache_primer"),
        vec![a1],
        "M2 at the primer's end"
    );
    assert_eq!(
        flagged(&msgs, "cache_fold"),
        vec![f2],
        "M3 at the last fold's end"
    );
    assert!(
        msgs.iter().any(|m| m["id"] == json!(f1)),
        "F1 stays in the view"
    );

    // a new fold after the last one moves M3; the primer's M2 stays where it was
    let u4 = put(&kernel, &agent, "user", "u4").await;
    let a4 = put(&kernel, &agent, "assistant", "a4").await;
    let f3 = fold(&kernel, &agent, u4, a4, "fold three").await;
    let mut msgs = view_with(&kernel, &agent, Caching::ProviderCache).await;
    crate::caching::mark(&mut msgs, Some(a1));
    assert_eq!(flagged(&msgs, "cache_primer"), vec![a1], "M2 stays");
    assert_eq!(
        flagged(&msgs, "cache_fold"),
        vec![f3],
        "M3 moves to the new fold"
    );
}

#[tokio::test]
async fn no_primer_field_flags_no_primer_and_no_fold_flags_no_fold() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_nofold").await;
    set(&kernel, &agent, json!({"caching": "provider_cache"})).await;
    let a1 = put(&kernel, &agent, "user", "u1").await;
    put(&kernel, &agent, "assistant", "a1").await;

    let mut msgs = view_with(&kernel, &agent, Caching::ProviderCache).await;
    crate::caching::mark(&mut msgs, None);
    assert!(
        msgs.iter().all(|m| m.get("cache_primer").is_none()),
        "no primer, no M2"
    );
    assert!(
        msgs.iter().all(|m| m.get("cache_fold").is_none()),
        "no fold, no M3"
    );

    // the primer is named, but no fold exists: M2 only
    let mut msgs = view_with(&kernel, &agent, Caching::ProviderCache).await;
    crate::caching::mark(&mut msgs, Some(a1));
    assert_eq!(
        msgs.iter()
            .filter(|m| m.get("cache_primer").is_some())
            .count(),
        1,
        "{msgs:?}"
    );
    assert!(
        msgs.iter().all(|m| m.get("cache_fold").is_none()),
        "no fold, no M3"
    );
}

#[tokio::test]
async fn a_primer_outside_the_view_flags_nothing_and_pins_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_gone").await;
    set(&kernel, &agent, json!({"caching": "provider_cache"})).await;
    put(&kernel, &agent, "user", "u1").await;
    let a1 = put(&kernel, &agent, "assistant", "a1").await;
    set(&kernel, &agent, json!({"cache_primer_until": a1})).await;
    // a clear starts a new thread: the old rows, primer included, leave the view
    verb(
        &kernel,
        &agent,
        json!({"type": "context_clear", "why": "next"}),
    )
    .await;
    let u2 = put(&kernel, &agent, "user", "u2").await;
    let a2 = put(&kernel, &agent, "assistant", "a2").await;
    let mut msgs = view_with(&kernel, &agent, Caching::ProviderCache).await;
    crate::caching::mark(&mut msgs, Some(a1));
    assert!(
        msgs.iter().all(|m| m.get("cache_primer").is_none()),
        "{msgs:?}"
    );
    // with the primer out of the view, a fold may start at the view's first row
    let f = fold(&kernel, &agent, u2, a2, "the new thread").await;
    assert!(f > a1, "the fold is a row of the new thread");
}

// ─── the primer pins its prefix ─────────────────────────────────────────────

#[tokio::test]
async fn context_fold_refuses_a_range_that_starts_at_or_before_the_primer() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_pinned").await;
    let u1 = put(&kernel, &agent, "user", "u1").await;
    let a1 = put(&kernel, &agent, "assistant", "a1").await;
    let u2 = put(&kernel, &agent, "user", "u2").await;
    let a2 = put(&kernel, &agent, "assistant", "a2").await;
    set(&kernel, &agent, json!({"cache_primer_until": a1})).await;

    // a range that includes the primer, one that precedes it, and the default range
    for payload in [
        json!({"type": "context_fold", "from": a1, "to": u2, "summary": "s"}),
        json!({"type": "context_fold", "from": u1, "to": u1, "summary": "s"}),
        json!({"type": "context_fold", "summary": "s"}),
    ] {
        let out = verb(&kernel, &agent, payload.clone()).await;
        assert_eq!(out["reason"], "bad_arg", "{payload}: {out}");
        assert_eq!(out["arg"], "from", "{payload}: {out}");
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("the primer is pinned"),
            "{out}"
        );
    }
    // the rows after the primer fold as before
    let out = verb(
        &kernel,
        &agent,
        json!({"type": "context_fold", "from": u2, "to": a2, "summary": "s"}),
    )
    .await;
    assert_eq!(out["rows"], 2, "{out}");
    assert!(out.get("error").is_none(), "{out}");
}

#[tokio::test]
async fn context_fold_refuses_a_caching_value_it_cannot_read() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_badmode").await;
    put(&kernel, &agent, "user", "u1").await;
    set(&kernel, &agent, json!({"caching": "always"})).await;
    let out = verb(
        &kernel,
        &agent,
        json!({"type": "context_fold", "summary": "s"}),
    )
    .await;
    assert_eq!(out["reason"], "bad_arg", "{out}");
    assert_eq!(out["arg"], "caching", "{out}");
}

#[tokio::test]
async fn provider_cache_is_refused_by_a_backend_that_cannot_cache() {
    // the mock backend declares no provider caching: a turn on it refuses the mode
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_cap").await;
    put(&kernel, &agent, "user", "kept").await;
    set(&kernel, &agent, json!({"caching": "provider_cache"})).await;
    let before = store(&kernel, &agent).await.len();
    let reply = verb(
        &kernel,
        &agent,
        json!({"type": "send", "text": "hi", "client_id": "c"}),
    )
    .await;
    let text = reply.to_string();
    assert!(
        text.contains("mock_backend: caching=provider_cache is not supported by this backend"),
        "{reply}"
    );
    assert_eq!(
        store(&kernel, &agent).await.len(),
        before,
        "a refused turn stores no row: {reply}"
    );
}

// ─── the flags reach the provider on a real turn ────────────────────────────

#[tokio::test]
async fn a_provider_cache_turn_hands_the_provider_the_primer_and_the_last_fold_flagged() {
    // [primer rows][fold][fold][task rows], then the live turn: the primer's end and
    // the LAST fold's end are flagged on the messages the provider receives, the
    // earlier fold is not.
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_caching_kernel(&tmp, "cm_wire").await;
    set(&kernel, &agent, json!({"caching": "provider_cache"})).await;
    put(&kernel, &agent, "user", "u1").await;
    let a1 = put(&kernel, &agent, "assistant", "a1").await;
    let u2 = put(&kernel, &agent, "user", "u2").await;
    let a2 = put(&kernel, &agent, "assistant", "a2").await;
    let u3 = put(&kernel, &agent, "user", "u3").await;
    let a3 = put(&kernel, &agent, "assistant", "a3").await;
    put(&kernel, &agent, "user", "task").await;
    set(&kernel, &agent, json!({"cache_primer_until": a1})).await;
    fold(&kernel, &agent, u2, a2, "fold one").await;
    fold(&kernel, &agent, u3, a3, "fold two").await;

    let msgs = turn_messages(&kernel, &agent, "live").await;
    assert_eq!(
        flagged_contents(&msgs, "cache_primer"),
        vec!["a1".to_string()],
        "M2 on the primer row's message only: {msgs:?}"
    );
    let folds = flagged_contents(&msgs, "cache_fold");
    assert_eq!(folds.len(), 1, "M3 on one message: {msgs:?}");
    assert!(
        folds[0].contains("\"fold two\""),
        "M3 is on the LAST fold: {folds:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m["content"].as_str().unwrap_or("").contains("\"fold one\"")),
        "the earlier fold stays in the view, unflagged: {msgs:?}"
    );
}

#[tokio::test]
async fn a_cleanup_or_none_turn_hands_the_provider_no_flag_even_with_a_primer_and_folds() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_caching_kernel(&tmp, "cm_wire_off").await;
    put(&kernel, &agent, "user", "u1").await;
    let a1 = put(&kernel, &agent, "assistant", "a1").await;
    let u2 = put(&kernel, &agent, "user", "u2").await;
    let a2 = put(&kernel, &agent, "assistant", "a2").await;
    set(&kernel, &agent, json!({"cache_primer_until": a1})).await;
    fold(&kernel, &agent, u2, a2, "fold one").await;
    for mode in ["cleanup", "none"] {
        set(&kernel, &agent, json!({"caching": mode})).await;
        let msgs = turn_messages(&kernel, &agent, "live").await;
        assert!(
            msgs.iter()
                .all(|m| m.get("cache_primer").is_none() && m.get("cache_fold").is_none()),
            "{mode}: no flag reaches the provider: {msgs:?}"
        );
        assert!(msgs.len() > 3, "{mode}: the view is sent: {msgs:?}");
    }
}

// ─── a mode switch keeps the view array's membership ────────────────────────

#[tokio::test]
async fn a_mode_switch_keeps_the_rows_a_cleanup_left_out_of_the_view_array() {
    // The view array is the record of what the view is. A fold written under cleanup
    // lists only the rows cleanup kept, so the superseded read stays out after a
    // switch to none: none renders the listed rows and the rows after the newest of
    // them. The row returns only when the array is gone (a context_clear starts a new
    // thread, so it never does within this one).
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "cm_switch").await;
    put(
        &kernel,
        &agent,
        "user",
        "[reply to 11 · notes reflect]\nold state",
    )
    .await;
    put(&kernel, &agent, "assistant", "ok one").await;
    put(
        &kernel,
        &agent,
        "user",
        "[reply to 14 · notes reflect]\nnew state",
    )
    .await;
    put(&kernel, &agent, "assistant", "ok two").await;
    let u5 = put(&kernel, &agent, "user", "question").await;
    let a5 = put(&kernel, &agent, "assistant", "answer").await;
    fold(&kernel, &agent, u5, a5, "fold").await;

    let cleaned = history(&view_with(&kernel, &agent, Caching::Cleanup).await);
    assert!(cleaned.iter().all(|(_, c)| !c.contains("old state")));
    let switched = history(&view_with(&kernel, &agent, Caching::None).await);
    assert_eq!(
        switched.len(),
        4,
        "the listed rows, the fold among them: {switched:?}"
    );
    assert!(
        switched.iter().all(|(_, c)| !c.contains("old state")),
        "a dropped row stays out of the array's view: {switched:?}"
    );
    assert!(switched.iter().any(|(_, c)| c.contains("new state")));
}
