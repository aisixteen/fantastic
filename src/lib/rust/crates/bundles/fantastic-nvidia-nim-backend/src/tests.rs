//! Unit tests for `fantastic-nvidia-nim-backend`.
//!
//! Wiremock spins up a real HTTP server emitting SSE chunks; we drive
//! the bundle via `kernel.send` and assert on the file-agent-routed
//! sidecars + the per-client inbox.

use super::*;
use fantastic_ai_core::think::{THINK_CLOSE, THINK_OPEN};
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
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
    let nim = nim_id_for(tmp);
    let fid = format!("ff_{}", nim);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type":"create_agent",
                "parent_id": "kernel",
                "handler_module":"file_bridge.tools",
                "id": fid,
                "root": tmp.path().join(".fantastic").to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the backend that persists through it.
                "callable_by": { nim.clone(): "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    let store_grant = fid.clone();
    let mut create = json!({
        "type":"create_agent",
        "parent_id": "kernel",
        "handler_module":HANDLER_MODULE,
        "id": nim,
        "file_bridge_id": fid.clone(),
        "model": "test-model",
        // Deny-all: persists to its store directly + streams to `cli`.
        "can_call": { store_grant: "*", "cli": "*" },
    });
    if let Some(e) = endpoint {
        create["endpoint"] = json!(e);
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("core"), create)])
        .await
        .remove(0)
        .result;
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

