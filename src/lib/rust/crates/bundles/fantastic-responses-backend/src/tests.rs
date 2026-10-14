//! Unit tests for `fantastic-responses-backend` — python
//! `responses_backend/tests/test_responses_provider.py` is the oracle.
//!
//! Wiremock serves the Responses event-typed SSE; the provider is driven
//! directly for the wire and the decoder, and through the kernel for reflect
//! and a full streamed `send`.

use super::*;
use fantastic_ai_core::credentials::{CredentialError, KeySource};
use fantastic_credential_store::test_support as creds;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The key reader a provider awaits before each request (in the kernel, the
/// record's `credentials` read from a credential store — python's `_key`).
struct FixedKey(&'static str);

#[async_trait]
impl KeySource for FixedKey {
    async fn key(&self, _endpoint: &str) -> Result<Option<String>, CredentialError> {
        Ok(Some(self.0.to_string()))
    }
}

/// python's `_refused` reader.
struct Refused;

#[async_trait]
impl KeySource for Refused {
    async fn key(&self, _endpoint: &str) -> Result<Option<String>, CredentialError> {
        Err(CredentialError(
            "responses_backend: credentials creds/openai: no such ref".to_string(),
        ))
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
        key: Arc::new(FixedKey("sk-test")),
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
    let mut p = provider("http://127.0.0.1:1/v1/responses".into(), 16, Map::new());
    p.key = Arc::new(Refused);
    let e = match p.chat(&msgs(), None).await {
        Err(e) => e,
        Ok(_) => panic!("a refused key must fail the pass"),
    };
    assert_eq!(
        e.message,
        "responses_backend: credentials creds/openai: no such ref"
    );
    assert_eq!(e.kind, "CredentialError");
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

/// The ref the agent's record names in the test credential store.
const KEY_REF: &str = "openai";

/// Store (or rotate) the agent's key, bound to the host of its endpoint.
async fn set_key(kernel: &Arc<Kernel>, tmp: &tempfile::TempDir, id: &AgentId, key: &str) {
    let endpoint = fantastic_ai_core::helpers::agent_meta(id, kernel)["endpoint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    creds::set_key(kernel, tmp.path(), id.as_str(), KEY_REF, &endpoint, key).await;
}

/// A kernel with the ram store, the credential store, and one responses agent
/// pointed at `endpoint` whose `credentials` name the store's `openai` ref.
async fn mk_kernel(tmp: &tempfile::TempDir, endpoint: &str) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ResponsesBundle);
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    kernel
        .bundles
        .register(fantastic_credential_store::HANDLER_MODULE, creds::bundle());
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
    let mut memory =
        fantastic_ram_store::test_support::memory_meta(&kernel, &id, json!({"cli": "*"})).await;
    creds::ensure_store(&kernel, tmp.path(), &id).await;
    creds::add_credentials(&mut memory, KEY_REF);
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
    set_key(&kernel, &tmp, &id, "sk-x").await;
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

/// `endpoint` is REQUIRED on the record — refused AT THE WRITE (the kind's
/// schema), naming the field and what it is, never only at the first turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_without_an_endpoint_is_refused_at_the_write_naming_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, "http://127.0.0.1:9/stub").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": id.as_str(), "endpoint": ""}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("invalid_meta"), "{r}");
    assert_eq!(
        r["error"],
        json!(
            "responses_backend: required on the record and missing: endpoint — nothing is \
             guessed. endpoint = the full Responses API URL (…/v1/responses)"
        )
    );
}

/// `store:false` keeps the conversation ours and `model` is the record's — a
/// `sampling` key naming either is refused, never merged over it.
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

// ─── a raw HTTP/1.1 stub ────────────────────────────────────────────────────
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

async fn send_once(kernel: &Arc<Kernel>, id: &AgentId) -> Value {
    kernel
        .send(vec![Item::new(
            id.clone(),
            json!({"type": "send", "text": "go", "client_id": "t", "reply_to": null}),
        )])
        .await
        .remove(0)
        .result
}

const DONE_OK: &str = "ok<call name=\"done\"></call>";

fn ok_response(_: usize) -> (Vec<u8>, bool) {
    full_response("text/event-stream", &text(&[DONE_OK], "completed", None))
}

/// A body cut mid-stream raises `stream: <reqwest's text ← every cause>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_cut_mid_stream_raises_stream_and_the_cause_chain() {
    let (port, _log) = raw_stub(|_| {
        cut_response(
            "text/event-stream",
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n\n",
        )
    })
    .await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, &format!("http://127.0.0.1:{port}/v1/responses")).await;
    set_key(&kernel, &tmp, &id, "sk").await;
    let r = send_once(&kernel, &id).await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!("RuntimeError: stream: error decoding response body ← error reading a body from connection ← end of file before message length reached"),
        "{r}"
    );
}

/// The key rides VERBATIM as `Authorization: Bearer <key>` (never trimmed: a key
/// with whitespace at an edge is refused by the reader, naming its ref); a key
/// ROTATED in the store rides the very next request (it is read per
/// request, never cached), and nothing pools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_key_rides_verbatim_a_rotated_key_rides_next_and_nothing_pools() {
    let (port, log) = raw_stub(ok_response).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, &format!("http://127.0.0.1:{port}/v1/responses")).await;
    set_key(&kernel, &tmp, &id, "sk-pad\tinner").await;
    assert_eq!(send_once(&kernel, &id).await["response"], json!(DONE_OK));
    let first = log.heads().len();
    set_key(&kernel, &tmp, &id, "sk-next").await;
    assert_eq!(send_once(&kernel, &id).await["response"], json!(DONE_OK));
    let heads = log.heads();
    assert!(first >= 1 && heads.len() > first, "{heads:#?}");
    for h in &heads[..first] {
        assert!(h.starts_with("POST /v1/responses HTTP/1.1\r\n"), "{h}");
        assert_eq!(
            raw_header(h, "authorization").as_deref(),
            Some("Bearer sk-pad\tinner"),
            "{h}"
        );
        assert_eq!(
            raw_header(h, "accept").as_deref(),
            Some("text/event-stream")
        );
    }
    for h in &heads[first..] {
        assert_eq!(
            raw_header(h, "authorization").as_deref(),
            Some("Bearer sk-next"),
            "a rotated key rides the next request: {h}"
        );
    }
    assert_eq!(
        log.conns(),
        heads.len(),
        "pool_max_idle_per_host(0): one connection per request"
    );
}

/// The unknown-type reply, as TEXT — a bare `{error}`; key order is wire.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, "http://127.0.0.1:1/v1/responses").await;
    let r = kernel
        .send(vec![Item::new(id, json!({"type": "frob"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"responses_backend: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#
    );
}
