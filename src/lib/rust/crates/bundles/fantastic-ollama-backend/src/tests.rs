//! Transport-specific tests for the ollama backend: the NDJSON
//! `/api/chat` parse path, driven end-to-end through `kernel.send` with
//! `wiremock` standing in for ollama's HTTP API. The shared verb / loop
//! / state / assembly behaviour is tested in `fantastic-ai-core`.

use super::*;
use fantastic_ai_core::helpers::safe_client;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "id": file_id,
                "root": tmp.path().join(".fantastic").to_string_lossy(),
                "ingress_rule": "allow_all",
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
                "file_bridge_id": file_id,
                "endpoint": endpoint,
                "model": "test-model",
            }),
        )
        .await;
    (kernel, AgentId::from(backend_id.as_str()))
}

/// Replace the backend's inbox sender with a fresh channel we own. The
/// cli-round-trip route emits non-cli events on the backend's inbox.
fn rebind_backend_inbox(
    kernel: &Arc<Kernel>,
    backend: &AgentId,
) -> tokio::sync::mpsc::Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(kernel.inbox_bound);
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

async fn drain(rx: &mut tokio::sync::mpsc::Receiver<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..40 {
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        if out
            .iter()
            .any(|v| v.get("type").and_then(Value::as_str) == Some("done"))
        {
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
        "expected >=3 token events, got {token_count}: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("done")),
        "expected a done event, got: {events:#?}"
    );
}

#[tokio::test]
async fn raw_tool_call_in_content_parses_and_dispatches() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    // RAW: ollama streams plain CONTENT text containing the `<tool_call>` envelope;
    // ai-core's shared parser extracts it and dispatches. Second response: final text.
    let with_tool = ndjson_body(&[json!({
        "message": {
            "content": "<tool_call>{\"name\":\"send\",\"arguments\":{\"target_id\":\"core\",\"payload\":{\"type\":\"list_agents\"}}}</tool_call>"
        },
        "done": true
    })]);
    let final_body = ndjson_body(&[json!({"message": {"content": "ok"}, "done": true})]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(with_tool))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(final_body))
        .mount(&server)
        .await;

    let (kernel, backend) = mk_kernel(&tmp, "tool", &server.uri()).await;
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_tool";
    let reply = kernel
        .send(
            &backend,
            json!({"type": "send", "text": "go", "client_id": client_id}),
        )
        .await;
    assert_eq!(reply["response"], "ok");

    let events = drain(&mut rx).await;
    // The args object must JSON-parse + dispatch against 'core'.
    let dispatched = events.iter().any(|e| {
        e.get("type").and_then(Value::as_str) == Some("status")
            && e.get("detail")
                .and_then(|d| d.get("tool"))
                .and_then(|t| t.get("args"))
                .and_then(|a| a.get("target_id"))
                .and_then(Value::as_str)
                == Some("core")
    });
    assert!(dispatched, "expected parsed tool args -> core: {events:#?}");
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
    assert!(
        messages.len() >= 2,
        "expected >=2 messages, got {messages:#?}"
    );
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "ping");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"], "ack");
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
