//! Unit tests for `fantastic-llamacpp-backend`.
//!
//! This bundle had none until the python-parity pass; what it answers `reflect`
//! with is what every model on a local llama-server reads about itself, so the
//! reflect body is pinned here against `llamacpp_backend/tools.py`.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::Map;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Per-test id derived from the tempdir name — the state maps in this crate are
/// process-global, so a shared id would race across parallel tests.
fn agent_id_for(tmp: &TempDir) -> String {
    format!(
        "lc_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

/// Kernel + root + this bundle's agent. No file leg: the conversation lives in
/// the agent's memory store (a `ram_store`, wired both ways).
async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, LlamaCppBundle);
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
    let id = agent_id_for(tmp);
    let memory =
        fantastic_ram_store::test_support::memory_meta(&kernel, &id, json!({ "cli": "*" })).await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": id,
        // required at the write (the schema); a test that dials a server
        // points it at its own mock with `update_agent`
        "endpoint": "http://127.0.0.1:9/stub",
    });
    for (k, v) in memory {
        create[k] = v;
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    (kernel, AgentId::from(id.as_str()))
}

#[test]
fn readme_is_python_verbatim() {
    // B4.5 — the readme a model reads about this backend is the PYTHON bundle's,
    // byte for byte. Each runtime had written its own, so the same agent
    // described itself differently depending on where it ran.
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/ai/llamacpp/llamacpp_backend/src/llamacpp_backend/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

#[test]
fn the_unset_model_is_pythons_sentinel() {
    // `llama-server` serves exactly ONE model and ignores the request's `model`
    // field, so there is nothing to guess; python names the same sentinel.
    assert_eq!(DEFAULT_MODEL, "(server-loaded)");
    let p = LlamaCppProvider {
        endpoint: "http://127.0.0.1:1/v1".to_string(),
        requested_model: None,
        api_key: None,
        max_tokens: None,
        temperature: DEFAULT_TEMPERATURE,
        think: None,
        report: Default::default(),
    };
    assert_eq!(p.model(), "(server-loaded)");
}

/// B4.4 — python folds the default in at CONSTRUCTION
/// (`self._temperature = 0.3 if temperature is None else float(temperature)`),
/// so `temperature` is on the wire of EVERY request. Rust omitted the field
/// when the record named none, leaving the server's own default (often 0.8) to
/// decide how literal a tool-calling model was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temperature_is_always_on_the_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok<call name=\\\"done\\\"></call>\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(),
                   "endpoint": format!("{}/v1", server.uri())}),
        )])
        .await;
    let _ = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await;
    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["temperature"], json!(DEFAULT_TEMPERATURE));

    // …and the record still overrides it.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(),
                   "endpoint": format!("{}/v1", server.uri()), "temperature": 0.9}),
        )])
        .await;
    let _ = kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await;
    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert_eq!(body["temperature"], json!(0.9));
}

#[tokio::test]
async fn reflect_extras_match_python() {
    // python `_make_reflect` + `llamacpp_backend/tools.py::_reflect_extra`: the
    // shared base body in python's order, then `temperature` (0.3 by default),
    // `num_predict`, `think`, `has_api_key`. `num_ctx` is deliberately ABSENT —
    // llama-server fixes the window at start.
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(agent.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    let keys: Vec<String> = r.as_object().unwrap().keys().take(16).cloned().collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "model",
            "endpoint",
            "context_window",
            "context_strategy",
            "memory_store_id",
            "verbs",
            "generating",
            "cancel",
            "emits",
            "concurrency",
            "temperature",
            "num_predict",
            "think",
            "has_api_key",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>(),
        "{r:#?}"
    );
    assert_eq!(
        r["sentence"],
        "llama.cpp-backed local LLM agent (raw prompt-and-parse tool-calling)."
    );
    // what the RECORD names — nothing guessed (the provider reports the
    // server's sentinel; reflect reports the record)
    assert_eq!(r["model"], Value::Null);
    assert_eq!(r["temperature"], 0.3);
    assert_eq!(r["num_predict"], Value::Null);
    assert_eq!(r["think"], Value::Null);
    assert_eq!(r["has_api_key"], false);
    assert!(r.get("num_ctx").is_none(), "{r:#?}");
    // The shared verb menu and the shared emits, not a bundle-private copy.
    assert_eq!(r["verbs"], fantastic_ai_core::verbs::reflect_verbs());
    assert_eq!(r["emits"], fantastic_ai_core::verbs::reflect_emits());

    // Set the knobs: python's conversions apply.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(),
                   "temperature": 0.7, "num_predict": 0, "think": false,
                   "api_key": "k"}),
        )])
        .await;
    let r = kernel
        .send(vec![Item::new(agent, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["temperature"], 0.7);
    assert_eq!(r["num_predict"], Value::Null, "0 is falsy → None");
    assert_eq!(r["think"], false, "present-and-not-null → bool(x)");
    assert_eq!(r["has_api_key"], true);
}

/// B3.1 — python `handler`'s `{name}: unknown type {t!r}`: python's repr (single
/// quotes), and `shutdown` is NOT in the VERBS map, so it is unknown like
/// anything else. The prefix is the BACKEND's name — rust answered `ollama:`
/// where python answers `ollama_backend:`.
#[tokio::test]
async fn unknown_type_is_pythons_repr_and_shutdown_is_unknown() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    for (verb, want) in [
        ("garbage", "llamacpp_backend: unknown type 'garbage'"),
        ("shutdown", "llamacpp_backend: unknown type 'shutdown'"),
    ] {
        let r = kernel
            .send(vec![Item::new(agent.clone(), json!({"type": verb}))])
            .await
            .remove(0)
            .result;
        assert_eq!(r["error"], json!(want));
    }
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

async fn point_at(kernel: &Arc<Kernel>, agent: &AgentId, extra: Value) {
    let mut p = json!({"type": "update_agent", "id": agent.as_str()});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), p)])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
}

