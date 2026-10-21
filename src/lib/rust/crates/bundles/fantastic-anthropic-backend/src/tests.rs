//! anthropic_backend — python `tests/test_anthropic_backend.py`, case for case.

use super::*;
use fantastic_ai_core::credentials::CredentialError;
use fantastic_ai_core::think::{THINK_CLOSE, THINK_OPEN};
use fantastic_kernel::{Agent, Item};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "claude-test-5";

struct NoKey;
#[async_trait]
impl KeySource for NoKey {
    async fn key(&self, _e: &str) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }
}

struct Key;
#[async_trait]
impl KeySource for Key {
    async fn key(&self, _e: &str) -> Result<Option<String>, CredentialError> {
        Ok(Some("sk-test".into()))
    }
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("")))
        .collect()
}

fn text(t: &str) -> Value {
    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": t}})
}

fn think(t: &str) -> Value {
    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": t}})
}

fn start() -> Value {
    json!({"type": "message_start", "message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 20}}})
}

fn stop(reason: &str) -> [Value; 2] {
    [
        json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ]
}

async fn api(status: u16, body: String) -> (MockServer, String) {
    let srv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(status).set_body_raw(body, "text/event-stream"))
        .mount(&srv)
        .await;
    let ep = format!("{}/v1/messages", srv.uri());
    (srv, ep)
}

fn provider(endpoint: &str, key: Arc<dyn KeySource>) -> AnthropicProvider {
    provider_idle(
        endpoint,
        key,
        Duration::from_secs_f64(fantastic_ai_core::constants::STREAM_IDLE_SECONDS),
    )
}

/// The provider with a chosen read clock — a stall test shortens it.
fn provider_idle(
    endpoint: &str,
    key: Arc<dyn KeySource>,
    stream_idle: Duration,
) -> AnthropicProvider {
    AnthropicProvider {
        endpoint: endpoint.to_string(),
        model: MODEL.into(),
        key,
        max_tokens: 8000,
        temperature: None,
        thinking_budget: None,
        report: ReportCell::default(),
        stream_idle,
    }
}

/// A fake endpoint on a local socket: it reads the request, answers with `body`
/// as a chunked SSE response (or, with `None`, never answers at all), then holds
/// the connection open and sends nothing more. The hold is the stall under test;
/// the caller aborts the returned task when it is done.
async fn stalling_endpoint(body: Option<String>) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut req = vec![0u8; 65536];
        let _ = sock.read(&mut req).await;
        if let Some(body) = body {
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        transfer-encoding: chunked\r\n\r\n";
            sock.write_all(head.as_bytes()).await.expect("head");
            let chunk = format!("{:x}\r\n{body}\r\n", body.len());
            sock.write_all(chunk.as_bytes()).await.expect("chunk");
        }
        // Held open, silent — longer than any idle window a test uses.
        tokio::time::sleep(Duration::from_secs(600)).await;
    });
    (format!("http://{addr}/v1/messages"), task)
}

async fn drain(p: &AnthropicProvider, max_tokens: Option<i64>) -> Result<String, String> {
    let mut s = p
        .chat(&[json!({"role": "user", "content": "hi"})], max_tokens)
        .await
        .map_err(|e| e.message)?;
    let mut out = String::new();
    while let Some(ev) = s.next().await {
        match ev {
            Ok(ProviderEvent::Token(t)) => out.push_str(&t),
            Err(e) => return Err(e.message),
        }
    }
    Ok(out)
}

#[test]
fn system_is_one_string_and_turns_of_one_role_merge() {
    let (system, turns) = to_messages(&[
        json!({"role": "system", "content": "a"}),
        json!({"role": "user", "content": "u1"}),
        json!({"role": "system", "content": "b"}),
        json!({"role": "user", "content": "u2"}),
        json!({"role": "assistant", "content": "x"}),
    ]);
    assert_eq!(system, "a\n\nb");
    assert_eq!(
        turns,
        vec![
            json!({"role": "user", "content": "u1\n\nu2"}),
            json!({"role": "assistant", "content": "x"}),
        ]
    );
}

