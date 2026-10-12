//! The memory-store era of ai_core: the calibrated compaction trigger, the
//! repeat-read dedupe, the head cache that never diverges from the store, ids
//! minted at append, and the row event. The twin of python `ai_core/tests/test_memory_features.py`; the fold
//! summary budget half lives in `strategies/fold.rs`'s tests.
//!
//! A child of `tests`, so it drives the same harness: a real `ram_store` named
//! `memory` under the root, every backend wired to it both ways.

use super::*;
use crate::provider::ProviderReport;
use fantastic_kernel::test_support::as_agent;

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn call_as(kernel: &Arc<Kernel>, sender: &AgentId, target: &str, payload: Value) -> Value {
    as_agent(
        sender.clone(),
        kernel.send(vec![Item::new(AgentId::from(target), payload)]),
    )
    .await
    .remove(0)
    .result
}

async fn set_meta(kernel: &Arc<Kernel>, id: &AgentId, fields: Value) {
    let mut p = json!({"type": "update_agent", "id": id.as_str()});
    for (k, v) in fields.as_object().unwrap() {
        p[k] = v.clone();
    }
    let _ = call(kernel, "kernel_state", p).await;
}

fn contents(rows: &[Value]) -> Vec<Value> {
    rows.iter().map(|r| r["content"].clone()).collect()
}

// ── the trigger, calibrated by provider-reported usage ─────────────────

/// A provider that REPORTS a prompt size, like the real endpoints do.
struct Reporting(i64);

#[async_trait]
impl Provider for Reporting {
    async fn chat(
        &self,
        _messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        let ev = Ok(ProviderEvent::Token(
            "ok <call name=\"done\"></call>".into(),
        ));
        Ok(Box::pin(futures_util::stream::iter(vec![ev])))
    }
    fn model(&self) -> String {
        "reporting".into()
    }
    fn report(&self) -> ProviderReport {
        ProviderReport {
            prompt_tokens: Some(self.0),
            completion_tokens: None,
            stop_reason: None,
        }
    }
}

#[tokio::test]
async fn the_endpoints_count_calibrates_the_estimate() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai) = mk_kernel(&tmp, "calib").await;
    set_meta(
        &kernel,
        &ai,
        json!({"context_window": 12000, "output_reserve": 0}),
    )
    .await;
    let state = crate::state::state_for(&ai);
    let msgs = vec![
        json!({"role": "system", "content": "s"}),
        json!({"role": "user", "content": "x".repeat(4000)}),
    ];
    let provider: Arc<dyn Provider> = Arc::new(Reporting(0));
    // under the trigger by our own estimate (~1000 of a ~12k budget)
    let out = crate::projection::project_context(
        &provider,
        &ai,
        &state,
        &kernel,
        "cli",
        "mock",
        crate::constants::DEFAULT_CONTEXT_WINDOW,
        msgs.clone(),
        1,
    )
    .await
    .expect("projection");
    assert_eq!(out, msgs);
    let proj = state.projection.lock().unwrap().clone().unwrap();
    assert_eq!(proj["fired"], json!(false), "{proj}");
    // the endpoint said the same prompt was TEN times bigger — the budget shrinks
    *state.calib.lock().unwrap() = Some(10.0);
    let out = crate::projection::project_context(
        &provider,
        &ai,
        &state,
        &kernel,
        "cli",
        "mock",
        crate::constants::DEFAULT_CONTEXT_WINDOW,
        msgs.clone(),
        1,
    )
    .await;
    assert!(out.is_ok(), "{out:?}");
    let proj = state.projection.lock().unwrap().clone().unwrap();
    assert_eq!(proj["fired"], json!(true), "{proj}");
    assert_eq!(proj["calibration"], json!(10.0), "{proj}");
}

