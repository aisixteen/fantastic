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

/// A free TCP port that is then RELEASED — a subsequent connect refuses. Used to
/// simulate an unreachable ollama endpoint deterministically.
fn free_then_closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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

    let backend_id = backend_id_for(tmp, tag);
    let file_id = format!("ff_{}", backend_id);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": "file_bridge.tools",
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
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": backend_id,
                "file_bridge_id": file_id,
                "endpoint": endpoint,
                "model": "test-model",
                // Deny-all: persists to its store directly + streams to `cli`.
                "can_call": { file_id: "*", "cli": "*" },
            }),
        )])
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
    let (tx, rx) = fantastic_kernel::inbox::inbox(kernel.inbox_bound);
    kernel.inboxes.insert(backend.clone(), tx);
    rx
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
            out.push(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        if out.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("done")
        }) {
            while let Some(v) = rx.try_recv() {
                out.push(v);
            }
            break;
        }
    }
    out
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("ollama_backend"));
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
    let mut out: Vec<Result<ProviderEvent, String>> = Vec::new();
    for c in chunks {
        decode_chunk_into(c, &mut wrap, &mut out);
    }
    if let Some(m) = wrap.close_marker() {
        out.push(Ok(ProviderEvent::Token(m.to_string())));
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
    let messages = h["messages"].as_array().expect("messages array");
    assert_eq!(messages.last().unwrap()["content"], "ack");
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
            AgentId::from("core"),
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

#[test]
fn list_models_parses_tag_names_in_order() {
    // ollama's `/api/tags` shape → just the names, in order. Extra fields ignored,
    // odd/nameless entries skipped (never a panic).
    let body = json!({
        "models": [
            {"name": "gemma3:12b", "size": 8_000_000_000_u64},
            {"name": "llama3:8b"},
            {"no_name": true},
            {"name": "mistral:7b"}
        ]
    });
    assert_eq!(
        parse_tag_names(&body),
        vec![
            "gemma3:12b".to_string(),
            "llama3:8b".to_string(),
            "mistral:7b".to_string()
        ]
    );
    // Absent/empty `models` → an empty list, not an error.
    assert!(parse_tag_names(&json!({})).is_empty());
    assert!(parse_tag_names(&json!({"models": []})).is_empty());
}

#[tokio::test]
async fn list_models_served_returns_names_in_order() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "gemma3:12b"}, {"name": "llama3:8b"}]
        })))
        .mount(&server)
        .await;

    let (kernel, backend) = mk_kernel(&tmp, "lm", &server.uri()).await;
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "list_models"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["models"], json!(["gemma3:12b", "llama3:8b"]));
    assert!(
        reply.get("error").is_none(),
        "no error on success: {reply:#?}"
    );
}

#[tokio::test]
async fn list_models_payload_endpoint_overrides_record() {
    // The backend record points at a DEAD endpoint, but the payload override wins —
    // proving a caller can probe an ARBITRARY endpoint with no configured record.
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models": [{"name": "probe:1b"}]})),
        )
        .mount(&server)
        .await;

    let dead = format!("http://127.0.0.1:{}", free_then_closed_port());
    let (kernel, backend) = mk_kernel(&tmp, "lmo", &dead).await;
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "list_models", "endpoint": server.uri()}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["models"], json!(["probe:1b"]));
}

#[tokio::test]
async fn list_models_unreachable_returns_error_naming_the_endpoint() {
    let tmp = TempDir::new().unwrap();
    let dead = format!("http://127.0.0.1:{}", free_then_closed_port());
    let (kernel, backend) = mk_kernel(&tmp, "lmx", &dead).await;
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "list_models"}),
        )])
        .await
        .remove(0)
        .result;
    let err = reply
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected an error, got: {reply:#?}"));
    assert!(err.contains("ollama:"), "human-readable prefix: {err}");
    assert!(
        err.contains(&dead) && err.contains("/api/tags"),
        "the message must name the probed url: {err}"
    );
    assert!(reply.get("models").is_none());
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
    let messages = h["messages"].as_array().expect("messages array");
    assert!(
        messages.len() >= 2,
        "expected >=2 messages, got {messages:#?}"
    );
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "ping");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"], "ack");

    // The turns live in the KERNEL's row store — one table, keyed by the one
    // counter, agent id IS conversation id. They used to be an append-only
    // JSONL sidecar on the backend's own leg, split per client; that split gave
    // two clients on one agent two threads that could not see each other, and
    // the encoding was this bundle's private choice rather than the kernel's.
    let rows = kernel.rows.for_agent(&backend);
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

/// NO guessy default (mirror of the Python provider test): the sentinel
/// `DEFAULT_MODEL` is not a real id, and the request path fails loud — naming
/// `model` — rather than putting the sentinel (or an empty string) on the wire.
#[tokio::test]
async fn unset_model_is_the_sentinel_and_chat_fails_loud() {
    assert_eq!(DEFAULT_MODEL, "(no default — set the `model` meta)");
    for m in [DEFAULT_MODEL.to_string(), String::new()] {
        let p = OllamaProvider {
            endpoint: "http://127.0.0.1:1/api/chat".to_string(),
            model: m,
            temperature: 0.3,
            num_ctx: None,
            num_predict: None,
            think: None,
        };
        let err = match p.chat(&[json!({"role": "user", "content": "hi"})]).await {
            Ok(_) => panic!("chat must refuse an unset/sentinel model"),
            Err(e) => e,
        };
        assert!(
            err.contains("ollama_backend: no `model` set") && err.contains("model=..."),
            "unexpected error: {err}"
        );
    }
}