#[test]
fn the_body_sends_no_tools_and_only_what_the_record_set() {
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    let b = p.body(
        &[
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "hi"}),
        ],
        None,
    );
    assert_eq!(
        b,
        json!({"model": MODEL, "max_tokens": 8000, "messages": [{"role": "user", "content": "hi"}],
               "stream": true, "system": "s"})
    );
    p.temperature = Some(0.2);
    p.thinking_budget = Some(2048);
    let b = p.body(&[json!({"role": "user", "content": "hi"})], None);
    assert_eq!(b["temperature"], json!(0.2));
    assert_eq!(
        b["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    let b = p.body(&[json!({"role": "user", "content": "hi"})], Some(1500));
    assert_eq!(b["max_tokens"], json!(1500));
    assert!(b.get("thinking").is_none());
}

#[tokio::test]
async fn the_key_rides_as_x_api_key_only_when_named() {
    let mut events = vec![start(), text("ok")];
    events.extend(stop("end_turn"));
    let (srv, ep) = api(200, sse(&events)).await;
    drain(&provider(&ep, Arc::new(Key)), None)
        .await
        .expect("turn");
    drain(&provider(&ep, Arc::new(NoKey)), None)
        .await
        .expect("turn");
    let reqs = srv.received_requests().await.unwrap_or_default();
    assert_eq!(
        reqs[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("sk-test")
    );
    assert_eq!(
        reqs[0]
            .headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some(ANTHROPIC_VERSION)
    );
    assert!(reqs[1].headers.get("x-api-key").is_none());
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn the_thought_is_wrapped_once_and_the_usage_reported() {
    let mut events = vec![start(), think("let me "), think("see"), text("done")];
    events.extend(stop("end_turn"));
    let (_srv, ep) = api(200, sse(&events)).await;
    let p = provider(&ep, Arc::new(NoKey));
    assert_eq!(
        drain(&p, None).await,
        Ok(format!("{THINK_OPEN}let me see{THINK_CLOSE}done"))
    );
    let r = p.report();
    assert_eq!(
        (r.prompt_tokens, r.stop_reason.as_deref()),
        (Some(120), Some("end_turn"))
    );
}

/// A stream that sends its first events and then goes quiet: once the read clock's
/// idle window passes, the stream fails with `stream_stalled`, naming the seconds
/// and the endpoint — it does not wait for the send ceiling (#177).
#[tokio::test]
async fn a_stream_that_goes_quiet_fails_after_the_idle_window() {
    let (ep, server) = stalling_endpoint(Some(sse(&[start(), text("half")]))).await;
    let p = provider_idle(&ep, Arc::new(NoKey), Duration::from_millis(200));
    let started = std::time::Instant::now();
    let e = drain(&p, None).await.unwrap_err();
    server.abort();
    assert!(e.contains("stream_stalled"), "{e}");
    assert!(e.contains("sent no bytes for 0.2s"), "{e}");
    assert!(e.contains(&ep), "{e}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

/// Headers that never come are the same stall: the window runs from the request,
/// and the turn fails naming the stall, not a connect error (#177).
#[tokio::test]
async fn an_endpoint_that_never_answers_fails_after_the_idle_window() {
    let (ep, server) = stalling_endpoint(None).await;
    let p = provider_idle(&ep, Arc::new(NoKey), Duration::from_millis(200));
    let e = drain(&p, None).await.unwrap_err();
    server.abort();
    assert!(e.contains("stream_stalled"), "{e}");
    assert!(e.contains("sent no bytes for 0.2s"), "{e}");
}

#[test]
fn stream_idle_seconds_defaults_and_is_validated() {
    let meta = |v: Value| {
        let mut m = Map::new();
        m.insert("stream_idle_seconds".to_string(), v);
        m
    };
    let default = fantastic_ai_core::provider::stream_idle_of(&Map::new()).expect("default");
    assert_eq!(default, Duration::from_secs(60));
    let set = fantastic_ai_core::provider::stream_idle_of(&meta(json!(2.5))).expect("set");
    assert_eq!(set, Duration::from_millis(2500));
    for bad in [json!(0), json!(-1), json!("soon"), json!(true)] {
        let m = meta(bad.clone());
        assert!(validate(&m, &Map::new()).is_some(), "{bad} is refused");
        assert!(
            fantastic_ai_core::provider::stream_idle_of(&m).is_err(),
            "{bad} is refused at the read too"
        );
    }
}

#[tokio::test]
async fn a_stream_without_message_stop_did_not_finish() {
    let (_srv, ep) = api(200, sse(&[start(), text("half")])).await;
    let e = drain(&provider(&ep, Arc::new(NoKey)), None)
        .await
        .unwrap_err();
    assert!(e.contains("ended without message_stop"), "{e}");
}

#[tokio::test]
async fn an_error_event_carries_the_apis_own_message() {
    let err =
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    let (_srv, ep) = api(200, sse(&[start(), err])).await;
    let e = drain(&provider(&ep, Arc::new(NoKey)), None)
        .await
        .unwrap_err();
    assert!(
        e.contains("stream error: overloaded_error: Overloaded"),
        "{e}"
    );
}

#[tokio::test]
async fn a_refused_key_says_to_name_credentials() {
    let (_srv, ep) = api(401, "{\"type\":\"error\"}".into()).await;
    let e = drain(&provider(&ep, Arc::new(NoKey)), None)
        .await
        .unwrap_err();
    assert!(e.contains("name it in `credentials`"), "{e}");
}

#[tokio::test]
async fn a_cut_main_turn_fails_and_a_cut_side_call_is_its_answer() {
    let mut events = vec![start(), text("half")];
    events.extend(stop("max_tokens"));
    let (_srv, ep) = api(200, sse(&events)).await;
    let e = drain(&provider(&ep, Arc::new(NoKey)), None)
        .await
        .unwrap_err();
    assert!(e.contains("cut at max_tokens=8000"), "{e}");
    let p = provider(&ep, Arc::new(NoKey));
    assert_eq!(drain(&p, Some(500)).await, Ok("half".to_string()));
    assert_eq!(p.report().stop_reason.as_deref(), Some("max_tokens"));
}

// ─── the write ──────────────────────────────────────────────────────────────

fn rec(extra: Value) -> Map<String, Value> {
    let mut r = json!({"endpoint": "https://api.test/v1/messages", "model": MODEL, "memory_store_id": "memory"})
        .as_object()
        .cloned()
        .unwrap();
    r.extend(extra.as_object().cloned().unwrap_or_default());
    r
}

#[test]
fn endpoint_model_and_a_store_are_all_a_record_needs() {
    let required: Vec<&str> = record_schema()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["required"] == json!(true))
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(required, ["endpoint", "model", "memory_store_id"]);
    assert_eq!(validate(&rec(json!({})), &Map::new()), None);
}

#[test]
fn a_write_the_backend_cannot_honour_is_refused() {
    for (meta, fragment) in [
        (
            json!({"max_tokens": 0}),
            "max_tokens must be a positive int",
        ),
        (json!({"thinking_budget": 512}), "below the API's 1024"),
        (
            json!({"thinking_budget": 4000, "max_tokens": 4000}),
            "not below max_tokens=4000",
        ),
        (
            json!({"temperature": "hot"}),
            "temperature must be a number",
        ),
        (
            json!({"endpoint": "https://api.test/v1/chat"}),
            "max_tokens is left to the API, but the endpoint",
        ),
    ] {
        let err = validate(meta.as_object().unwrap(), &rec(json!({}))).unwrap_or_default();
        assert!(err.contains(fragment), "{meta}: {err}");
    }
}

// ─── the read ───────────────────────────────────────────────────────────────

async fn models(status: u16, entry: Value) -> (MockServer, String) {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/models/{MODEL}")))
        .respond_with(ResponseTemplate::new(status).set_body_string(entry.to_string()))
        .mount(&srv)
        .await;
    let ep = format!("{}/v1/messages", srv.uri());
    (srv, ep)
}

fn entry() -> Value {
    json!({"id": MODEL, "max_input_tokens": 200_000, "max_tokens": 64000})
}

#[tokio::test]
async fn what_the_record_leaves_out_is_read_from_the_model_entry() {
    let (srv, ep) = models(200, entry()).await;
    let got = server::read(&rec(json!({"endpoint": ep})), &Key)
        .await
        .expect("read");
    assert_eq!(
        Value::Object(got),
        json!({"max_tokens": 64000, "context_window": 200_000})
    );
    let reqs = srv.received_requests().await.unwrap_or_default();
    assert_eq!(
        reqs[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("sk-test")
    );
    let got = server::read(&rec(json!({"endpoint": ep, "max_tokens": 4000})), &NoKey)
        .await
        .expect("read");
    assert_eq!(Value::Object(got), json!({"context_window": 200_000}));
}

#[tokio::test]
async fn an_api_that_does_not_answer_refuses_naming_the_field() {
    let (srv, ep) = models(404, json!({})).await;
    let e = server::read(&rec(json!({"endpoint": ep})), &NoKey)
        .await
        .unwrap_err();
    assert_eq!(e.field, "max_tokens");
    assert_eq!(
        e.message,
        format!(
            "max_tokens: GET {}/v1/models/{MODEL} answered HTTP 404; set max_tokens on the record",
            srv.uri()
        )
    );
    let (_s, ep) = models(401, json!({})).await;
    let e = server::read(&rec(json!({"endpoint": ep})), &NoKey)
        .await
        .unwrap_err();
    assert!(
        e.message.contains("name it in `credentials`"),
        "{}",
        e.message
    );
    let (_s, ep) = models(200, json!({"id": MODEL})).await;
    let e = server::read(&rec(json!({"endpoint": ep, "max_tokens": 10})), &NoKey)
        .await
        .unwrap_err();
    assert_eq!(e.field, "context_window");
    assert!(
        e.message.contains("names no max_input_tokens"),
        "{}",
        e.message
    );
}

// ─── through the kernel ─────────────────────────────────────────────────────

async fn mk(tmp: &TempDir, endpoint: &str) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, AnthropicBundle);
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
    let mut payload = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE, "id": "a1"});
    for (k, v) in rec(json!({"endpoint": endpoint})) {
        payload[k] = v;
    }
    let r = call(&kernel, "kernel_state", payload).await;
    assert_eq!(r["id"], json!("a1"), "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_reads_the_api_first_and_a_no_turn_verb_reads_nothing() {
    let tmp = TempDir::new().unwrap();
    let (srv, ep) = models(200, entry()).await;
    let kernel = mk(&tmp, &ep).await;
    for verb in ["reflect", "status", "context_status", "rows"] {
        call(&kernel, "a1", json!({"type": verb})).await;
    }
    assert!(srv.received_requests().await.unwrap_or_default().is_empty());
    call(
        &kernel,
        "a1",
        json!({"type": "send", "text": "hi", "client_id": "t"}),
    )
    .await;
    let rec = helpers::agent_meta(&AgentId::from("a1"), &kernel);
    assert_eq!(
        (&rec["max_tokens"], &rec["context_window"]),
        (&json!(64000), &json!(200_000))
    );
}
