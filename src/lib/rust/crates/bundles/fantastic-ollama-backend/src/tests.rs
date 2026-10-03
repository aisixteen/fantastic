//! Transport-specific tests for the ollama backend: the NDJSON
//! `/api/chat` parse path, driven end-to-end through `kernel.send` with
//! `wiremock` standing in for ollama's HTTP API. The shared verb / loop
//! / state / assembly behaviour is tested in `fantastic-ai-core`.

use super::*;
use fantastic_ai_core::think::{THINK_CLOSE, THINK_OPEN};
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::Map;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn backend_id_for(tmp: &TempDir, tag: &str) -> String {
    format!(
        "ob_{}_{}",
        tag,
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

/// Build a kernel with a file agent (rooted at tmp) and an ollama
/// backend agent bound to that file agent + the given ollama endpoint.
async fn mk_kernel(tmp: &TempDir, tag: &str, endpoint: &str) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, OllamaBackendBundle);
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

    let backend_id = backend_id_for(tmp, tag);
    let file_id = format!("ff_{}", backend_id);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": file_id,
                "root": tmp.path().join(".fantastic").to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the backend that persists through it.
                "callable_by": { backend_id.clone(): "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    // Deny-all: its memory store (wired both ways) + its leg + `cli`.
    let memory = fantastic_ram_store::test_support::memory_meta(
        &kernel,
        &backend_id,
        json!({ file_id.clone(): "*", "cli": "*" }),
    )
    .await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": backend_id,
        "endpoint": endpoint,
        "model": "test-model",
    });
    for (k, v) in memory {
        create[k] = v;
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    (kernel, AgentId::from(backend_id.as_str()))
}

/// Replace the backend's inbox sender with a fresh channel we own. The
/// cli-round-trip route emits non-cli events on the backend's inbox.
fn rebind_backend_inbox(
    kernel: &Arc<Kernel>,
    backend: &AgentId,
) -> fantastic_kernel::inbox::InboxReceiver {
    kernel.ensure_inbox(backend)
}

/// Build an ollama-style NDJSON body from message objects.
fn ndjson_body(parts: &[Value]) -> String {
    let mut out = String::new();
    for p in parts {
        out.push_str(&serde_json::to_string(p).unwrap());
        out.push('\n');
    }
    out
}

async fn drain(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..40 {
        while let Some(v) = rx.try_recv() {
            out.push(v.payload);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        if out.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("done")
        }) {
            while let Some(v) = rx.try_recv() {
                out.push(v.payload);
            }
            break;
        }
    }
    out
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
        .join("python/bundled_agents/ai/ollama/ollama_backend/src/ollama_backend/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

#[tokio::test]
async fn send_streams_tokens_and_emits_done() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    // Three text chunks via NDJSON.
    let body = ndjson_body(&[
        json!({"message": {"content": "Hello"}}),
        json!({"message": {"content": " "}}),
        json!({"message": {"content": "world"}, "done": true}),
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let (kernel, backend) = mk_kernel(&tmp, "tok", &server.uri()).await;
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_tok";

    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["client_id"], client_id);
    assert_eq!(reply["response"], "Hello world");
    assert_eq!(reply["final"], "Hello world");

    let events = drain(&mut rx).await;
    let token_count = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("token"))
        .count();
    assert!(
        token_count >= 3,
        "expected >=3 token events, got {token_count}: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("status")
                && e.get("phase").and_then(Value::as_str) == Some("done")),
        "expected a done event, got: {events:#?}"
    );
}

