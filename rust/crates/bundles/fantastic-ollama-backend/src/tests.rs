//! Unit tests for the ollama backend bundle.
//!
//! Tests run through `kernel.send`, with a real `fantastic-file` agent
//! handling persistence and `wiremock` standing in for ollama's HTTP
//! API. Each test uses a unique agent id (derived from its tempdir)
//! so the `BACKENDS` process-global static doesn't race.

use super::*;
use fantastic_kernel::Agent;
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
        .register("file.tools", fantastic_file::FileBundle);
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "file.tools",
                "id": file_id,
                "root": tmp.path().to_string_lossy(),
            }),
        )
        .await;
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": backend_id,
                "file_agent_id": file_id,
                "endpoint": endpoint,
                "model": "test-model",
            }),
        )
        .await;
    (kernel, AgentId::from(backend_id.as_str()))
}

/// Mount a synthetic client agent and return (id, rx). The browser
/// chat UI subscribes to the backend agent's inbox and filters by
/// `client_id`; we mirror that by swapping the backend's inbox
/// channel for one we own here, then forwarding any event whose
/// `client_id` matches the test's client.
///
/// Replaces the backend's inbox sender with a fresh channel, returns
/// the rx end for direct reading. NOTE: `kernel.emit(backend_id, ev)`
/// in the bundle pushes `ev` directly to this channel, exactly
/// matching how the WS proxy reads in production.
fn rebind_backend_inbox(
    kernel: &Arc<Kernel>,
    backend: &AgentId,
) -> tokio::sync::mpsc::Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(kernel.inbox_bound);
    kernel.inboxes.insert(backend.clone(), tx);
    rx
}

/// Build an ollama-style NDJSON body from an iterator of message
/// objects.
fn ndjson_body(parts: &[Value]) -> String {
    let mut out = String::new();
    for p in parts {
        out.push_str(&serde_json::to_string(p).unwrap());
        out.push('\n');
    }
    out
}

/// Drain everything currently in `rx`, then return.
async fn drain(rx: &mut tokio::sync::mpsc::Receiver<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    // Give the spawned send task a moment to enqueue all events.
    for _ in 0..40 {
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        // If we've got at least a `done`, bail early.
        if out
            .iter()
            .any(|v| v.get("type").and_then(Value::as_str) == Some("done"))
        {
            // Drain residual.
            while let Ok(v) = rx.try_recv() {
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
async fn reflect_reports_state_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "refl", "http://127.0.0.1:1").await;
    let r = kernel
        .send(&backend.clone(), json!({"type": "reflect"}))
        .await;
    // Every key in the contract must be present.
    for key in [
        "id",
        "sentence",
        "model",
        "endpoint",
        "file_agent_id",
        "generating",
        "verbs",
        "emits",
    ] {
        assert!(r.get(key).is_some(), "reflect missing key {key:?}: {r:#?}");
    }
    assert_eq!(r["id"], backend.as_str());
    assert_eq!(r["model"], "test-model");
    assert_eq!(r["generating"], false);
}

#[tokio::test]
async fn boot_is_noop() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "boot", "http://127.0.0.1:1").await;
    let r = kernel.send(&backend, json!({"type": "boot"})).await;
    assert!(
        r.is_null(),
        "boot should be a noop returning null, got {r:?}"
    );
}

#[tokio::test]
async fn send_without_file_agent_id_returns_error() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, OllamaBackendBundle);
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": "ob_nofile",
            }),
        )
        .await;
    let r = kernel
        .send(
            &AgentId::from("ob_nofile"),
            json!({"type": "send", "text": "hi"}),
        )
        .await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("file_agent_id"),
        "expected file_agent_id error, got {r:?}",
    );
}

