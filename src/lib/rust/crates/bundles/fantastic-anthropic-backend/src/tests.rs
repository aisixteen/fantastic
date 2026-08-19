//! Unit tests for `fantastic-anthropic-backend`.
//!
//! Wiremock spins up a real HTTP server emitting Anthropic event-typed
//! SSE; we drive the bundle via `kernel.send` and assert on the
//! file-agent-routed sidecars + the per-client inbox. The Anthropic
//! wire shape (event-typed SSE, `tool_use` blocks, `input_json_delta`
//! aggregation, `x-api-key` auth) is what this crate adds over ai-core.

use super::*;
use fantastic_kernel::Agent;
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
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, AnthropicBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let ant = ant_id_for(tmp);
    let fid = format!("ff_{}", ant);
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type":"create_agent",
                "handler_module":"file_bridge.tools",
                "id": fid,
                "root": tmp.path().join(".fantastic").to_string_lossy(),
                "ingress_rule": "allow_all",
            }),
        )
        .await;
    let mut create = json!({
        "type":"create_agent",
        "handler_module":HANDLER_MODULE,
        "id": ant,
        "file_bridge_id": fid,
        "model": "test-model",
    });
    if let Some(e) = endpoint {
        create["endpoint"] = json!(e);
    }
    kernel.send(&AgentId::from("core"), create).await;
    (
        kernel,
        AgentId::from(ant.as_str()),
        AgentId::from(fid.as_str()),
    )
}

/// Register a synthetic client inbox so we can drain events emitted by
/// the bundle via `to_caller`.
fn open_client_inbox(kernel: &Arc<Kernel>, client_id: &str) -> tokio::sync::mpsc::Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    kernel.inboxes.insert(AgentId::from(client_id), tx);
    rx
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
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("anthropic_backend"));
}

#[test]
fn translate_messages_splits_system_and_text_turns() {
    // RAW: tool calls and their replies are inline TEXT, never native `tool_use` /
    // `tool_result` blocks — so translation is pure text turns and this transport
    // learns nothing about the tool protocol.
    //
    // A `role:tool` turn is not something ai-core produces: routing delivers a
    // reply as an ordinary `user` turn tagged `[reply to <id>]`. It arrives only as
    // a LEGACY turn in an old store or from a caller-supplied `messages` override,
    // and ai-core maps it to `role:user` before we see it — mapped again here
    // because many chat templates render no `tool` role at all.
    let messages = vec![
        json!({"role":"system","content":"sys-a"}),
        json!({"role":"system","content":"sys-b"}),
        json!({"role":"user","content":"hi"}),
        json!({"role":"assistant","content":"thinking <tool_call>{\"name\":\"send\",\"arguments\":{\"target_id\":\"core\",\"payload\":{\"type\":\"list_agents\"}}}</tool_call>"}),
        json!({"role":"tool","content":"<tool_response name=\"send\">[]</tool_response>"}),
    ];
    let (system, out) = translate_messages(&messages);
    assert_eq!(system.as_deref(), Some("sys-a\n\nsys-b"));
    assert_eq!(out.len(), 3);
    assert_eq!(out[0]["role"], "user");
    // assistant turn → a plain TEXT turn carrying the <tool_call> inline.
    assert_eq!(out[1]["role"], "assistant");
    assert!(out[1]["content"].as_str().unwrap().contains("<tool_call>"));
    // role:tool → a plain user TEXT turn carrying the <tool_response>.
    assert_eq!(out[2]["role"], "user");
    assert!(out[2]["content"]
        .as_str()
        .unwrap()
        .contains("<tool_response"));
}

#[tokio::test]
async fn reflect_shape_has_api_key_false_initially() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel.send(&ant, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], ant.as_str());
    assert_eq!(r["has_api_key"], false);
    assert_eq!(r["model"], "test-model");
    assert_eq!(r["max_tokens"], DEFAULT_MAX_TOKENS);
    assert!(r["verbs"]["set_api_key"].is_string());
    assert!(r["verbs"]["clear_api_key"].is_string());
}

#[tokio::test]
async fn set_api_key_persists_via_file_agent_and_reflect_flips() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(
            &ant,
            json!({"type": "set_api_key", "api_key": "sk-ant-test-abc"}),
        )
        .await;
    assert_eq!(r["ok"], true);
    let key_file = tmp
        .path()
        .join(format!(".fantastic/agents/{}/api_key", ant));
    assert!(key_file.exists(), "api_key sidecar should exist");
    let content = std::fs::read_to_string(&key_file).unwrap();
    assert_eq!(content.trim(), "sk-ant-test-abc");
    let r2 = kernel.send(&ant, json!({"type": "reflect"})).await;
    assert_eq!(r2["has_api_key"], true);
    let serialized = serde_json::to_string(&r2).unwrap();
    assert!(
        !serialized.contains("sk-ant-test-abc"),
        "reflect must never include the api_key value"
    );
}

