//! Unit tests for `fantastic-responses-backend` — python
//! `responses_backend/tests/test_responses_provider.py` is the oracle.
//!
//! Wiremock serves the Responses event-typed SSE; the provider is driven
//! directly for the wire and the decoder, and through the kernel for reflect
//! and a full streamed `send`.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every test touching the key var takes this lock and restores what it found.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct KeyEnv {
    prev: Option<String>,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl KeyEnv {
    async fn set(value: Option<&str>) -> Self {
        let guard = ENV_LOCK.lock().await;
        let prev = std::env::var(KEY_VAR).ok();
        match value {
            Some(k) => std::env::set_var(KEY_VAR, k),
            None => std::env::remove_var(KEY_VAR),
        }
        Self {
            prev,
            _guard: guard,
        }
    }
}

impl Drop for KeyEnv {
    fn drop(&mut self) {
        match &self.prev {
            Some(x) => std::env::set_var(KEY_VAR, x),
            None => std::env::remove_var(KEY_VAR),
        }
    }
}

fn sse(events: &[Value]) -> String {
    let mut s = String::new();
    for e in events {
        let ty = e.get("type").and_then(Value::as_str).unwrap_or("event");
        s.push_str(&format!(
            "event: {ty}\ndata: {}\n\n",
            serde_json::to_string(e).unwrap()
        ));
    }
    s
}

fn done(status: &str, reason: Option<&str>, input_tokens: i64) -> Value {
    let mut resp = json!({"status": status, "usage": {"input_tokens": input_tokens}});
    if let Some(r) = reason {
        resp["incomplete_details"] = json!({"reason": r});
    }
    let ty = if status == "completed" {
        "response.completed"
    } else {
        "response.incomplete"
    };
    json!({"type": ty, "response": resp})
}

fn text(chunks: &[&str], status: &str, reason: Option<&str>) -> String {
    let mut evts = vec![
        json!({"type": "response.created", "response": {"status": "in_progress"}}),
        json!({"type": "response.output_item.added", "item": {"type": "message"}}),
    ];
    for c in chunks {
        evts.push(json!({"type": "response.output_text.delta", "delta": c}));
    }
    evts.push(done(status, reason, 42));
    sse(&evts)
}

fn provider(endpoint: String, max_tokens: i64, sampling: Map<String, Value>) -> ResponsesProvider {
    ResponsesProvider {
        self_id: AgentId::from(format!("resp_{}", endpoint.len()).as_str()),
        endpoint,
        model: "qwen-test".to_string(),
        max_tokens,
        sampling,
        report: ReportCell::default(),
    }
}

fn msgs() -> Vec<Value> {
    vec![
        json!({"role": "system", "content": "be terse"}),
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "hello"}),
        json!({"role": "user", "content": "again"}),
    ]
}

/// Drain a pass: the joined text, or the first error's message.
async fn drain(p: &ResponsesProvider, max_tokens: Option<i64>) -> Result<String, String> {
    let mut s = p.chat(&msgs(), max_tokens).await.map_err(|e| e.message)?;
    let mut out = String::new();
    while let Some(ev) = s.next().await {
        match ev {
            Ok(ProviderEvent::Token(t)) => out.push_str(&t),
            Err(e) => return Err(e.message),
        }
    }
    Ok(out)
}

async fn serve(body: String) -> (MockServer, String) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let ep = format!("{}/v1/responses", server.uri());
    (server, ep)
}

/// Decode a whole body through the line decoder; the text, then the verdict.
fn decode_all(body: &str, bounded: bool) -> (String, Option<String>, ReportCell) {
    let report = ReportCell::default();
    let mut st = SseState::new(Arc::clone(&report), bounded);
    let mut out = String::new();
    for line in body.lines() {
        match decode_sse_line(&mut st, line) {
            Ok(evs) => {
                for ProviderEvent::Token(t) in evs {
                    out.push_str(&t);
                }
            }
            Err(e) => return (out, Some(e), report),
        }
        if st.stop {
            break;
        }
    }
    if let Some(m) = st.wrap.close_marker() {
        out.push_str(m);
    }
    let verdict = st.finish();
    (out, verdict, report)
}

#[test]
fn readme_is_python_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join(
            "python/bundled_agents/ai/responses/responses_backend/src/responses_backend/readme.md",
        );
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

#[test]
fn to_responses_matches_python() {
    let (ins, items) = to_responses(&[
        json!({"role": "system", "content": "first"}),
        json!({"role": "system", "content": "last wins"}),
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": ""}),
        json!({"role": "tool", "content": "dropped"}),
    ]);
    assert_eq!(ins, Some(json!("last wins")));
    assert_eq!(
        items,
        vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "(no content)"}]}),
        ]
    );
    let (ins, _) = to_responses(&[json!({"role": "system", "content": ""})]);
    assert_eq!(
        ins, None,
        "a falsy system block means no `instructions` key"
    );
}