/// An envelope in the completion DISPATCHES, over the real NDJSON transport.
///
/// This test used to assert the opposite (`envelope_text_streams_verbatim_no_dispatch`
/// — "just TEXT, nothing parsed, nothing dispatched"). That was never a principle,
/// it was a gap: python's ai_core has parsed and dispatched this envelope since
/// 2026-08-02, and this crate's own module header already said ai-core owns the
/// parse. What the transport still owes is the OTHER half — the text reaches the
/// caller verbatim, markers and all, so a mid-thought call survives the trip.
///
/// `reply_to:null` keeps it to exactly ONE HTTP pass: the wiremock stub answers
/// with the same envelope every time, so a default `reply_to` would (correctly)
/// chain forever against a mock that never says anything else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_envelope_dispatches_and_streams_verbatim() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (kernel, backend) = mk_kernel(&tmp, "tool", &server.uri()).await;
    // Address the backend's OWN file agent: its write is observable on disk, which
    // is proof the dispatch happened and not just that nothing crashed.
    let envelope = format!(
        "<call name=\"send\"><target_id>ff_{backend}</target_id>\
         <reply_to>null</reply_to><type>mkdir</type><path>routed</path></call>"
    );
    let body = ndjson_body(&[json!({"message": {"content": envelope}, "done": true})]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1) // one pass: the call discards its reply, so nothing follows
        .mount(&server)
        .await;

    let client_id = "browser_tool";
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["response"], envelope, "verbatim text: {reply:#?}");

    let routed = tmp.path().join(".fantastic").join("routed");
    for _ in 0..250 {
        if routed.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        routed.exists(),
        "the envelope never dispatched: {routed:?} missing"
    );
}

/// Join a decoded pass into the text ai-core would see.
fn decoded_text(chunks: &[Value]) -> String {
    let mut wrap = ThinkWrap::new();
    let mut out: VecDeque<Result<ProviderEvent, ProviderError>> = VecDeque::new();
    for c in chunks {
        decode_chunk_into(c, &mut wrap, &mut out);
    }
    if let Some(m) = wrap.close_marker() {
        out.push_back(Ok(ProviderEvent::Token(m.to_string())));
    }
    out.into_iter()
        .map(|e| match e.expect("no decode errors") {
            ProviderEvent::Token(t) => t,
        })
        .collect()
}

#[test]
fn reasoning_channel_is_wrapped_once_per_turn() {
    // ollama streams a reasoning model's tokens as `message.thinking` and only its
    // conclusion as `message.content`. THREE thinking chunks + one content chunk
    // must produce exactly ONE open and ONE close — per chunk it would be a
    // confetti of tags, and ai-core's stripper would eat the answer with it.
    let text = decoded_text(&[
        json!({"message": {"thinking": "step one "}}),
        json!({"message": {"thinking": "step two "}}),
        json!({"message": {"thinking": "step three"}}),
        json!({"message": {"content": "the answer"}, "done": true}),
    ]);
    assert_eq!(text.matches(THINK_OPEN).count(), 1, "one open: {text:?}");
    assert_eq!(text.matches(THINK_CLOSE).count(), 1, "one close: {text:?}");
    assert_eq!(
        text,
        format!("{THINK_OPEN}step one step two step three{THINK_CLOSE}the answer")
    );
}

#[test]
fn a_thinking_only_chunk_is_not_an_empty_completion() {
    // Measured live on 2026-08-02: gemma spent all 578 generated tokens on
    // `thinking`, we read only `content`, and the turn looked like the server had
    // wedged. The span closes at end-of-stream even though content never started.
    let text = decoded_text(&[json!({"message": {"thinking": "all of it"}, "done": true})]);
    assert_eq!(text, format!("{THINK_OPEN}all of it{THINK_CLOSE}"));
    // An explicit `null` on either channel is shape, not content.
    let none = decoded_text(&[json!({"message": {"thinking": null, "content": null}})]);
    assert_eq!(none, "");
}

#[tokio::test]
async fn thinking_streams_wrapped_and_persists_stripped() {
    // End-to-end over the real NDJSON transport: the caller gets the wrapped raw
    // text, the durable turn keeps only the conclusion.
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let body = ndjson_body(&[
        json!({"message": {"thinking": "deliberating"}}),
        json!({"message": {"content": "ack"}, "done": true}),
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let (kernel, backend) = mk_kernel(&tmp, "think", &server.uri()).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": "c_think"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        reply["response"],
        json!(format!("{THINK_OPEN}deliberating{THINK_CLOSE}ack"))
    );
    let h = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "history", "client_id": "c_think"}),
        )])
        .await
        .remove(0)
        .result;
    let rows = h["rows"].as_array().expect("rows array");
    assert_eq!(rows.last().unwrap()["head"], "ack");
}