/// THE KEY LIVES IN THE ENVIRONMENT, which is process-global — so the test that
/// needs it ABSENT and the ones that need it PRESENT must not overlap. Every
/// test touching the key takes this lock and restores what it found.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Set (or clear) the key var for the duration of a test, restoring on drop.
struct KeyEnv {
    prev: Option<String>,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl KeyEnv {
    async fn set(value: Option<&str>) -> Self {
        let guard = ENV_LOCK.lock().await;
        let prev = std::env::var("NVIDIA_API_KEY").ok();
        match value {
            Some(k) => std::env::set_var("NVIDIA_API_KEY", k),
            None => std::env::remove_var("NVIDIA_API_KEY"),
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
            Some(x) => std::env::set_var("NVIDIA_API_KEY", x),
            None => std::env::remove_var("NVIDIA_API_KEY"),
        }
    }
}

#[tokio::test]
async fn reflect_shape_has_api_key_tracks_the_env() {
    let _key = KeyEnv::set(None).await;
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(vec![Item::new(nim.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], nim.as_str());
    assert_eq!(r["has_api_key"], false);
    assert_eq!(r["model"], "test-model");
    // The key-storage verbs are GONE — the key is read from the environment and
    // this bundle never persists it.
    assert!(r["verbs"]["set_api_key"].is_null());
    assert!(r["verbs"]["clear_api_key"].is_null());
}

#[tokio::test]
async fn send_without_api_key_returns_clean_error() {
    let _key = KeyEnv::set(None).await;
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(vec![Item::new(
            nim.clone(),
            json!({"type": "send", "text": "hi", "client_id": "test"}),
        )])
        .await
        .remove(0)
        .result;
    let err = r["error"].as_str().unwrap_or("");
    assert!(
        err.contains("NVIDIA_API_KEY"),
        "expected api_key-not-set error, got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_streams_tokens_via_sse() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;

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
            .send(vec![Item::new(
                nim_for_task.clone(),
                json!({"type": "send", "text": "hi", "client_id": client_id}),
            )])
            .await
            .remove(0)
            .result
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
                } else if ty == "status" && ev.get("phase").and_then(Value::as_str) == Some("done")
                {
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

/// Join a decoded pass into the text ai-core would see.
fn decoded_text(deltas: &[Value]) -> String {
    let mut wrap = ThinkWrap::new();
    let mut out: Vec<Result<ProviderEvent, String>> = Vec::new();
    for d in deltas {
        decode_delta_into(d, &mut wrap, &mut out);
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
    // nemotron/deepseek-style NIM models stream most tokens as `reasoning_content`
    // and only the conclusion as `content`. THREE reasoning deltas + one content
    // delta must produce exactly ONE open and ONE close — per delta it would be a
    // confetti of tags, and ai-core's stripper would eat the answer with it.
    let text = decoded_text(&[
        json!({"reasoning_content": "step one "}),
        json!({"reasoning_content": "step two "}),
        json!({"reasoning_content": "step three"}),
        json!({"content": "the answer"}),
    ]);
    assert_eq!(text.matches(THINK_OPEN).count(), 1, "one open: {text:?}");
    assert_eq!(text.matches(THINK_CLOSE).count(), 1, "one close: {text:?}");
    assert_eq!(
        text,
        format!("{THINK_OPEN}step one step two step three{THINK_CLOSE}the answer")
    );
}

#[test]
fn a_reasoning_only_delta_is_not_an_empty_completion() {
    // Observed live: the model spent an entire pass on `reasoning_content`, we read
    // only `content`, and the chain died with no error anywhere. The span closes at
    // end-of-stream even though content never started.
    let text = decoded_text(&[json!({"reasoning_content": "740 chars of it"})]);
    assert_eq!(text, format!("{THINK_OPEN}740 chars of it{THINK_CLOSE}"));
    // A non-string / absent channel is shape, not content.
    let none = decoded_text(&[
        json!({"reasoning_content": null, "content": null}),
        json!({}),
    ]);
    assert_eq!(none, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_streams_wrapped_and_persists_stripped() {
    // End-to-end over the real SSE transport: the caller gets the wrapped raw
    // text, the durable turn keeps only the conclusion.
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;
    let l1 = json!({"choices":[{"delta":{"reasoning_content":"deliberating"}}]}).to_string();
    let l2 = json!({"choices":[{"delta":{"content":"ack"}}]}).to_string();
    let sse = format!("data: {l1}\n\ndata: {l2}\n\ndata: [DONE]\n\n");
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

    let client_id = "test_think";
    let _rx = open_client_inbox(&kernel, client_id);
    let reply = kernel
        .send(vec![Item::new(
            nim.clone(),
            json!({"type": "send", "text": "hi", "client_id": client_id}),
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
            nim.clone(),
            json!({"type": "history", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    let messages = h["messages"].as_array().expect("messages array");
    assert_eq!(messages.last().unwrap()["content"], "ack");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_retry_once_then_succeeds() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;

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
            .send(vec![Item::new(
                nim_for_task.clone(),
                json!({"type": "send", "text": "go", "client_id": client_id}),
            )])
            .await
            .remove(0)
            .result
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
async fn an_envelope_split_across_sse_chunks_dispatches() {
    // A `<tool_call>` split across two SSE chunks must REASSEMBLE and dispatch.
    // This test used to assert the opposite (`envelope_text_streams_verbatim_no_dispatch`
    // — "just TEXT, nothing parsed, nothing dispatched"). That was a gap, not a
    // principle: python's ai_core has dispatched this envelope since 2026-08-02 and
    // this crate's own header already said ai-core owns the parse. The transport's
    // remaining duty is the reassembly + verbatim delivery asserted below — an
    // envelope broken across chunk boundaries is the failure mode SSE actually has.
    //
    // `reply_to:null` keeps it to ONE pass: the stub answers with the same envelope
    // every time, so a default `reply_to` would (correctly) chain against a mock
    // that never says anything else.
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;

    // Address the nvidia agent's OWN file agent: its write lands on disk, which is
    // proof the dispatch happened and not just that nothing crashed.
    let whole = format!(
        "<tool_call>{{\"name\":\"send\",\"arguments\":{{\"target_id\":\"{fid}\",\
         \"payload\":{{\"type\":\"mkdir\",\"path\":\"routed\"}},\
         \"reply_to\":null}}}}</tool_call>"
    );
    let split = whole.len() / 2;
    let (part_1, part_2) = whole.split_at(split);
    let line_1 = json!({"choices":[{"delta":{"content": part_1}}]}).to_string();
    let line_2 = json!({"choices":[{"delta":{"content": part_2}}]}).to_string();
    let sse = format!("data: {line_1}\n\ndata: {line_2}\n\ndata: [DONE]\n\n");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1) // one pass: the call discards its reply, so nothing follows
        .mount(&server)
        .await;

    let client_id = "test_tool";
    let _rx = open_client_inbox(&kernel, client_id);
    let reply = kernel
        .send(vec![Item::new(
            nim.clone(),
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        routed.exists(),
        "the chunk-split envelope never dispatched: {routed:?} missing"
    );
}

/// The in-flight HTTP request is killed by the SUBSTRATE (`ps` → `cancel`), not
/// by a backend-private `interrupt`: the `send` dispatch IS a kernel task, and
/// killing it drops the reqwest stream. The bundle's `on_cancel` closes the turn
/// out first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kernel_cancel_kills_an_in_flight_send() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;
    // Slow-loris SSE: server delays then streams. The cancel path should drop
    // the request before tokens land.
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
            .send(vec![Item::new(
                nim_for_task.clone(),
                json!({"type": "send", "text": "x", "client_id": client_id}),
            )])
            .await
            .remove(0)
            .result
    });

    // Give the spawn a moment to acquire the lock and start the request.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let table = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    let task_id = table["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|t| t["agent"] == json!(nim.as_str()) && t["verb"] == json!("send"))
        .and_then(|t| t["id"].as_str())
        .expect("the in-flight send must be in `ps`")
        .to_string();
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": task_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["cancelled"], true);
    let send_reply = tokio::time::timeout(Duration::from_secs(1), join)
        .await
        .expect("send must return promptly after cancel")
        .unwrap();
    assert_eq!(send_reply, json!({"cancelled": true}));
    // No phantom generation left behind — the hook cleared it.
    let st = kernel
        .send(vec![Item::new(nim.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["generating"], false);
}

/// NO guessy default (mirror of the Python provider test): the sentinel
/// `DEFAULT_MODEL` is not a real id, and the request path fails loud — naming
/// `model` — rather than putting the sentinel (or an empty string) on the wire.
#[tokio::test]
async fn unset_model_is_the_sentinel_and_chat_fails_loud() {
    assert_eq!(DEFAULT_MODEL, "(no default — set the `model` meta)");
    let kernel = Arc::new(Kernel::new());
    for m in [DEFAULT_MODEL.to_string(), String::new()] {
        let p = NimProvider {
            self_id: AgentId::from("nim_x"),
            endpoint: "http://127.0.0.1:1/v1/chat/completions".to_string(),
            model: m,
            kernel: Arc::clone(&kernel),
            client_id: "cli".to_string(),
        };
        let err = match p.chat(&[json!({"role": "user", "content": "hi"})]).await {
            Ok(_) => panic!("chat must refuse an unset/sentinel model"),
            Err(e) => e,
        };
        assert!(
            err.contains("nvidia_nim_backend: no `model` set") && err.contains("model=..."),
            "unexpected error: {err}"
        );
    }
}
