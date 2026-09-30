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
    let nim = nim_id_for(tmp);
    let fid = format!("ff_{}", nim);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type":"create_agent",
                "parent_id": "kernel",
                "handler_module":"file_bridge.tools", "allow": ["."],
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
    // Deny-all: its memory store (wired both ways) + its leg + `cli`.
    let memory = fantastic_ram_store::test_support::memory_meta(
        &kernel,
        &nim,
        json!({ fid.clone(): "*", "cli": "*" }),
    )
    .await;
    let mut create = json!({
        "type":"create_agent",
        "parent_id": "kernel",
        "handler_module":HANDLER_MODULE,
        "id": nim,
        "model": "test-model",
    });
    for (k, v) in memory {
        create[k] = v;
    }
    // REQUIRED — nothing is guessed; an unreachable one unless a test serves it.
    create["endpoint"] = json!(endpoint.unwrap_or_else(|| "http://127.0.0.1:1/v1".to_string()));
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
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
/// The AGENT's inbox, where per-client events land tagged with their
/// `client_id` — python's routing, and what a `watch` on the agent mirrors. It
/// used to open an inbox named after the CLIENT, which no watcher ever sees.
fn open_client_inbox(
    kernel: &Arc<Kernel>,
    agent_id: &AgentId,
) -> fantastic_kernel::inbox::InboxReceiver {
    kernel.ensure_inbox(agent_id)
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
        .join(
            "python/bundled_agents/ai/nvidia/nvidia_nim_backend/src/nvidia_nim_backend/readme.md",
        );
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
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

/// python `test_send_requires_api_key`: the refusal is `send`'s, minted after
/// the recursion guard, VERBATIM, and carrying the caller's `client_id`. Rust
/// minted it in the backend before the guard and without the `client_id`.
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
    assert_eq!(
        r,
        json!({
            "error": "nvidia_nim_backend: no api key — set NVIDIA_API_KEY",
            "client_id": "test",
        })
    );
    // THE GUARD RUNS FIRST — a cycle is refused even with no key wired.
    let cycled = kernel
        .send(vec![Item::new(
            nim.clone(),
            json!({"type": "send", "text": "hi", "client_id": "test",
                   "_call_stack": [nim.as_str()]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(cycled["error"], json!("nvidia_nim_backend: cycle detected"));
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
    let mut rx = open_client_inbox(&kernel, &nim);
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
                let ev = ev.payload;
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
    let mut out: VecDeque<Result<ProviderEvent, ProviderError>> = VecDeque::new();
    for d in deltas {
        decode_delta_into(d, &mut wrap, &mut out);
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
    let _rx = open_client_inbox(&kernel, &nim);
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
    // `history` is the DIGEST (python `{rows, next, total}`); the persisted
    // assistant turn is stripped of its deliberation.
    let rows = h["rows"].as_array().expect("rows array");
    assert_eq!(rows.last().unwrap()["head"], "ack");
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
    // `done` because a call-less turn is nudged: without it this stub answers the
    // nudge with the same body forever and `expect(1)` fails on the count rather
    // than on the retry this test is about.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok<call name=\\\"done\\\"></call>\"}}]}\n\ndata: [DONE]\n\n";
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
    let mut rx = open_client_inbox(&kernel, &nim);
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

    // NO `say`. python `_stream_with_rate_limit_retry` emits the wait as a
    // STATUS and nothing else; the extra `say` was a rust-only line in the
    // caller's stream.
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
                let ev = ev.payload;
                let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "token" => {
                        if let Some(t) = ev.get("text").and_then(Value::as_str) {
                            tokens.push_str(t);
                        }
                    }
                    "say" => saw_say = true,
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
    assert!(!saw_say, "python emits no say during a 429 backoff");
    assert!(
        saw_rate_status,
        "expected status(waiting_on=rate_limit) event"
    );
    assert_eq!(tokens, "ok<call name=\"done\"></call>");
    assert_eq!(r["response"], "ok<call name=\"done\"></call>");
}

/// A stub that writes ONE SSE line, HOLDS the connection for `stall`, then
/// finishes. A provider that drains the whole body before yielding cannot
/// produce a token inside that window; python's yields per line.
///
/// Raw TCP because the mock-server crates answer with a COMPLETE body — the one
/// thing this must not do.
async fn stalling_sse_server(first: String, rest: String, stall: Duration) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (first, rest) = (first.clone(), rest.clone());
            tokio::spawn(async move {
                // Drain the whole request (headers + Content-Length body), or the
                // client blocks writing a 20KB prompt into a socket nobody reads.
                let mut req: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let head_end = req.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                    if let Some(h) = head_end {
                        let head = String::from_utf8_lossy(&req[..h]).to_lowercase();
                        let len: usize = head
                            .split("content-length:")
                            .nth(1)
                            .and_then(|t| t.split("\r\n").next())
                            .and_then(|t| t.trim().parse().ok())
                            .unwrap_or(0);
                        if req.len() >= h + len {
                            break;
                        }
                    }
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                            Transfer-Encoding: chunked\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock
                    .write_all(format!("{:x}\r\n{first}\r\n", first.len()).as_bytes())
                    .await;
                let _ = sock.flush().await;
                tokio::time::sleep(stall).await;
                let _ = sock
                    .write_all(format!("{:x}\r\n{rest}\r\n0\r\n\r\n", rest.len()).as_bytes())
                    .await;
                let _ = sock.flush().await;
            });
        }
    });
    port
}

/// B2.11: TOKENS REACH THE CALLER WHILE THE BODY IS STILL OPEN. This provider
/// used to drain the whole response into a `Vec` and hand it over only once the
/// stream had ENDED, so `text_so_far`, the `token` events and the `streaming`
/// phase all arrived at once at the end — a long turn read as a stalled one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_and_streaming_status_arrive_before_the_body_ends() {
    let first =
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok<call name=\\\"done\\\"></call>\"}}]}\n\n"
            .to_string();
    let port = stalling_sse_server(
        first,
        "data: [DONE]\n\n".to_string(),
        Duration::from_secs(2),
    )
    .await;
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(format!("http://127.0.0.1:{port}/v1"))).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;
    let mut rx = open_client_inbox(&kernel, &nim);
    let send_kernel = Arc::clone(&kernel);
    let nim_for_task = nim.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(vec![Item::new(
                nim_for_task,
                json!({"type": "send", "text": "go", "client_id": "t"}),
            )])
            .await
            .remove(0)
            .result
    });

    // INSIDE the stall — the body has not ended yet.
    let mut token = String::new();
    let mut saw_streaming = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
    while tokio::time::Instant::now() < deadline && !(saw_streaming && !token.is_empty()) {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            let ev = ev.payload;
            match ev.get("type").and_then(Value::as_str) {
                Some("token") => token.push_str(ev["text"].as_str().unwrap_or("")),
                Some("status") if ev["phase"] == json!("streaming") => saw_streaming = true,
                _ => {}
            }
        }
    }
    assert!(
        !join.is_finished(),
        "the stub must still be holding the body open"
    );
    assert_eq!(token, "ok<call name=\"done\"></call>");
    assert!(
        saw_streaming,
        "status(streaming) must precede the body's end"
    );
    let r = join.await.unwrap();
    assert_eq!(r["response"], "ok<call name=\"done\"></call>");
}