#[tokio::test]
async fn sends_temperature_default_and_override() {
    // The backend ALWAYS sends `options.temperature` to ollama (ollama's own
    // default 0.8 is wrong for an agentic backend). Default = DEFAULT_TEMPERATURE
    // (0.3), overridable via the record's `temperature` field; reflect surfaces it.
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let body = ndjson_body(&[json!({"message": {"content": "ok"}, "done": true})]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let (kernel, backend) = mk_kernel(&tmp, "temp", &server.uri()).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);

    // default: reflect + the actual request both carry 0.3.
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type":"send","text":"hi","client_id":"c"}),
        )])
        .await
        .remove(0)
        .result;
    let r = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["temperature"], json!(DEFAULT_TEMPERATURE));
    let reqs = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(sent["options"]["temperature"], json!(DEFAULT_TEMPERATURE));

    // override via the record: reflect + the next request both carry it.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type":"update_agent","id": backend.as_str(), "temperature": 0.15}),
        )])
        .await
        .remove(0)
        .result;
    let r2 = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r2["temperature"], json!(0.15));
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type":"send","text":"hi2","client_id":"c"}),
        )])
        .await
        .remove(0)
        .result;
    let reqs = server.received_requests().await.unwrap();
    let last: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert_eq!(last["options"]["temperature"], json!(0.15));
}