#[tokio::test]
async fn clear_api_key_deletes_sidecar() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    kernel
        .send(&ant, json!({"type": "set_api_key", "api_key": "k1"}))
        .await;
    let key_file = tmp
        .path()
        .join(format!(".fantastic/agents/{}/api_key", ant));
    assert!(key_file.exists());
    let r = kernel.send(&ant, json!({"type": "clear_api_key"})).await;
    assert_eq!(r["ok"], true);
    assert_eq!(r["deleted"], true);
    assert!(!key_file.exists());
    let r2 = kernel.send(&ant, json!({"type": "reflect"})).await;
    assert_eq!(r2["has_api_key"], false);
}

#[tokio::test]
async fn send_without_api_key_returns_clean_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, ant, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(
            &ant,
            json!({"type": "send", "text": "hi", "client_id": "test"}),
        )
        .await;
    let err = r["error"].as_str().unwrap_or("");
    assert!(
        err.contains("api_key not set"),
        "expected api_key-not-set error, got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_streams_tokens_via_event_typed_sse() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    kernel
        .send(&ant, json!({"type": "set_api_key", "api_key": "sk-ant-x"}))
        .await;

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
    let mut rx = open_client_inbox(&kernel, client_id);
    let send_kernel = Arc::clone(&kernel);
    let ant_for_task = ant.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(
                &ant_for_task,
                json!({"type": "send", "text": "hi", "client_id": client_id}),
            )
            .await
    });

    let mut tokens: Vec<String> = Vec::new();
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline && !saw_done {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => {
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
    // A `<tool_call>` split across two `text_delta`s must REASSEMBLE and dispatch.
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
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, ant, fid) = mk_kernel(&tmp, Some(endpoint)).await;
    kernel
        .send(&ant, json!({"type": "set_api_key", "api_key": "sk-ant-x"}))
        .await;

    // Address the anthropic agent's OWN file agent: its write lands on disk, which
    // is proof the dispatch happened and not just that nothing crashed.
    let whole = format!(
        "<tool_call>{{\"name\":\"send\",\"arguments\":{{\"target_id\":\"{fid}\",\
         \"payload\":{{\"type\":\"write\",\"path\":\"routed.txt\",\"content\":\"ok\"}},\
         \"reply_to\":null}}}}</tool_call>"
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
    let _rx = open_client_inbox(&kernel, client_id);
    let reply = kernel
        .send(
            &ant,
            json!({"type": "send", "text": "do it", "client_id": client_id}),
        )
        .await;
    assert_eq!(reply["response"], whole, "verbatim text: {reply:#?}");

    let routed = tmp.path().join(".fantastic").join("routed.txt");
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
    assert_eq!(std::fs::read_to_string(&routed).unwrap(), "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_retry_once_then_succeeds() {
    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, ant, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    kernel
        .send(&ant, json!({"type": "set_api_key", "api_key": "sk-ant-x"}))
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("rate limited"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let body = sse(&[
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":0}),
        // The real API always sends stop_reason before message_stop; a mock without
        // it now reads as a cut stream (the provider raises), which is the point.
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
        .expect(1)
        .mount(&server)
        .await;

    let client_id = "test_rl";
    let mut rx = open_client_inbox(&kernel, client_id);
    let send_kernel = Arc::clone(&kernel);
    let ant_for_task = ant.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(
                &ant_for_task,
                json!({"type": "send", "text": "go", "client_id": client_id}),
            )
            .await
    });

    let mut saw_rate_status = false;
    let mut tokens = String::new();
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !saw_done {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => {
                let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "token" => {
                        if let Some(t) = ev.get("text").and_then(Value::as_str) {
                            tokens.push_str(t);
                        }
                    }
                    "status"
                        if ev
                            .get("detail")
                            .and_then(|d| d.get("waiting_on"))
                            .and_then(Value::as_str)
                            == Some("rate_limit") =>
                    {
                        saw_rate_status = true;
                    }
                    "done" => saw_done = true,
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let r = join.await.unwrap();
    assert!(
        saw_rate_status,
        "expected status(waiting_on=rate_limit) event"
    );
    assert_eq!(tokens, "ok");
    assert_eq!(r["response"], "ok");
}