#[tokio::test]
async fn a_turn_records_what_the_endpoint_reported() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai) = mk_kernel(&tmp, "reported").await;
    set_meta(
        &kernel,
        &ai,
        json!({"context_window": 32000, "output_reserve": 0}),
    )
    .await;
    let provider: Arc<dyn Provider> = Arc::new(Reporting(777));
    let out = crate::verbs::send(
        provider,
        &ai,
        &json!({"type": "send", "text": "hello"}),
        &kernel,
        CFG,
    )
    .await
    .expect("send");
    assert!(out.get("error").is_none(), "{out}");
    let state = crate::state::state_for(&ai);
    assert!(state.calib.lock().unwrap().is_some_and(|c| c > 0.0));
    let st = call(&kernel, ai.as_str(), json!({"type": "context_status"})).await;
    assert_eq!(st["reported_prompt_tokens"], json!(777), "{st}");
    assert_eq!(st["trigger"], json!(0.8), "{st}");
}

// ── the same bytes twice: one copy in view ─────────────────────────────

#[test]
fn a_repeated_body_is_a_stub_citing_the_first() {
    let body = "line\n".repeat(200);
    let hist = vec![
        json!({"role": "user", "id": 1, "content": format!("[reply to 10 · notes read]\n{body}")}),
        json!({"role": "assistant", "id": 2, "content": "again"}),
        json!({"role": "user", "id": 3, "content": format!("[reply to 11 · notes read]\n{body}")}),
        json!({"role": "user", "id": 4, "content": "[reply to 12 · notes read]\nsmall"}),
    ];
    let out = crate::history_view::dedupe_repeat_bodies(&hist);
    assert_eq!(out[0], hist[0], "the first copy stays whole");
    assert!(
        out[2]["content"]
            .as_str()
            .unwrap()
            .contains("identical to the reply in row 1"),
        "{}",
        out[2]
    );
    assert_eq!(out[2]["id"], json!(3));
    assert_eq!(out[3], hist[3]);
}

#[tokio::test]
async fn a_row_another_writer_appends_reaches_the_next_head_read() {
    let tmp = TempDir::new().unwrap();
    let (kernel, alice) = mk_kernel(&tmp, "planted").await;
    fantastic_kernel::test_support::as_agent(
        alice.clone(),
        crate::memory::save_history(
            &alice,
            &kernel,
            &[json!({"role": "user", "content": "first"})],
        ),
    )
    .await
    .expect("append");
    let hist = fantastic_kernel::test_support::as_agent(
        alice.clone(),
        crate::memory::load_history(&alice, &kernel, false),
    )
    .await
    .expect("store read");
    assert_eq!(contents(&hist), vec![json!("first")]);
    let rid = kernel.mint_id();
    // another writer under alice's key — a direct `append` sent as alice, beside
    // the backend's own write-through
    let out = call_as(
        &kernel,
        &alice,
        MEMORY,
        json!({"type": "append",
               "rows": [{"id": rid, "role": "user", "content": "planted"}]}),
    )
    .await;
    assert_eq!(out["ids"], json!([rid]), "{out}");
    let hist = fantastic_kernel::test_support::as_agent(
        alice.clone(),
        crate::memory::load_history(&alice, &kernel, false),
    )
    .await
    .expect("store read");
    assert_eq!(contents(&hist), vec![json!("first"), json!("planted")]);
    // and read again: merged once, not twice
    assert_eq!(
        fantastic_kernel::test_support::as_agent(
            alice.clone(),
            crate::memory::load_history(&alice, &kernel, false)
        )
        .await
        .expect("store read")
        .len(),
        2
    );
}

/// A provider that writes a row to its agent's store MID-TURN — what the
/// projection's fold or view does — before answering.
struct MidTurnWriter {
    kernel: Arc<Kernel>,
    agent: AgentId,
}

#[async_trait]
impl Provider for MidTurnWriter {
    async fn chat(
        &self,
        _messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        fantastic_kernel::test_support::as_agent(
            self.agent.clone(),
            crate::memory::save_history(
                &self.agent,
                &self.kernel,
                &[json!({"role": "user", "content": "mid"})],
            ),
        )
        .await
        .map_err(|e| ProviderError::raised(e.0))?;
        let ev = Ok(ProviderEvent::Token(
            "ok <call name=\"done\"></call>".into(),
        ));
        Ok(Box::pin(futures_util::stream::iter(vec![ev])))
    }
    fn model(&self) -> String {
        "mid".into()
    }
}