#[tokio::test]
async fn request_is_stateless_typed_items_no_tools_in_pythons_key_order() {
    let _key = KeyEnv::set(Some("sk-test")).await;
    let (server, ep) = serve(text(&["ok"], "completed", None)).await;
    let mut sampling = Map::new();
    sampling.insert("temperature".into(), json!(0.2));
    let p = provider(ep.clone(), 900, sampling);
    assert_eq!(drain(&p, None).await.unwrap(), "ok");
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer sk-test"
    );
    let body: Map<String, Value> = serde_json::from_slice(&reqs[0].body).unwrap();
    let keys: Vec<&str> = body.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "model",
            "input",
            "max_output_tokens",
            "stream",
            "store",
            "instructions",
            "temperature"
        ]
    );
    assert_eq!(body["max_output_tokens"], 900);
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "be terse");
    assert_eq!(body["input"].as_array().unwrap().len(), 3);
    assert!(!body.contains_key("tools"));
    // a side call's own bound wins
    drain(&p, Some(64)).await.unwrap();
    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(body["max_output_tokens"], 64);
}

#[test]
fn thinking_is_wrapped_not_dropped() {
    let body = sse(&[
        json!({"type": "response.reasoning_text.delta", "delta": "let me "}),
        json!({"type": "response.reasoning_text.delta", "delta": "think"}),
        json!({"type": "response.output_text.delta", "delta": "answer"}),
        done("completed", None, 7),
    ]);
    let (out, verdict, report) = decode_all(&body, false);
    assert_eq!(out, "<think>let me think</think>answer");
    assert_eq!(verdict, None);
    assert_eq!(report.lock().unwrap().prompt_tokens, Some(7));
    assert_eq!(
        report.lock().unwrap().stop_reason.as_deref(),
        Some("completed")
    );
}

#[test]
fn a_reasoning_summary_is_the_thought_channel_too() {
    let body = sse(&[
        json!({"type": "response.reasoning_summary_text.delta", "delta": "sum"}),
        json!({"type": "response.output_text.delta", "delta": "out"}),
        done("completed", None, 1),
    ]);
    assert_eq!(decode_all(&body, false).0, "<think>sum</think>out");
}

#[test]
fn all_thought_turn_still_closes_the_wrapper() {
    let body = sse(&[
        json!({"type": "response.reasoning_text.delta", "delta": "hm"}),
        done("completed", None, 1),
    ]);
    assert_eq!(decode_all(&body, false).0, "<think>hm</think>");
}

#[test]
fn a_cut_main_turn_is_an_error_and_a_bounded_cut_an_answer() {
    let body = text(&["half a "], "incomplete", Some("max_output_tokens"));
    let (_, verdict, report) = decode_all(&body, false);
    let why = verdict.expect("an unbounded cut is an error");
    assert!(
        why.contains("without completing") && why.contains("max_output_tokens"),
        "{why}"
    );
    assert_eq!(
        report.lock().unwrap().stop_reason.as_deref(),
        Some("max_tokens")
    );
    let (out, verdict, report) = decode_all(&body, true);
    assert_eq!((out.as_str(), verdict), ("half a ", None));
    assert_eq!(
        report.lock().unwrap().stop_reason.as_deref(),
        Some("max_tokens")
    );
}

#[test]
fn no_terminal_event_is_a_cut_connection() {
    let body = sse(&[json!({"type": "response.output_text.delta", "delta": "x"})]);
    let why = decode_all(&body, false).1.expect("error");
    assert!(why.contains("connection cut mid-stream"), "{why}");
}

#[test]
fn response_failed_and_a_native_function_call_are_loud() {
    let body = sse(&[
        json!({"type": "response.failed", "response": {"error": {"message": "upstream died"}}}),
    ]);
    let why = decode_all(&body, false).1.expect("error");
    assert!(why.contains("upstream died"), "{why}");
    let body = sse(&[
        json!({"type": "response.output_item.added", "item": {"type": "function_call", "name": "send"}}),
    ]);
    let why = decode_all(&body, false).1.expect("error");
    assert!(
        why.contains("native function_call") && why.contains("<call name="),
        "{why}"
    );
}

#[tokio::test]
async fn missing_key_endpoint_or_model_fail_loud_naming_it() {
    let _key = KeyEnv::set(None).await;
    let p = provider("http://127.0.0.1:1/v1/responses".into(), 16, Map::new());
    assert!(drain(&p, None).await.unwrap_err().contains(KEY_VAR));
    std::env::set_var(KEY_VAR, "sk");
    let p = provider(String::new(), 16, Map::new());
    assert!(drain(&p, None)
        .await
        .unwrap_err()
        .contains("no `endpoint` set"));
    let mut p = provider("http://127.0.0.1:1/v1/responses".into(), 16, Map::new());
    p.model = String::new();
    assert!(drain(&p, None)
        .await
        .unwrap_err()
        .contains("no `model` set"));
}