#[tokio::test]
async fn list_models_is_unknown_type() {
    // D7: `list_models` was a rust-only verb. Python's ollama backend never had
    // one, so a model that read it off `reflect` was taught a probe that does not
    // exist on the canonical runtime — it is now an unknown type like any other.
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (kernel, backend) = mk_kernel(&tmp, "lm", &server.uri()).await;
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "list_models"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        reply["error"],
        json!("ollama_backend: unknown type 'list_models'")
    );
    assert!(reply.get("models").is_none());
    // And it is gone from the verb menu the model reads.
    let reflect = kernel
        .send(vec![Item::new(backend, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert!(
        reflect["verbs"].get("list_models").is_none(),
        "{reflect:#?}"
    );
}

#[tokio::test]
async fn history_persists_and_round_trips() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let body = ndjson_body(&[json!({"message": {"content": "ack"}, "done": true})]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let (kernel, backend) = mk_kernel(&tmp, "hist", &server.uri()).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_hist";
    let _reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;

    let h = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "history", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    let digest = h["rows"].as_array().expect("rows array");
    assert!(digest.len() >= 2, "expected >=2 rows, got {digest:#?}");
    assert_eq!(digest[0]["role"], "user");
    assert_eq!(digest[0]["head"], "ping");
    let last = digest.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["head"], "ack");

    // The turns live in the agent's MEMORY STORE (`memory_store_id`) — agent id
    // IS conversation id; the kernel keeps no conversation.
    let rows = fantastic_ai_core::memory::load_history(&backend, &kernel, true)
        .await
        .expect("store read");
    assert_eq!(rows.len(), 2, "one row per message: {rows:#?}");
    assert_eq!(rows[0]["content"], "ping");
    assert_eq!(rows[1]["content"], "ack");
    assert!(
        !tmp.path()
            .join(".fantastic")
            .join(format!("agents/{backend}"))
            .exists(),
        "the backend wrote a conversation sidecar on its own leg"
    );
    assert!(
        !kernel.agents.contains_key(&fantastic_kernel::AgentId::from(
            format!("{backend}_chat").as_str()
        )),
        "the `<self>_chat` child must be gone"
    );
}

/// NO guessy default (mirror of the Python provider test): an empty model never
/// reaches the wire — the request path fails loud, naming `model`.
#[tokio::test]
async fn an_empty_model_fails_loud_in_chat() {
    for m in [String::new()] {
        let p = OllamaProvider {
            endpoint: "http://127.0.0.1:1/api/chat".to_string(),
            model: m,
            temperature: 0.3,
            num_ctx: None,
            num_predict: None,
            think: None,
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
            err.contains("ollama_backend: no `model` set") && err.contains("model=..."),
            "unexpected error: {err}"
        );
    }
}

#[tokio::test]
async fn reflect_extras_match_python() {
    // python `_reflect_extra`: temperature defaults to the provider's 0.3, and
    // `num_ctx`/`num_predict`/`think` are null unless SET on the record. They
    // land AFTER `concurrency`, exactly as `out.update(extra)` does.
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (kernel, backend) = mk_kernel(&tmp, "rx", &server.uri()).await;
    let r = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "reflect"}))])
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
            "num_ctx",
            "num_predict",
            "think",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>(),
        "{r:#?}"
    );
    assert_eq!(
        r["sentence"],
        "Ollama-backed LLM agent (raw prompt-and-parse tool-calling)."
    );
    assert_eq!(r["temperature"], 0.3);
    assert_eq!(r["num_ctx"], Value::Null);
    assert_eq!(r["num_predict"], Value::Null);
    assert_eq!(r["think"], Value::Null);

    // Set them, and python's conversions apply: `float(t)`, `int(x) if x`,
    // `bool(x) if x is not None`.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(),
                   "temperature": "0.7", "num_ctx": "4096", "num_predict": 0,
                   "think": 0}),
        )])
        .await;
    let r = kernel
        .send(vec![Item::new(backend, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["temperature"], 0.7);
    assert_eq!(r["num_ctx"], 4096);
    assert_eq!(r["num_predict"], Value::Null, "0 is falsy → None");
    assert_eq!(r["think"], false, "present-and-not-null → bool(x)");
}

/// B3.1 — python `handler`'s `{name}: unknown type {t!r}`: python's repr (single
/// quotes), and `shutdown` is NOT in the VERBS map, so it is unknown like
/// anything else. The prefix is the BACKEND's name — rust answered `ollama:`
/// where python answers `ollama_backend:`.
#[tokio::test]
async fn unknown_type_is_pythons_repr_and_shutdown_is_unknown() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "unk", "http://127.0.0.1:1").await;
    for (verb, want) in [
        ("garbage", "ollama_backend: unknown type 'garbage'"),
        ("shutdown", "ollama_backend: unknown type 'shutdown'"),
    ] {
        let r = kernel
            .send(vec![Item::new(agent.clone(), json!({"type": verb}))])
            .await
            .remove(0)
            .result;
        assert_eq!(r["error"], json!(want));
    }
}

/// PROVIDER-REPORTED USAGE off the NDJSON stream: `prompt_eval_count` calibrates
/// ai_core's estimate, and `done_reason: length` is the output stopping AT its
/// bound (`max_tokens`) — what a fold reads to refuse a cut summary.
#[test]
fn ndjson_usage_reports_prompt_tokens_and_a_cut_as_max_tokens() {
    let report = fantastic_ai_core::provider::ReportCell::default();
    note_usage(
        &json!({"message": {"content": "x"}, "done": false}),
        &report,
    );
    assert_eq!(report.lock().unwrap().prompt_tokens, None);
    note_usage(
        &json!({"done": true, "done_reason": "length", "prompt_eval_count": 321}),
        &report,
    );
    let r = report.lock().unwrap().clone();
    assert_eq!(r.prompt_tokens, Some(321));
    assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
    note_usage(&json!({"done": true, "done_reason": "stop"}), &report);
    assert_eq!(report.lock().unwrap().stop_reason.as_deref(), Some("stop"));
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

async fn send_once(kernel: &Arc<Kernel>, backend: &AgentId, client: &str) -> Value {
    kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": client, "reply_to": null}),
        )])
        .await
        .remove(0)
        .result
}

