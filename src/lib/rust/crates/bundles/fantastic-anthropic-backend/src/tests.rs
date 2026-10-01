//! Unit tests for `fantastic-anthropic-backend`.
//!
//! Wiremock spins up a real HTTP server emitting Anthropic event-typed
//! SSE; we drive the bundle via `kernel.send` and assert on the
//! file-agent-routed sidecars + the per-client inbox. The Anthropic
//! wire shape (event-typed SSE, `tool_use` blocks, `input_json_delta`
//! aggregation, `x-api-key` auth) is what this crate adds over ai-core.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::Map;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Per-test id derived from the tempdir name. Static maps in this crate
/// are process-global; sharing one id across parallel tests would race.
fn ant_id_for(tmp: &tempfile::TempDir) -> String {
    format!(
        "ant_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

/// Stand up: kernel + file bundle + anthropic bundle + a root agent +
/// a file agent rooted at the workdir + an anthropic agent bound to it.
async fn mk_kernel(
    tmp: &tempfile::TempDir,
    endpoint: Option<String>,
) -> (Arc<Kernel>, AgentId, AgentId) {
    mk_kernel_with(tmp, endpoint, Map::new()).await
}

/// `mk_kernel` plus extra record fields (e.g. the optional `tool_use` route).
async fn mk_kernel_with(
    tmp: &tempfile::TempDir,
    endpoint: Option<String>,
    extra: Map<String, Value>,
) -> (Arc<Kernel>, AgentId, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, AnthropicBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let ant = ant_id_for(tmp);
    let fid = format!("ff_{}", ant);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type":"create_agent",
                "parent_id": "kernel",
                "handler_module":"file_bridge.tools", "allow": ["."],
                "id": fid,
                "root": tmp.path().join(".fantastic").to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the backend that persists through it.
                "callable_by": { ant.clone(): "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    // Deny-all: its memory store (wired both ways) + its leg + `cli`.
    let memory = fantastic_ram_store::test_support::memory_meta(
        &kernel,
        &ant,
        json!({ fid.clone(): "*", "cli": "*" }),
    )
    .await;
    let mut create = json!({
        "type":"create_agent",
        "parent_id": "kernel",
        "handler_module":HANDLER_MODULE,
        "id": ant,
        "model": "test-model",
        // REQUIRED — nothing is guessed; an unreachable one unless a test serves it.
        "endpoint": endpoint.unwrap_or_else(|| "http://127.0.0.1:1/v1/messages".to_string()),
    });
    for (k, v) in memory {
        create[k] = v;
    }
    for (k, v) in extra {
        create[k] = v;
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    (
        kernel,
        AgentId::from(ant.as_str()),
        AgentId::from(fid.as_str()),
    )
}

/// Register a synthetic client inbox so we can drain events emitted by
/// the bundle via `to_caller`.
/// The AGENT's inbox, where per-client events land tagged with their
/// `client_id` — python's routing, and what a `watch` on the agent mirrors. It
/// used to open an inbox named after the CLIENT, which no watcher ever sees.
fn open_client_inbox(
    kernel: &Arc<Kernel>,
    agent_id: &AgentId,
) -> fantastic_kernel::inbox::InboxReceiver {
    kernel.ensure_inbox(agent_id)
}

/// Build an Anthropic event-typed SSE body from `data` objects. Each gets
/// an `event:` line (from its own `type`) + a `data:` JSON line.
fn sse(events: &[Value]) -> String {
    let mut s = String::new();
    for e in events {
        let ty = e.get("type").and_then(Value::as_str).unwrap_or("event");
        s.push_str("event: ");
        s.push_str(ty);
        s.push('\n');
        s.push_str("data: ");
        s.push_str(&serde_json::to_string(e).unwrap());
        s.push_str("\n\n");
    }
    s
}

#[test]
fn readme_is_python_verbatim() {
    // B4.9 — the readme a model reads about this backend is the PYTHON bundle's,
    // byte for byte. Each runtime had written its own, so the same agent
    // described itself differently depending on where it ran.
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join(
            "python/bundled_agents/ai/anthropic/anthropic_backend/src/anthropic_backend/readme.md",
        );
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

#[test]
fn to_anthropic_matches_python() {
    // python `_to_anthropic`: the LAST system message wins, a falsy content on a
    // user/assistant turn becomes `(no content)`, and every other role — including
    // `role:tool` — is DROPPED.
    let messages = vec![
        json!({"role":"system","content":"A"}),
        json!({"role":"user","content":""}),
        json!({"role":"assistant","content":null}),
        json!({"role":"tool","content":"x"}),
        json!({"role":"user","content":"hi"}),
    ];
    let (system, out) = to_anthropic(&messages);
    assert_eq!(system, Some(json!("A")));
    assert_eq!(
        out,
        vec![
            json!({"role":"user","content":"(no content)"}),
            json!({"role":"assistant","content":"(no content)"}),
            json!({"role":"user","content":"hi"}),
        ]
    );
    // A LATER EMPTY SYSTEM OVERRIDES `A` — assignment, not concatenation — and a
    // falsy system block means the body carries no `system` key at all.
    let mut later = messages.clone();
    later.push(json!({"role":"system","content":""}));
    let (system, _) = to_anthropic(&later);
    assert_eq!(system, None);
}

#[test]
fn request_body_key_order_matches_python() {
    // python `AnthropicProvider.chat`: model, max_tokens, messages, stream,
    // system, then the record's sampling knobs merged last.
    let (system, anthropic_messages) = to_anthropic(&[
        json!({"role":"system","content":"S"}),
        json!({"role":"user","content":"hi"}),
    ]);
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!("m"));
    body.insert("max_tokens".into(), json!(8192));
    body.insert("messages".into(), json!(anthropic_messages));
    body.insert("stream".into(), json!(true));
    if let Some(sys) = system {
        body.insert("system".into(), sys);
    }
    body.insert("top_p".into(), json!(0.9));
    assert_eq!(
        body.keys().cloned().collect::<Vec<_>>(),
        vec![
            "model".to_string(),
            "max_tokens".to_string(),
            "messages".to_string(),
            "stream".to_string(),
            "system".to_string(),
            "top_p".to_string(),
        ]
    );
}

/// THE KEY LIVES IN THE ENVIRONMENT, which is process-global — so the test that
/// needs it ABSENT and the ones that need it PRESENT must not overlap. Every
/// test touching the key takes this lock and restores what it found.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Set (or clear) the key var for the duration of a test, restoring on drop.
struct KeyEnv {
    prev: Vec<(&'static str, Option<String>)>,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl KeyEnv {
    async fn set(value: Option<&str>) -> Self {
        let guard = ENV_LOCK.lock().await;
        let var = "ANTHROPIC_API_KEY";
        let prev = vec![(var, std::env::var(var).ok())];
        std::env::remove_var(var);
        if let Some(k) = value {
            std::env::set_var(var, k);
        }
        Self {
            prev,
            _guard: guard,
        }
    }
}

impl Drop for KeyEnv {
    fn drop(&mut self) {
        for (v, old) in &self.prev {
            match old {
                Some(x) => std::env::set_var(v, x),
                None => std::env::remove_var(v),
            }
        }
    }
}

#[tokio::test]
async fn reflect_keys_are_python_keys_in_python_order() {
    // python `_make_reflect` with no `reflect_extra`.
    let _key = KeyEnv::set(None).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(vec![Item::new(ant.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    // The kernel's own reflect frame (`can_call`, `tree`, …) is appended AFTER
    // the bundle's body; what this pins is the bundle's half, in python's order.
    let keys: Vec<String> = r.as_object().unwrap().keys().take(12).cloned().collect();
    assert_eq!(
        keys,
        vec![
            "id".to_string(),
            "sentence".to_string(),
            "model".to_string(),
            "endpoint".to_string(),
            "context_window".to_string(),
            "context_strategy".to_string(),
            "memory_store_id".to_string(),
            "verbs".to_string(),
            "generating".to_string(),
            "cancel".to_string(),
            "emits".to_string(),
            "concurrency".to_string(),
        ],
        "{r:#?}"
    );
    assert_eq!(r["id"], ant.as_str());
    assert_eq!(r["model"], "test-model");
    assert_eq!(r["generating"], false);
    assert_eq!(r["context_strategy"], "compact");
    // The key-storage verbs are GONE — the key is read from the environment and
    // this bundle never persists it.
    assert!(r["verbs"]["set_api_key"].is_null());
    assert!(r["verbs"]["clear_api_key"].is_null());
}

#[tokio::test]
async fn reflect_sentence_is_python_verbatim() {
    let _key = KeyEnv::set(None).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(vec![Item::new(ant, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["sentence"],
        "Anthropic-backed LLM agent (raw prompt-and-parse tool-calling)."
    );
}

/// THE KEY IS THE PROVIDER'S BUSINESS, CHECKED INSIDE THE TURN — python
/// `provider.py:112-119`. The turn opens (`status(thinking)`), the key check
/// raises, `status(done, reason='error')` carries `str(e)`, and the caller reads
/// the kernel's settle of a handler raise. Rust used to refuse BEFORE the
/// provider was built, with a sentence (`anthropic_backend: no api key — set
/// <var>`) python does not have, as a COMPLETED
/// execution with no events at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_key_raises_inside_chat_after_status_thinking() {
    let _key = KeyEnv::set(None).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let mut rx = open_client_inbox(&kernel, &ant);
    let r = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "hi", "client_id": "test"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"],
        json!("RuntimeError: ANTHROPIC_API_KEY not set in environment (.env)")
    );
    assert_eq!(r["raised"], json!(true));

    let mut seen: Vec<(String, Value)> = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        let ev = ev.payload;
        if ev.get("type").and_then(Value::as_str) == Some("status") {
            seen.push((
                ev["phase"].as_str().unwrap_or("").to_string(),
                ev["detail"].clone(),
            ));
        }
    }
    assert_eq!(
        seen.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        vec!["thinking", "done"]
    );
    let done = &seen[1].1;
    assert_eq!(done["reason"], json!("error"));
    assert_eq!(
        done["error"],
        json!("ANTHROPIC_API_KEY not set in environment (.env)")
    );
}

/// The RECORD is checked before the key — now AT THE WRITE (the kind's schema,
/// lib request #3): with no key in the environment, blanking the model is
/// refused naming the model and what it is, and the record keeps its value.
/// The key is read only at `chat`, so it never gets a say.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_model_is_named_before_a_missing_key() {
    let _key = KeyEnv::set(None).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": ant.as_str(), "model": ""}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("invalid_meta"), "{r}");
    assert_eq!(
        r["error"],
        json!(
            "anthropic_backend: required on the record and missing: model — nothing is \
             guessed. model = the model name"
        )
    );
    let model = kernel
        .resolve_target(ant.as_str())
        .and_then(|a| a.record().meta.get("model").cloned());
    assert_ne!(model, Some(json!("")), "the refused write changed nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_streams_tokens_via_event_typed_sse() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    // `endpoint` is the messages URL VERBATIM (py parity) — not a base.
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;

    let body = sse(&[
        json!({"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-x"))
        .and(header("anthropic-version", ANTHROPIC_VERSION))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client_id = "test_client";
    let mut rx = open_client_inbox(&kernel, &ant);
    let send_kernel = Arc::clone(&kernel);
    let ant_for_task = ant.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(vec![Item::new(
                ant_for_task.clone(),
                json!({"type": "send", "text": "hi", "client_id": client_id}),
            )])
            .await
            .remove(0)
            .result
    });

    let mut tokens: Vec<String> = Vec::new();
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline && !saw_done {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => {
                let ev = ev.payload;
                let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
                if ty == "token" {
                    if let Some(t) = ev.get("text").and_then(Value::as_str) {
                        tokens.push(t.to_string());
                    }
                } else if ty == "status" && ev.get("phase").and_then(Value::as_str) == Some("done")
                {
                    // `status(phase:"done")` IS completion — the duplicate bare
                    // `done` event is gone.
                    saw_done = true;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let r = join.await.unwrap();
    assert!(saw_done, "expected a done event on the client inbox");
    assert_eq!(tokens.join(""), "hello", "tokens were {tokens:?}");
    assert_eq!(r["response"], "hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_envelope_split_across_text_deltas_dispatches() {
    // A `<call>` split across two `text_delta`s must REASSEMBLE and dispatch.
    // This test used to assert the opposite (`envelope_text_streams_verbatim_no_dispatch`
    // — "just TEXT, nothing parsed, nothing dispatched"). That was a gap, not a
    // principle: python's ai_core has dispatched this envelope since 2026-08-02 and
    // this crate's own header already said ai-core parses it. The transport's
    // remaining duty is the reassembly + verbatim delivery asserted below —
    // Anthropic frames text in deltas, so a call arriving in pieces is the real
    // failure mode.
    //
    // `reply_to:null` keeps it to ONE pass: the stub answers with the same envelope
    // every time, so a default `reply_to` would (correctly) chain against a mock
    // that never says anything else.
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    // `endpoint` is the messages URL VERBATIM (py parity) — not a base.
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;

    // Address the anthropic agent's OWN file agent: its write lands on disk, which
    // is proof the dispatch happened and not just that nothing crashed.
    let whole = format!(
        "<call name=\"send\"><target_id>{fid}</target_id><reply_to>null</reply_to>\
         <type>mkdir</type><path>routed</path></call>"
    );
    let split = whole.len() / 2;
    let (part_1, part_2) = whole.split_at(split);
    let body = sse(&[
        json!({"type":"message_start","message":{"id":"m1","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":part_1}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":part_2}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(1) // one pass: the call discards its reply, so nothing follows
        .mount(&server)
        .await;

    let client_id = "test_tool";
    let _rx = open_client_inbox(&kernel, &ant);
    let reply = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "do it", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["response"], whole, "verbatim text: {reply:#?}");

    let routed = tmp.path().join(".fantastic").join("routed");
    for _ in 0..250 {
        if routed.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        routed.exists(),
        "the delta-split envelope never dispatched: {routed:?} missing"
    );
}

// ── transport retry: python's policy, and ONLY python's ──────────────
//
// A 4xx — 429 INCLUDED — or any 5xx carrying a JSON body is the endpoint
// answering deliberately: returned as `anthropic <code>: <body>`, never retried,
// and with no event of its own. Rust used to retry a 429 once, announce the wait
// with a `say` + a `status(waiting_on=rate_limit)` event and report the give-up
// as `send: rate limited (429); retry in Ns` — three surfaces python does not
// have. Only a TRANSPORT code with a NON-JSON body is retried: that is a CDN
// error page, meaning the request never reached anything that could judge it.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_429_is_answered_loudly_and_never_retried() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("rate limited"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut rx = open_client_inbox(&kernel, &ant);
    let reply = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "go", "client_id": "test_rl"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        reply["error"],
        json!("RuntimeError: anthropic 429: rate limited")
    );
    assert_eq!(reply["raised"], json!(true));

    // NO rate-limit surface at all: no `say`, no `status(waiting_on)`.
    let mut phases: Vec<String> = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        let ev = ev.payload;
        let ty = ev
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        assert_ne!(ty, "say", "python emits no say for a 429: {ev:#?}");
        if ty == "status" {
            assert!(
                ev.get("detail").and_then(|d| d.get("waiting_on")).is_none(),
                "python emits no rate-limit wait status: {ev:#?}"
            );
            phases.push(
                ev.get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    assert_eq!(phases, vec!["thinking".to_string(), "done".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_json_transport_code_gives_up_after_the_bound() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;

    // Bounded, not infinite — python `test_transport_error_gives_up_after_the
    // _bound`: RETRIES + 1 requests, then the status is returned as an answer.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(521).set_body_raw("<!DOCTYPE html>", "text/html"))
        .expect(u64::from(RETRIES + 1))
        .mount(&server)
        .await;

    let reply = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        reply["error"],
        json!("RuntimeError: anthropic 521: <!DOCTYPE html>")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_json_error_body_is_never_retried() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;

    // The proxy answered deliberately; retrying would spend the same broken
    // request N times and hide the caller's bug — python
    // `test_json_error_body_is_never_retried`.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(503).set_body_raw("{\"type\":\"error\"}", "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let reply = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        reply["error"],
        json!("RuntimeError: anthropic 503: {\"type\":\"error\"}")
    );
}

/// `stop_reason='tool_use'` over nothing but text means the stream did not carry
/// what its stop_reason claims — python
/// `test_tool_use_stop_reason_without_a_tool_use_block_is_an_anomaly`. A bare
/// stop_reason names the symptom and nothing else, which is why the error names
/// the block types it DID see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_use_stop_reason_without_a_tool_use_block_is_an_anomaly() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let body = sse(&[
        json!({"type":"content_block_start","content_block":{"type":"text"}}),
        json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}),
        json!({"type":"content_block_stop"}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let reply = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        reply["error"],
        json!(
            "RuntimeError: anthropic stream ended without end_turn: \
             stop_reason='tool_use' blocks=['text']"
        )
    );
}

/// A CUT stream — no `stop_reason` at all — names that it saw none, python
/// `provider.py`'s ` (none seen — connection cut mid-stream)` tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_stream_says_no_stop_reason_was_seen() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let body = sse(&[
        json!({"type":"content_block_start","content_block":{"type":"text"}}),
        json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let reply = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        reply["error"],
        json!(
            "RuntimeError: anthropic stream ended without end_turn: \
             stop_reason=None blocks=['text'] (none seen — connection cut mid-stream)"
        )
    );
}

// ── the RAW route is the only route ──────────────────────────────────
//
// RAW is the contract, and there is no longer an opt-out. The `tool_use` route
// that declared one `send` tool and translated the blocks back into a text
// envelope is DELETED: the envelope it translated INTO no longer exists, and a
// model writing JSON tool calls is writing a grammar this kernel does not parse.
// Saying so is the honest answer; translating a second dialect at the edge was a
// way of not saying it.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_raw_route_declares_no_tools() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    // `endpoint` is the messages URL VERBATIM (py parity) — not a base.
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let body = sse(&[
        json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let _ = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "hi", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;

    let reqs = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(
        sent.get("tools").is_none(),
        "a native `tools` key went on the wire: {sent}"
    );
}

/// A NATIVE `tool_use` BLOCK IS A LOUD ERROR, not a silent drop. Some
/// `/cc`-style routes emit one for a request carrying no `tools` key at all —
/// and its arguments are JSON this kernel does not read, so a dropped block ends
/// the turn looking finished with the call lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_tool_use_block_is_a_loud_error_not_a_silent_drop() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let body = sse(&[
        json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,
               "content_block":{"type":"tool_use","id":"tu_1","name":"send","input":{}}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let reply = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "do it", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    let err = reply["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("native tool_use block") && err.contains("<call name="),
        "the block was swallowed: {reply:#?}"
    );
}

/// NO guessy default (mirror of the Python provider test): an empty model never
/// reaches the wire — the request path fails loud, naming `model`.
#[tokio::test]
async fn an_empty_model_fails_loud_in_chat() {
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    for m in [String::new()] {
        let p = AnthropicProvider {
            self_id: AgentId::from("ant_x"),
            endpoint: "http://127.0.0.1:1/v1/messages".to_string(),
            model: m,
            max_tokens: 16,
            sampling: Map::new(),
            report: Default::default(),
        };
        let err = match p
            .chat(&[json!({"role": "user", "content": "hi"})], None)
            .await
        {
            Ok(_) => panic!("chat must refuse an unset/sentinel model"),
            Err(e) => e.message,
        };
        assert!(
            err.contains("anthropic_backend: no `model` set") && err.contains("model=..."),
            "unexpected error: {err}"
        );
    }
}

// ── the wire trace ──────────────────────────────────────────────────
//
// PYTHON IS THE ORACLE (`provider.py`): `FANTASTIC_AI_WIRE=1` prints exactly two
// lines on STDOUT — `[wire] -> …` once per `chat`, `[wire] <- …` once per
// completed stream — and NOTHING else. The api key and the headers are never on
// the trace, so nothing here redacts: they are not printed at all.

#[test]
fn the_wire_trace_lines_are_pythons_text() {
    // python `f"[wire] -> max_tokens={body['max_tokens']} tools={len(body.get('tools', []))} msgs={len(anth_messages)}"`
    // — this route declares no `tools`, so the count is python's `[]` default.
    let body = json!({"model": "m", "max_tokens": 4096, "messages": [], "stream": true});
    assert_eq!(
        wire_request_line(&body, 3),
        "[wire] -> max_tokens=4096 tools=0 msgs=3"
    );
    // python `f"[wire] <- stop_reason={stop_reason!r} blocks={blocks!r}"` — a
    // stream that carried nothing prints python's `None` and `[]`.
    let mut st = SseState::new(Default::default(), false);
    assert_eq!(wire_reply_line(&st), "[wire] <- stop_reason=None blocks=[]");
    st.stop_reason = Some("end_turn".to_string());
    st.blocks = vec![json!("thinking"), json!("text")];
    assert_eq!(
        wire_reply_line(&st),
        "[wire] <- stop_reason='end_turn' blocks=['thinking', 'text']"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wire_trace_prints_only_when_the_var_is_1() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1/messages", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    // `KeyEnv` holds the process-global env lock — the wire var rides with it.
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let prev = std::env::var("FANTASTIC_AI_WIRE").ok();
    std::env::remove_var("FANTASTIC_AI_WIRE");
    assert!(!wire_on(), "unset prints nothing");
    std::env::set_var("FANTASTIC_AI_WIRE", "0");
    assert!(!wire_on(), "only `1` prints — python `== \"1\"`");
    std::env::set_var("FANTASTIC_AI_WIRE", "1");
    assert!(wire_on());

    let body = sse(&[
        json!({"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    // The traced path is the ORDINARY path: tracing changes no reply.
    let r = kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "hi", "client_id": "test_client"}),
        )])
        .await
        .remove(0)
        .result;
    match prev {
        Some(x) => std::env::set_var("FANTASTIC_AI_WIRE", x),
        None => std::env::remove_var("FANTASTIC_AI_WIRE"),
    }
    assert_eq!(r["response"], "hi");
}

/// PROVIDER-REPORTED USAGE: `message_start.usage` — input tokens with the cache
/// reads and writes, what the window really held — calibrates ai_core's
/// estimate; a BOUNDED side call that stopped at `max_tokens` is an ANSWER (no
/// terminality error) and reports the stop reason for the fold to read.
#[test]
fn message_start_usage_and_a_bounded_cut_are_reported() {
    let report = fantastic_ai_core::provider::ReportCell::default();
    let mut st = SseState::new(std::sync::Arc::clone(&report), true);
    for ev in [
        json!({"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cut"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"}}),
        json!({"type":"message_stop"}),
    ] {
        decode_sse_line(&mut st, &format!("data: {ev}")).expect("decodes");
    }
    assert_eq!(report.lock().unwrap().prompt_tokens, Some(123));
    assert_eq!(st.finish(), None, "a bounded cut is an answer");
    assert_eq!(
        report.lock().unwrap().stop_reason.as_deref(),
        Some("max_tokens")
    );
    // unbounded, the same cut is a terminality error
    let mut st = SseState::new(Default::default(), false);
    decode_sse_line(
        &mut st,
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}",
    )
    .expect("decodes");
    assert!(st.finish().is_some());
}

// ─── STEP-4 PINS (2026-10-01): a raw HTTP/1.1 stub ──────────────────────────
//
// The mock-server crates always answer with a COMPLETE body; the behaviours
// pinned below need a body CUT mid-stream, the raw request head (headers as
// sent), and a count of TCP connections (the client must not pool).

/// What the stub saw: every request head, in order, and how many connections.
#[derive(Clone, Default)]
struct RawLog {
    heads: Arc<std::sync::Mutex<Vec<String>>>,
    conns: Arc<std::sync::atomic::AtomicUsize>,
}

impl RawLog {
    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
    fn conns(&self) -> usize {
        self.conns.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Answer request `n` (counted across connections) with `script(n)`: the raw
/// bytes to write and whether to close the connection after them. A connection
/// left open serves its next request (HTTP/1.1 keep-alive).
async fn raw_stub(script: fn(usize) -> (Vec<u8>, bool)) -> (u16, RawLog) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = RawLog::default();
    let srv = log.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            srv.conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let log = srv.clone();
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let head = loop {
                        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                        if let Some(h) = end {
                            let head = String::from_utf8_lossy(&buf[..h]).to_string();
                            let len: usize = head
                                .to_ascii_lowercase()
                                .split("content-length:")
                                .nth(1)
                                .and_then(|t| t.split("\r\n").next())
                                .and_then(|t| t.trim().parse().ok())
                                .unwrap_or(0);
                            if buf.len() >= h + len {
                                buf.drain(..h + len);
                                break head;
                            }
                        }
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let n = {
                        let mut heads = log.heads.lock().unwrap();
                        heads.push(head);
                        heads.len() - 1
                    };
                    let (out, close) = script(n);
                    let _ = sock.write_all(&out).await;
                    let _ = sock.flush().await;
                    if close {
                        let _ = sock.shutdown().await;
                        return;
                    }
                }
            });
        }
    });
    (port, log)
}

/// A 200 whose `Content-Length` promises far more than `body` — the client reads
/// `body`, then the connection closes: a transport error MID-STREAM.
fn cut_response(content_type: &str, body: &str) -> (Vec<u8>, bool) {
    (
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len() + 4096
        )
        .into_bytes(),
        true,
    )
}

/// A complete 200, connection kept alive.
fn full_response(content_type: &str, body: &str) -> (Vec<u8>, bool) {
    (
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
        false,
    )
}

/// The value of header `name` (case-insensitive) in a raw request head, as sent.
fn raw_header(head: &str, name: &str) -> Option<String> {
    let want = format!("{}:", name.to_ascii_lowercase());
    head.split("\r\n").find_map(|l| {
        l.to_ascii_lowercase().starts_with(&want).then(|| {
            l[want.len()..]
                .strip_prefix(' ')
                .unwrap_or(&l[want.len()..])
                .to_string()
        })
    })
}

async fn send_once(kernel: &Arc<Kernel>, ant: &AgentId) -> Value {
    kernel
        .send(vec![Item::new(
            ant.clone(),
            json!({"type": "send", "text": "go", "client_id": "t", "reply_to": null}),
        )])
        .await
        .remove(0)
        .result
}

const DONE_OK: &str = "ok<call name=\"done\"></call>";

fn ok_body() -> String {
    sse(&[
        json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":DONE_OK}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ])
}

/// A body cut mid-stream raises `stream: <reqwest's text ← every cause>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_cut_mid_stream_raises_stream_and_the_cause_chain() {
    let (port, _log) = raw_stub(|_| {
        cut_response(
            "text/event-stream",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"text\"}}\n\n",
        )
    })
    .await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) =
        mk_kernel(&tmp, Some(format!("http://127.0.0.1:{port}/v1/messages"))).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let r = send_once(&kernel, &ant).await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!("RuntimeError: stream: error decoding response body ← error reading a body from connection ← end of file before message length reached"),
        "{r}"
    );
}

/// The key rides VERBATIM (python reads `os.environ` as is — never trimmed) as
/// `x-api-key`, with `anthropic-version` and `Accept: text/event-stream`; the
/// cached client never pools: every request dials anew.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_key_rides_verbatim_and_no_connection_is_pooled() {
    let (port, log) = raw_stub(|_| full_response("text/event-stream", &ok_body())).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) =
        mk_kernel(&tmp, Some(format!("http://127.0.0.1:{port}/v1/messages"))).await;
    let _key = KeyEnv::set(Some(" sk-ant-pad ")).await;
    assert_eq!(send_once(&kernel, &ant).await["response"], json!(DONE_OK));
    assert_eq!(send_once(&kernel, &ant).await["response"], json!(DONE_OK));
    let heads = log.heads();
    assert!(heads.len() >= 2, "{heads:#?}");
    for h in &heads {
        assert!(h.starts_with("POST /v1/messages HTTP/1.1\r\n"), "{h}");
        assert_eq!(
            raw_header(h, "x-api-key").as_deref(),
            Some(" sk-ant-pad "),
            "{h}"
        );
        assert_eq!(
            raw_header(h, "anthropic-version").as_deref(),
            Some(ANTHROPIC_VERSION)
        );
        assert_eq!(
            raw_header(h, "accept").as_deref(),
            Some("text/event-stream")
        );
    }
    assert_eq!(
        log.conns(),
        heads.len(),
        "pool_max_idle_per_host(0): one connection per request"
    );
}

/// `on_delete` drops the agent's cached client: an agent re-created under the
/// same id builds a new one with the key the environment holds NOW.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_drops_the_cached_client() {
    let (port, log) = raw_stub(|_| full_response("text/event-stream", &ok_body())).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("http://127.0.0.1:{port}/v1/messages");
    let _key = KeyEnv::set(Some("sk-first")).await;
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint.clone())).await;
    assert_eq!(send_once(&kernel, &ant).await["response"], json!(DONE_OK));
    let gone = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": ant.as_str()}),
        )])
        .await
        .remove(0)
        .result;
    assert!(gone.get("error").is_none(), "{gone}");
    std::env::set_var("ANTHROPIC_API_KEY", "sk-second");
    let (kernel2, ant2, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    assert_eq!(ant2, ant, "the same id");
    let before = log.heads().len();
    assert_eq!(send_once(&kernel2, &ant2).await["response"], json!(DONE_OK));
    assert_eq!(
        raw_header(&log.heads()[before], "x-api-key").as_deref(),
        Some("sk-second"),
        "a deleted agent's client is gone"
    );
}