/// PROVIDER FAILURE, KIND 1 (B2.1): MAPPED. nvidia is the one backend with an
/// `error_mapper` — an `httpx.HTTPStatusError` that survives the retry budget
/// becomes a clean sentence the caller READS on a COMPLETED turn, with its
/// `client_id`, and the handler does not fail. Every other failure raises.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mapped_rate_limit_is_an_answer_not_a_raise() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let endpoint = format!("{}/v1", server.uri());
    let (kernel, nim, _fid) = mk_kernel(&tmp, Some(endpoint)).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;

    // 429 forever: one retry is spent, then `_map_error`'s text is the reply.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let mut rx = open_client_inbox(&kernel, &nim);
    let r = kernel
        .send(vec![Item::new(
            nim.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], json!("send: rate limited (429); retry in 1s"));
    assert_eq!(r["client_id"], json!("t"));
    assert!(r.get("raised").is_none(), "a mapped error answers: {r:#?}");

    let mut done: Option<Value> = None;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        let ev = ev.payload;
        if ev.get("type").and_then(Value::as_str) == Some("status")
            && ev.get("phase").and_then(Value::as_str) == Some("done")
        {
            done = Some(ev["detail"].clone());
        }
    }
    let detail = done.expect("status(done)");
    assert_eq!(detail["reason"], json!("error"));
    assert_eq!(
        detail["error"],
        json!("send: rate limited (429); retry in 1s")
    );
}