async fn send_once(kernel: &Arc<Kernel>, agent: &AgentId) -> Value {
    kernel
        .send(vec![Item::new(
            agent.clone(),
            json!({"type": "send", "text": "go", "client_id": "t", "reply_to": null}),
        )])
        .await
        .remove(0)
        .result
}

const SSE: &str = "text/event-stream";

/// A body cut mid-stream raises `stream: <reqwest's text>` — reqwest's OUTER
/// text only (no cause chain, unlike nim/anthropic).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_cut_mid_stream_raises_stream_and_the_outer_text() {
    let (port, _log) = raw_stub(|_| {
        cut_response(
            SSE,
            "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
        )
    })
    .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    point_at(
        &kernel,
        &agent,
        json!({"endpoint": format!("http://127.0.0.1:{port}/v1")}),
    )
    .await;
    let r = send_once(&kernel, &agent).await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!("RuntimeError: stream: error decoding response body"),
        "{r}"
    );
}

/// No server at all: `http: <reqwest's outer text>`, raised.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_connection_raises_http_and_the_outer_text() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    let r = send_once(&kernel, &agent).await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!("RuntimeError: http: error sending request for url (http://127.0.0.1:9/stub/chat/completions)"),
        "{r}"
    );
}

/// A non-2xx status raises `send: provider HTTP <code>` (no mapper here).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http_error_status_raises_naming_the_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    point_at(
        &kernel,
        &agent,
        json!({"endpoint": format!("{}/v1", server.uri())}),
    )
    .await;
    let r = send_once(&kernel, &agent).await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!("RuntimeError: send: provider HTTP 503"),
        "{r}"
    );
}

/// A stream that ENDS without `[DONE]` is still a turn; a reasoning span left
/// open at the end is closed exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eof_without_done_closes_the_reasoning_span_once() {
    use fantastic_ai_core::think::{THINK_CLOSE, THINK_OPEN};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hm\"}}]}\n\n",
            SSE,
        ))
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    point_at(
        &kernel,
        &agent,
        json!({"endpoint": format!("{}/v1", server.uri())}),
    )
    .await;
    let r = send_once(&kernel, &agent).await;
    assert_eq!(
        r["response"],
        json!(format!("{THINK_OPEN}hm{THINK_CLOSE}")),
        "{r}"
    );
}

/// The record's `api_key` rides as `Authorization: Bearer <key>` only when set;
/// llamacpp builds a FRESH client per pass (one connection each).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bearer_rides_only_when_set_and_each_pass_dials_anew() {
    let (port, log) = raw_stub(|_| {
        full_response(
            SSE,
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok<call name=\\\"done\\\"></call>\"}}]}\n\ndata: [DONE]\n\n",
        )
    })
    .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    point_at(
        &kernel,
        &agent,
        json!({"endpoint": format!("http://127.0.0.1:{port}/v1")}),
    )
    .await;
    let done = json!("ok<call name=\"done\"></call>");
    assert_eq!(send_once(&kernel, &agent).await["response"], done);
    let first = log.heads().len();
    point_at(&kernel, &agent, json!({"api_key": " sk-1 "})).await;
    assert_eq!(send_once(&kernel, &agent).await["response"], done);
    // `done` ends each chain: no background pass straddles the key change
    let heads = log.heads();
    assert!(first >= 1 && heads.len() > first, "{heads:#?}");
    for h in &heads[..first] {
        assert!(
            h.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
            "{h}"
        );
        assert_eq!(raw_header(h, "authorization"), None, "no key, no header");
    }
    for h in &heads[first..] {
        assert_eq!(
            raw_header(h, "authorization").as_deref(),
            Some("Bearer  sk-1 "),
            "the record's key, verbatim: {h}"
        );
    }
    assert_eq!(log.conns(), heads.len(), "no pooled connection is reused");
}

/// The unknown-type reply, as TEXT — a bare `{error}`; key order is wire.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(agent, json!({"type": "frob"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"llamacpp_backend: unknown type 'frob'"}"#
    );
}