/// A 5xx from a gateway (non-JSON body) is retried with python's stdout line;
/// the SAME status with a JSON body is the API speaking and is raised at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_json_502_is_raised_verbatim_without_a_retry() {
    let (port, log) = raw_stub(|_| {
        (
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"error\":\"no\"}\n"
                .to_vec(),
            false,
        )
    })
    .await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) =
        mk_kernel(&tmp, Some(format!("http://127.0.0.1:{port}/v1/messages"))).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let r = send_once(&kernel, &ant).await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!("RuntimeError: anthropic 502: {\"error\":\"no\"}\n"),
        "{r}"
    );
    assert_eq!(log.heads().len(), 1, "never retried");
}

/// The unknown-type reply, as TEXT — a bare `{error}`; key order is wire.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(vec![Item::new(ant, json!({"type": "frob"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"anthropic_backend: unknown type 'frob'"}"#
    );
}

/// STEP-4 PIN (2026-10-01): BOTH text channels are read — a `thinking_delta`
/// opens the reasoning span once, the first `text_delta` closes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_then_text_is_wrapped_once() {
    use fantastic_ai_core::think::{THINK_CLOSE, THINK_OPEN};
    let body = sse(&[
        json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" ok"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":DONE_OK}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(format!("{}/v1/messages", server.uri()))).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let r = send_once(&kernel, &ant).await;
    assert_eq!(
        r["response"],
        json!(format!("{THINK_OPEN}hm ok{THINK_CLOSE}{DONE_OK}")),
        "{r}"
    );
}

/// STEP-4 PIN (2026-10-01): an `error` event raises the WHOLE error object,
/// python-rendered (`str(dict)`), under `anthropic stream error:`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_event_raises_the_whole_object_python_rendered() {
    let body = sse(&[
        json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}),
        json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}),
    ]);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(format!("{}/v1/messages", server.uri()))).await;
    let _key = KeyEnv::set(Some("sk-ant-x")).await;
    let r = send_once(&kernel, &ant).await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!(
            "RuntimeError: anthropic stream error: {'type': 'overloaded_error', 'message': 'busy'}"
        ),
        "{r}"
    );
}