/// A row saved MID-TURN must not end up behind the turn's own rows in id order:
/// the store returns id order, the cache append order.
#[tokio::test]
async fn a_turns_rows_are_minted_when_appended_so_id_order_is_append_order() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai) = mk_kernel(&tmp, "midturn").await;
    let provider: Arc<dyn Provider> = Arc::new(MidTurnWriter {
        kernel: Arc::clone(&kernel),
        agent: ai.clone(),
    });
    let out = crate::verbs::send(
        provider,
        &ai,
        &json!({"type": "send", "text": "hello"}),
        &kernel,
        CFG,
    )
    .await
    .expect("send");
    assert!(out.get("error").is_none(), "{out}");
    let hist = fantastic_kernel::test_support::as_agent(
        ai.clone(),
        crate::memory::load_history(&ai, &kernel, false),
    )
    .await
    .expect("store read");
    let ids: Vec<i64> = hist.iter().filter_map(|r| r["id"].as_i64()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "append order diverged from id order");
    assert_eq!(contents(&hist)[..2], [json!("mid"), json!("hello")]);
}

// ── the row event ──────────────────────────────────────────────────────

/// `watch` announces every row the agent appends to the watcher's inbox,
/// content-free without `body`; `unwatch` stops it; a watcher with no inbox is
/// refused.
#[tokio::test]
async fn watch_announces_each_appended_row_until_unwatch() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai) = mk_kernel(&tmp, "watch").await;
    let refused = call(
        &kernel,
        ai.as_str(),
        json!({"type": "watch", "watcher": "nobody"}),
    )
    .await;
    assert_eq!(refused["reason"], json!("no_consumer"), "{refused}");

    let mut rx = kernel.ensure_inbox(&AgentId::from("ui_probe"));
    let w = call(
        &kernel,
        ai.as_str(),
        json!({"type": "watch", "watcher": "ui_probe"}),
    )
    .await;
    assert_eq!(
        w,
        json!({"watching": true, "watcher": "ui_probe", "body": false})
    );
    let ids = fantastic_kernel::test_support::as_agent(
        ai.clone(),
        crate::memory::save_history(&ai, &kernel, &[json!({"role": "user", "content": "hello"})]),
    )
    .await
    .expect("append");
    let ev = rx.try_recv().expect("a row event").payload;
    assert_eq!(
        ev,
        json!({"type": "row", "source": ai.as_str(),
               "row": {"id": ids[0], "kind": "user", "role": "user", "bytes": 5}}),
        "a content-free digest"
    );

    let u = call(
        &kernel,
        ai.as_str(),
        json!({"type": "unwatch", "watcher": "ui_probe"}),
    )
    .await;
    assert_eq!(u["watching"], json!(false), "{u}");
    fantastic_kernel::test_support::as_agent(
        ai.clone(),
        crate::memory::save_history(&ai, &kernel, &[json!({"role": "user", "content": "again"})]),
    )
    .await
    .expect("append");
    assert!(rx.try_recv().is_none(), "no event after unwatch");
}

// ── fail loud: no store, no conversation ───────────────────────────────

/// An AI agent whose record names no `memory_store_id` refuses at boot and fails
/// its turn — nothing falls back to RAM.
#[tokio::test]
async fn an_agent_without_a_memory_store_fails_loud() {
    let tmp = TempDir::new().unwrap();
    let (kernel, ai) = mk_kernel(&tmp, "nostore").await;
    set_meta(&kernel, &ai, json!({"memory_store_id": null})).await;
    let boot = call(&kernel, ai.as_str(), json!({"type": "boot"})).await;
    assert_eq!(boot["reason"], json!("missing_arg"), "{boot}");
    assert_eq!(boot["arg"], json!("memory_store_id"), "{boot}");
    let out = kernel
        .send(vec![Item::new(ai.clone(), json!({"type": "history"}))])
        .await
        .remove(0);
    assert!(
        out.result["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("MemoryStoreError: ") && e.contains("memory_store_id")),
        "{:?}",
        out.result
    );
}
