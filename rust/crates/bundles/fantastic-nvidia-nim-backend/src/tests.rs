//! Unit tests for `fantastic-nvidia-nim-backend`.
//!
//! Wiremock spins up a real HTTP server emitting SSE chunks; we drive
//! the bundle via `kernel.send` and assert on the file-agent-routed
//! sidecars + the per-client inbox.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Per-test id derived from the tempdir name. Static maps in this
/// crate are process-global; sharing one id across parallel tests
/// would race.
fn nim_id_for(tmp: &TempDir) -> String {
    format!(
        "nim_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

/// Stand up: kernel + file bundle + nvidia bundle + a root agent +
/// a file agent rooted at the workdir + an nvidia agent bound to it.
async fn mk_kernel(tmp: &TempDir, endpoint: Option<String>) -> (Arc<Kernel>, AgentId, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, NvidiaNimBundle);
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
    let nim = nim_id_for(tmp);
    let fid = format!("ff_{}", nim);
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type":"create_agent",
                "handler_module":"file.tools",
                "id": fid,
                "root": tmp.path().to_string_lossy(),
            }),
        )
        .await;
    let mut create = json!({
        "type":"create_agent",
        "handler_module":HANDLER_MODULE,
        "id": nim,
        "file_agent_id": fid,
        "model": "test-model",
    });
    if let Some(e) = endpoint {
        create["endpoint"] = json!(e);
    }
    kernel.send(&AgentId::from("core"), create).await;
    (
        kernel,
        AgentId::from(nim.as_str()),
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

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("nvidia_nim_backend"));
}

#[tokio::test]
async fn reflect_shape_has_api_key_false_initially() {
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel.send(&nim, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], nim.as_str());
    assert_eq!(r["has_api_key"], false);
    assert_eq!(r["model"], "test-model");
    assert!(r["verbs"]["set_api_key"].is_string());
    assert!(r["verbs"]["clear_api_key"].is_string());
}

#[tokio::test]
async fn set_api_key_persists_via_file_agent_and_reflect_flips() {
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(
            &nim,
            json!({"type": "set_api_key", "api_key": "nvapi-test-abc"}),
        )
        .await;
    assert_eq!(r["ok"], true);
    // Sidecar on disk via file agent.
    let key_file = tmp
        .path()
        .join(format!(".fantastic/agents/{}/api_key", nim));
    assert!(key_file.exists(), "api_key sidecar should exist");
    let content = std::fs::read_to_string(&key_file).unwrap();
    assert_eq!(content.trim(), "nvapi-test-abc");
    let r2 = kernel.send(&nim, json!({"type": "reflect"})).await;
    assert_eq!(r2["has_api_key"], true);
    // The reflect MUST NOT leak the key value.
    let serialized = serde_json::to_string(&r2).unwrap();
    assert!(
        !serialized.contains("nvapi-test-abc"),
        "reflect must never include the api_key value"
    );
}

#[tokio::test]
async fn clear_api_key_deletes_sidecar() {
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    kernel
        .send(&nim, json!({"type": "set_api_key", "api_key": "k1"}))
        .await;
    let key_file = tmp
        .path()
        .join(format!(".fantastic/agents/{}/api_key", nim));
    assert!(key_file.exists());
    let r = kernel.send(&nim, json!({"type": "clear_api_key"})).await;
    assert_eq!(r["ok"], true);
    assert_eq!(r["deleted"], true);
    assert!(!key_file.exists());
    let r2 = kernel.send(&nim, json!({"type": "reflect"})).await;
    assert_eq!(r2["has_api_key"], false);
}