#[tokio::test]
async fn send_streams_tokens_and_emits_done() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    // Three text chunks + a final done frame.
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
        .send(
            &backend,
            json!({"type": "send", "text": "hi", "client_id": client_id}),
        )
        .await;
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
        "expected ≥3 token events, got {token_count}: {events:#?}"
    );
    let done_count = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("done"))
        .count();
    assert!(done_count >= 1, "expected a done event, got: {events:#?}");
    // Status events should cover at least thinking + streaming + done phases.
    let phases: std::collections::HashSet<String> = events
        .iter()
        .filter_map(|e| {
            if e.get("type").and_then(Value::as_str) == Some("status") {
                e.get("phase").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    assert!(
        phases.contains("thinking"),
        "missing thinking phase: {phases:?}"
    );
    assert!(
        phases.contains("streaming"),
        "missing streaming phase: {phases:?}"
    );
    assert!(phases.contains("done"), "missing done phase: {phases:?}");
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
        .send(
            &backend,
            json!({"type": "send", "text": "ping", "client_id": client_id}),
        )
        .await;

    let h = kernel
        .send(&backend, json!({"type": "history", "client_id": client_id}))
        .await;
    let messages = h["messages"].as_array().expect("messages array");
    // First persisted message is the user turn, last is the assistant turn.
    assert!(
        messages.len() >= 2,
        "expected ≥2 messages, got {messages:#?}"
    );
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "ping");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"], "ack");
    // The chat file should exist on disk under the backend's dir.
    let expected_path = tmp.path().join(format!(
        ".fantastic/agents/{}/chat_{}.json",
        backend,
        safe_client(client_id)
    ));
    assert!(
        expected_path.exists(),
        "chat file not on disk: {expected_path:?}"
    );
}

#[tokio::test]
async fn interrupt_cancels_in_flight() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    // 2-second delayed response — long enough to interrupt mid-flight.
    let body = ndjson_body(&[json!({"message": {"content": "slow"}, "done": true})]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;

    let (kernel, backend) = mk_kernel(&tmp, "intr", &server.uri()).await;
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_intr";

    // Fire the slow send in the background.
    let k_for_send = Arc::clone(&kernel);
    let backend_for_send = backend.clone();
    let client_for_send = client_id.to_string();
    let send_join = tokio::spawn(async move {
        k_for_send
            .send(
                &backend_for_send,
                json!({"type": "send", "text": "hi", "client_id": client_for_send}),
            )
            .await
    });

    // Wait for the in-flight task to kick off, then interrupt.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let intr = kernel.send(&backend, json!({"type": "interrupt"})).await;
    assert_eq!(intr["interrupted"], true);

    let reply = send_join.await.expect("send join");
    assert_eq!(reply["interrupted"], true);

    let events = drain(&mut rx).await;
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("done")),
        "expected a done event after interrupt: {events:#?}",
    );
}

#[tokio::test]
async fn refresh_menu_drops_cache() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rmen", "http://127.0.0.1:1").await;
    // Pre-seed the menu cache with a sentinel value so we can observe
    // the drop without driving a full send.
    let state = state_for(&backend);
    *state.menu.lock().unwrap() = Some(vec![json!({"id": "sentinel"})]);
    assert!(state.menu.lock().unwrap().is_some());
    let r = kernel.send(&backend, json!({"type": "refresh_menu"})).await;
    assert_eq!(r["refreshed"], true);
    assert!(state.menu.lock().unwrap().is_none());
}

#[tokio::test]
async fn status_during_in_flight_shows_phase() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let body = ndjson_body(&[json!({"message": {"content": "x"}, "done": true})]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&server)
        .await;

    let (kernel, backend) = mk_kernel(&tmp, "stat", &server.uri()).await;
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_stat";

    let k_for_send = Arc::clone(&kernel);
    let backend_for_send = backend.clone();
    let client_for_send = client_id.to_string();
    let send_join = tokio::spawn(async move {
        k_for_send
            .send(
                &backend_for_send,
                json!({
                    "type": "send",
                    "text": "hi",
                    "client_id": client_for_send,
                }),
            )
            .await
    });

    // Wait for send to acquire the lock.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = kernel
        .send(&backend, json!({"type": "status", "client_id": client_id}))
        .await;
    assert_eq!(snap["generating"], true);
    let cur = &snap["current"];
    assert!(
        cur.is_object(),
        "current should be set during send: {snap:#?}"
    );
    assert_eq!(cur["is_mine"], true);
    // Phase should be one of the early ones.
    let phase = cur["phase"].as_str().unwrap_or("");
    assert!(
        ["thinking", "streaming", "tool_calling"].contains(&phase),
        "unexpected phase {phase:?}: {snap:#?}",
    );

    // Let send finish.
    let _ = send_join.await.expect("send join");
}