/// UNMAPPED, same backend: a transport failure (the server refused the
/// connection) is not an HTTP status, so python's mapper returns None and the
/// raise goes through. A blanked `model` used to be this test's probe; it is
/// refused AT THE WRITE now (the kind's schema), so it never reaches a turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unmapped_nim_failure_still_raises() {
    let tmp = TempDir::new().unwrap();
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    let _key = KeyEnv::set(Some("nvapi-x")).await;
    let blanked = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": nim.as_str(), "model": ""}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(blanked["reason"], json!("invalid_meta"), "{blanked}");
    assert_eq!(
        blanked["error"],
        json!(
            "nvidia_nim_backend: required on the record and missing: model — nothing is \
             guessed. model = the model name"
        )
    );
    let r = kernel
        .send(vec![Item::new(
            nim.clone(),
            json!({"type": "send", "text": "go", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["raised"], json!(true), "{r:#?}");
    assert!(
        r["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("RuntimeError: http: error sending request"),
        "{r:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_envelope_split_across_sse_chunks_dispatches() {
    // A `<call>` split across two SSE chunks must REASSEMBLE and dispatch.
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
        "<call name=\"send\"><target_id>{fid}</target_id><reply_to>null</reply_to>\
         <type>mkdir</type><path>routed</path></call>"
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
    let _rx = open_client_inbox(&kernel, &nim);
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
    let mut rx = open_client_inbox(&kernel, &nim);
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
        .and_then(|t| t["id"].as_i64())
        .expect("the in-flight send must be in `ps`");
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
    // B2.3 — the cancel `status(done)` detail carries `started_at`. The entry is
    // already TAKEN by the hook, so `emit_status`'s auto-fill has nothing to
    // read: python passes it explicitly, and rust's cancel detail was the one
    // `done` event without it.
    let mut done: Option<Value> = None;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        let ev = ev.payload;
        if ev.get("type").and_then(Value::as_str) == Some("status")
            && ev.get("phase").and_then(Value::as_str) == Some("done")
        {
            done = Some(ev["detail"].clone());
        }
    }
    let detail = done.expect("status(done, cancelled)");
    assert_eq!(
        detail.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["reason", "send_id", "started_at", "queue_depth"],
        "{detail:#?}"
    );
    assert_eq!(detail["reason"], json!("cancelled"));
    assert!(detail["started_at"].as_f64().unwrap_or(0.0) > 0.0);
    // No phantom generation left behind — the hook cleared it.
    let st = kernel
        .send(vec![Item::new(nim.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["generating"], false);
}

/// NO guessy default (mirror of the Python provider test): an empty model never
/// reaches the wire — the request path fails loud, naming `model`.
#[tokio::test]
async fn an_empty_model_fails_loud_in_chat() {
    let kernel = Arc::new(Kernel::new());
    for m in [String::new()] {
        let p = NimProvider {
            self_id: AgentId::from("nim_x"),
            endpoint: "http://127.0.0.1:1/v1/chat/completions".to_string(),
            model: m,
            kernel: Arc::clone(&kernel),
            client_id: "cli".to_string(),
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
            err.contains("nvidia_nim_backend: no `model` set") && err.contains("model=..."),
            "unexpected error: {err}"
        );
    }
}

#[tokio::test]
async fn reflect_keys_are_python_keys_in_python_order() {
    // python `_make_reflect` + `_reflect_extra`: the shared base, then
    // `has_api_key` alone.
    let tmp = TempDir::new().unwrap();
    let _key = KeyEnv::set(Some("nvapi-x")).await;
    let (kernel, nim, _fid) = mk_kernel(&tmp, None).await;
    let r = kernel
        .send(vec![Item::new(nim, json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    let keys: Vec<String> = r.as_object().unwrap().keys().take(13).cloned().collect();
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
            "has_api_key",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>(),
        "{r:#?}"
    );
    assert_eq!(
        r["sentence"],
        "NVIDIA NIM-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling)."
    );
    assert_eq!(r["has_api_key"], true);
}