#[tokio::test]
async fn transport_retry_is_bounded_and_deliberate_answers_are_not_retried() {
    let _key = KeyEnv::set(Some("sk")).await;
    // a CDN error page twice, then success
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(521)
                .insert_header("content-type", "text/html")
                .set_body_string("<!DOCTYPE html>"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(text(&["recovered"], "completed", None)),
        )
        .mount(&server)
        .await;
    let p = provider(format!("{}/v1/responses", server.uri()), 16, Map::new());
    assert_eq!(drain(&p, None).await.unwrap(), "recovered");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    // a JSON 404 is the endpoint answering: one request, loud
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(404)
                .insert_header("content-type", "application/json")
                .set_body_string("{\"error\": \"not found\"}"),
        )
        .mount(&server)
        .await;
    let p = provider(format!("{}/v1/responses", server.uri()), 16, Map::new());
    assert!(drain(&p, None).await.unwrap_err().contains("responses 404"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// A kernel with the ram store and one responses agent pointed at `endpoint`.
async fn mk_kernel(tmp: &tempfile::TempDir, endpoint: &str) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ResponsesBundle);
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
    let id = format!(
        "resp_{}",
        tmp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('.', "_")
    );
    let memory =
        fantastic_ram_store::test_support::memory_meta(&kernel, &id, json!({"cli": "*"})).await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": id,
        "model": "test-model",
        "endpoint": endpoint,
    });
    for (k, v) in memory {
        create[k] = v;
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await;
    (kernel, AgentId::from(id.as_str()))
}

#[tokio::test]
async fn reflect_sentence_is_python_verbatim() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, "http://127.0.0.1:1/v1/responses").await;
    let r = kernel
        .send(vec![Item::new(id, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["sentence"], SENTENCE);
    assert_eq!(r["model"], "test-model");
    assert_eq!(r["endpoint"], "http://127.0.0.1:1/v1/responses");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_streams_tokens_raw_on_both_channels() {
    let server = MockServer::start().await;
    let body = sse(&[
        json!({"type": "response.created", "response": {"status": "in_progress"}}),
        json!({"type": "response.reasoning_text.delta", "delta": "pondering"}),
        json!({"type": "response.output_text.delta", "delta": "hel"}),
        json!({"type": "response.output_text.delta", "delta": "lo"}),
        done("completed", None, 12),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer sk-x"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, &format!("{}/v1/responses", server.uri())).await;
    let _key = KeyEnv::set(Some("sk-x")).await;
    let mut rx = kernel.ensure_inbox(&id);
    let k2 = Arc::clone(&kernel);
    let id2 = id.clone();
    let join = tokio::spawn(async move {
        k2.send(vec![Item::new(
            id2,
            json!({"type": "send", "text": "hi", "client_id": "c"}),
        )])
        .await
        .remove(0)
        .result
    });
    let mut tokens = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => {
                let ev = ev.payload;
                if ev["type"] == "token" {
                    tokens.push_str(ev["text"].as_str().unwrap_or(""));
                } else if ev["type"] == "status" && ev["phase"] == "done" {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let r = join.await.unwrap();
    assert_eq!(
        tokens, "<think>pondering</think>hello",
        "streamed raw, both channels"
    );
    // python `_send` returns the RAW text (`response: last_text`); only the stored
    // assistant row is stripped — ai-core's business, not this provider's.
    assert_eq!(r["response"], "<think>pondering</think>hello", "{r}");
}

/// `endpoint` is REQUIRED on the record — a send without one raises python's
/// `ValueError`, naming the field (audit 2026-09-28, c9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_without_an_endpoint_raises_naming_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, "").await;
    let r = kernel
        .send(vec![Item::new(
            id,
            json!({"type": "send", "text": "hi", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!(
            "ValueError: responses_backend: `endpoint` is required on the agent record — \
             nothing is guessed (update_agent <id> endpoint=…)"
        )
    );
}

/// `store:false` keeps the conversation ours and `model` is the record's — a
/// `sampling` key naming either is refused, never merged over it (audit d8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampling_may_not_override_what_the_provider_owns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, "http://127.0.0.1:1/v1/responses").await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": id.as_str(),
                   "sampling": {"store": true, "temperature": 0.1, "model": "x"}}),
        )])
        .await;
    let r = kernel
        .send(vec![Item::new(
            id,
            json!({"type": "send", "text": "hi", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"],
        json!(
            "ValueError: responses_backend: `sampling` may not set model, store — those \
             are the record's (`model`, `max_tokens`) or the provider's own"
        ),
        "{r}"
    );
}