/// The mid-stream transport error is raised with THIS backend's own wording
/// (`ollama: stream error: <reqwest's text>`), not the SSE backends' `stream:`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_cut_mid_stream_raises_in_ollamas_words() {
    let (port, _log) = raw_stub(|_| {
        cut_response(
            "application/x-ndjson",
            "{\"message\":{\"content\":\"par\"}}\n",
        )
    })
    .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cut", &format!("http://127.0.0.1:{port}")).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let r = send_once(&kernel, &backend, "c_cut").await;
    assert_eq!(r["raised"], json!(true), "{r}");
    assert_eq!(
        r["error"],
        json!("RuntimeError: ollama: stream error: error decoding response body"),
        "{r}"
    );
}

/// A last NDJSON line with no trailing newline is still a line.
#[tokio::test]
async fn a_final_ndjson_line_without_a_newline_still_counts() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let body = "{\"message\":{\"content\":\"Hello\"}}\n{\"message\":{\"content\":\" world\"},\"done\":true,\"done_reason\":\"stop\"}";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let (kernel, backend) = mk_kernel(&tmp, "eofline", &server.uri()).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let r = send_once(&kernel, &backend, "c_eof").await;
    assert_eq!(r["response"], json!("Hello world"), "{r}");
    // and a thinking-only turn cut at EOF (no newline) still closes its span once
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("{\"message\":{\"thinking\":\"hm\"}}"),
        )
        .mount(&server)
        .await;
    let tmp2 = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp2, "eofthink", &server.uri()).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let r = send_once(&kernel, &backend, "c_eof2").await;
    assert_eq!(
        r["response"],
        json!(format!("{THINK_OPEN}hm{THINK_CLOSE}")),
        "{r}"
    );
}

/// The unknown-type reply, as TEXT — a bare `{error}`; key order is wire.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "unkw", "http://127.0.0.1:1").await;
    let r = kernel
        .send(vec![Item::new(agent, json!({"type": "frob"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"ollama_backend: unknown type 'frob'"}"#
    );
}

/// ollama builds a FRESH client per pass — two passes, two connections, even
/// though the stub keeps the first alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_pass_opens_its_own_connection() {
    let (port, log) = raw_stub(|_| {
        full_response(
            "application/x-ndjson",
            "{\"message\":{\"content\":\"ok<call name=\\\"done\\\"></call>\"},\"done\":true}\n",
        )
    })
    .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "conn", &format!("http://127.0.0.1:{port}")).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let done = json!("ok<call name=\"done\"></call>");
    assert_eq!(send_once(&kernel, &backend, "c1").await["response"], done);
    assert_eq!(send_once(&kernel, &backend, "c1").await["response"], done);
    let heads = log.heads();
    assert!(heads.len() >= 2, "{heads:#?}");
    assert!(
        heads[0].starts_with("POST /api/chat HTTP/1.1\r\n"),
        "{heads:?}"
    );
    assert_eq!(log.conns(), heads.len(), "no pooled connection is reused");
}

/// The record's `system_prompt` is refused AT THE WRITE unless it is a non-blank
/// string; null clears it. Python `ai_core/schema.py:validate`.
#[tokio::test]
async fn a_bad_system_prompt_is_refused_at_the_write() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "sp", "http://127.0.0.1:1").await;
    let write = |v: Value| {
        let kernel = Arc::clone(&kernel);
        let id = backend.as_str().to_string();
        async move {
            kernel
                .send(vec![Item::new(
                    AgentId::from("kernel_state"),
                    json!({"type": "update_agent", "id": id, "system_prompt": v}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    for bad in [json!(""), json!("   "), json!(7), json!(["x"])] {
        let r = write(bad.clone()).await;
        assert_eq!(
            r["error"],
            json!(fantastic_ai_core::schema::SYSTEM_PROMPT_REFUSAL),
            "{bad}: {r}"
        );
    }
    let r = write(json!("be terse")).await;
    assert!(r.get("error").is_none(), "{r}");
    let r = write(Value::Null).await;
    assert!(r.get("error").is_none(), "{r}");
}