#[tokio::test]
async fn send_without_api_key_returns_clean_error() {
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(
            &nim,
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
async fn send_streams_tokens_via_sse() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    kernel
        .send(&nim, json!({"type": "set_api_key", "api_key": "nvapi-x"}))
        .await;

    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer nvapi-x"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client_id = "test_client";
    let mut rx = open_client_inbox(&kernel, client_id);
    let send_kernel = Arc::clone(&kernel);
    let nim_for_task = nim.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(
                &nim_for_task,
                json!({"type": "send", "text": "hi", "client_id": client_id}),
            )
            .await
    });

    // Drain inbox; collect token events until `done` arrives or timeout.
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
                } else if ty == "done" {
                    saw_done = true;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let r = join.await.unwrap();
    assert!(saw_done, "expected a done event on the client inbox");
    let joined: String = tokens.join("");
    assert_eq!(joined, "hello", "tokens were {tokens:?}");
    assert_eq!(r["response"], "hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_retry_once_then_succeeds() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    kernel
        .send(&nim, json!({"type": "set_api_key", "api_key": "nvapi-x"}))
        .await;

    // First match: 429 with Retry-After=1 (one expected hit).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("rate limited"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // Then: success.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client_id = "test_rl";
    let mut rx = open_client_inbox(&kernel, client_id);
    let send_kernel = Arc::clone(&kernel);
    let nim_for_task = nim.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(
                &nim_for_task,
                json!({"type": "send", "text": "go", "client_id": client_id}),
            )
            .await
    });

    let mut saw_say = false;
    let mut saw_rate_status = false;
    let mut tokens = String::new();
    let mut saw_done = false;
    // 2s deadline: this test exercises the bundle's rate-limit retry path,
    // which clamps Retry-After to >=1s. 1s deadline is too tight to fit the
    // mandatory 1s wait + the retry's stream completion.
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
                    "say"
                        if ev
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|s| s.contains("rate limited"))
                            .unwrap_or(false) =>
                    {
                        saw_say = true;
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
    assert!(saw_say, "expected say(rate limited) event");
    assert!(
        saw_rate_status,
        "expected status(waiting_on=rate_limit) event"
    );
    assert_eq!(tokens, "ok");
    assert_eq!(r["response"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_argument_aggregation_across_chunks() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    kernel
        .send(&nim, json!({"type": "set_api_key", "api_key": "nvapi-x"}))
        .await;

    // First call (tool fires): two SSE chunks split the arguments
    // string. The model wants `send(target_id='core', payload={...})`.
    let args_part_1 = "{\\\"target_id\\\":\\\"co";
    let args_part_2 = "re\\\",\\\"payload\\\":{\\\"type\\\":\\\"list_agents\\\"}}";
    let sse_with_tool = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_x\",\"function\":{{\"name\":\"send\",\"arguments\":\"{}\"}}}}]}}}}]}}\n\n\
data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{}\"}}}}]}}}}]}}\n\n\
data: [DONE]\n\n",
        args_part_1, args_part_2,
    );
    // After tool runs, the model returns a final assistant turn.
    let sse_final = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n";

    // First request → SSE with tool call.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_with_tool),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // Second request → final answer.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_final),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client_id = "test_tool";
    let mut rx = open_client_inbox(&kernel, client_id);
    let send_kernel = Arc::clone(&kernel);
    let nim_for_task = nim.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(
                &nim_for_task,
                json!({"type": "send", "text": "do it", "client_id": client_id}),
            )
            .await
    });

    let mut tool_invoked = false;
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline && !saw_done {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => {
                let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
                if ty == "say" {
                    let text = ev.get("text").and_then(Value::as_str).unwrap_or("");
                    // Aggregated args should JSON-parse and yield a
                    // tool_call against "core" with payload list_agents.
                    if text.contains("[tool core ->") {
                        tool_invoked = true;
                    }
                } else if ty == "status" {
                    if let Some(tool) = ev.get("detail").and_then(|d| d.get("tool")) {
                        if let Some(args) = tool.get("args") {
                            // The args dict must be FULLY reconstructed.
                            if args.get("target_id").and_then(Value::as_str) == Some("core")
                                && args
                                    .get("payload")
                                    .and_then(|p| p.get("type"))
                                    .and_then(Value::as_str)
                                    == Some("list_agents")
                            {
                                tool_invoked = true;
                            }
                        }
                    }
                } else if ty == "done" {
                    saw_done = true;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let _ = join.await.unwrap();
    assert!(
        tool_invoked,
        "expected the aggregated tool_call args to JSON-parse + dispatch against 'core'",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_cancels_in_flight() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    kernel
        .send(&nim, json!({"type": "set_api_key", "api_key": "nvapi-x"}))
        .await;
    // Slow-loris SSE: server delays then streams. The interrupt path
    // should abort before tokens land.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"never\"}}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(Duration::from_secs(2))
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client_id = "test_int";
    let _rx = open_client_inbox(&kernel, client_id);
    let send_kernel = Arc::clone(&kernel);
    let nim_for_task = nim.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(
                &nim_for_task,
                json!({"type": "send", "text": "x", "client_id": client_id}),
            )
            .await
    });

    // Give the spawn a moment to acquire the lock and start the request.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = kernel.send(&nim, json!({"type": "interrupt"})).await;
    assert_eq!(r["interrupted"], true);
    let send_reply = tokio::time::timeout(Duration::from_secs(1), join)
        .await
        .expect("send must return promptly after interrupt")
        .unwrap();
    assert_eq!(send_reply["interrupted"], true);
}
