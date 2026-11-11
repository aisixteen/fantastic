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

/// The first error a turn raises, whether the provider refuses at the send
/// or the stream fails mid-answer (#198).
async fn turn_error(p: &AnthropicProvider) -> ProviderError {
    let msgs = [json!({"role": "user", "content": "hi"})];
    match p.chat(&msgs, None).await {
        Err(e) => e,
        Ok(mut s) => {
            while let Some(ev) = futures_util::StreamExt::next(&mut s).await {
                if let Err(e) = ev {
                    return e;
                }
            }
            panic!("the turn raised no error");
        }
    }
}

#[tokio::test]
async fn connect_failure_is_transport() {
    let p = provider("http://127.0.0.1:1/v1/messages", Arc::new(Key));
    assert!(turn_error(&p).await.transport);
}

#[tokio::test]
async fn http_529_is_transport() {
    let (_srv, ep) = api(529, String::new()).await;
    let p = provider(&ep, Arc::new(Key));
    assert!(turn_error(&p).await.transport);
}

#[tokio::test]
async fn http_400_is_not_transport() {
    let (_srv, ep) = api(400, "bad request".into()).await;
    let p = provider(&ep, Arc::new(Key));
    assert!(!turn_error(&p).await.transport);
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
        effort: None,
        thinking_display: None,
        usage: Arc::new(Mutex::new(Usage::default())),
        base: Arc::new(Mutex::new(Usage::default())),
        usage_file: None,
        kernel: Arc::new(Kernel::new()),
        report: ReportCell::default(),
        stream_idle,
        long_context_tokens: None,
        usage_log: None,
        agent_id: "a1".into(),
        caller: None,
        images: Images::default(),
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
fn an_endpoint_and_a_store_are_all_a_record_needs() {
    let required: Vec<&str> = record_schema()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["required"] == json!(true))
        .filter_map(|f| f["name"].as_str())
        .collect();
    // the model is the chat's or the provider's `default_model`, not a required field
    assert_eq!(required, ["endpoint", "memory_store_id"]);
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
    mk_with(tmp, endpoint, json!({})).await
}

async fn mk_with(tmp: &TempDir, endpoint: &str, extra: Value) -> Arc<Kernel> {
    mk_with_id(tmp, endpoint, extra, "a1").await
}

async fn mk_with_id(tmp: &TempDir, endpoint: &str, extra: Value, id: &str) -> Arc<Kernel> {
    let mut kernel = Kernel::new();

    kernel.bundles.register(
        fantastic_ai_core::chat_bundle::HANDLER_MODULE,
        fantastic_ai_core::chat_bundle::AiChatBundle,
    );
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
    let mut payload = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE, "id": id});
    let mut record = rec(extra);
    record.insert("endpoint".into(), json!(endpoint));
    for (k, v) in record {
        payload[k] = v;
    }
    let r = call(&kernel, "kernel_state", payload).await;
    assert_eq!(r["id"], json!(id), "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    // a chat verb goes to an ai_chat completing through this provider
    let verb = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let to = if fantastic_ai_core::remote_provider::is_chat_verb(&verb) {
        fantastic_ai_core::remote_provider::pair_chat(kernel, &AgentId::from(target)).await
    } else {
        AgentId::from(target)
    };
    kernel
        .send(vec![Item::new(to, payload)])
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
    // the provider's turn (`complete`) reads the API first
    call(
        &kernel,
        "a1",
        json!({"type": "complete", "messages": [{"role": "user", "content": "hi"}], "rid": "r"}),
    )
    .await;
    let rec = helpers::agent_meta(&AgentId::from("a1"), &kernel);
    assert_eq!(
        (&rec["max_tokens"], &rec["context_window"]),
        (&json!(64000), &json!(200_000)),
        "{rec:?}"
    );
}

// ─── the bounded default (#495) ─────────────────────────────────────────────

/// A model entry whose maximum output is `max`.
fn entry_max(max: i64) -> Value {
    json!({"id": MODEL, "max_input_tokens": 200_000, "max_tokens": max})
}

/// A POST answer that says it was asked: the turn runs after `send` has replied, so
/// the test waits on this, not on a clock.
struct Asked(Arc<tokio::sync::Notify>, String);
impl wiremock::Respond for Asked {
    fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
        self.0.notify_one();
        ResponseTemplate::new(200).set_body_raw(self.1.clone(), "text/event-stream")
    }
}

/// The `max_tokens` a request carries when a record made from `extra` (its model
/// `model`) is sent to: the record is created, `send` reads the entry, and the POST's
/// body is read back off the wire.
async fn sent_max_tokens(model: &str, extra: Value, max: i64) -> Value {
    sent_body(model, extra, max, json!({})).await["max_tokens"].clone()
}

/// The whole POST body a turn sends. A non-empty `chat` object is set on the paired
/// ai_chat first (its `caching` places the marks).
async fn sent_body(model: &str, extra: Value, max: i64, chat: Value) -> Value {
    let tmp = TempDir::new().unwrap();
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/models/{model}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({"id": model, "max_input_tokens": 200_000, "max_tokens": max}).to_string(),
        ))
        .mount(&srv)
        .await;
    let mut events = vec![start(), text("<call name=\"done\"></call>")];
    events.extend(stop("end_turn"));
    let asked = Arc::new(tokio::sync::Notify::new());
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(Asked(Arc::clone(&asked), sse(&events)))
        .mount(&srv)
        .await;
    let ep = format!("{}/v1/messages", srv.uri());
    let mut extra = extra;
    extra["model"] = json!(model);
    // process-wide per-agent state (usage tables) is keyed by id: one id per test
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = format!(
        "bound{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let kernel = mk_unbounded_store(&tmp, &ep, &id, extra).await;
    if chat.as_object().is_some_and(|o| !o.is_empty()) {
        let c = fantastic_ai_core::remote_provider::pair_chat(&kernel, &AgentId::from(id.as_str()))
            .await;
        let mut payload = json!({"type": "update_agent", "id": c.as_str()});
        payload
            .as_object_mut()
            .expect("object")
            .extend(chat.as_object().expect("object").clone());
        let r = call(&kernel, "kernel_state", payload).await;
        assert!(r.get("error").is_none(), "update_agent refused: {r}");
    }
    call(
        &kernel,
        &id,
        json!({"type": "send", "text": "hi", "client_id": "t"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(30), asked.notified())
        .await
        .expect("the turn sent its request");
    let reqs = srv.received_requests().await.unwrap_or_default();
    let post = reqs
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .expect("a request was sent");
    serde_json::from_slice(&post.body).expect("json body")
}

/// `mk_with`, with a memory store the agent may use — a turn needs one to run — and no
/// `max_tokens` unless `extra` names one.
async fn mk_unbounded_store(tmp: &TempDir, endpoint: &str, id: &str, extra: Value) -> Arc<Kernel> {
    let mut kernel = Kernel::new();

    kernel.bundles.register(
        fantastic_ai_core::chat_bundle::HANDLER_MODULE,
        fantastic_ai_core::chat_bundle::AiChatBundle,
    );
    kernel.bundles.register(HANDLER_MODULE, AnthropicBundle);
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
    let mut payload = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE, "id": id});
    let mut record = rec(extra);
    record.insert("endpoint".into(), json!(endpoint));
    record.extend(fantastic_ram_store::test_support::memory_meta(&kernel, id, json!({})).await);
    for (k, v) in record {
        payload[k] = v;
    }
    let r = call(&kernel, "kernel_state", payload).await;
    assert_eq!(r["id"], json!(id), "{r}");
    kernel
}

/// A model whose id the budget-thinking refusal does not cover.
const OLD: &str = "claude-3-5-sonnet";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chats_caching_places_the_marks_and_the_provider_forwards_them() {
    // the provider record is in cleanup, the chat in provider_cache: the chat decides
    let b = sent_body(
        MODEL,
        json!({}),
        64000,
        json!({"caching": "provider_cache"}),
    )
    .await;
    assert_eq!(
        b["system"][0]["cache_control"],
        json!({"type": "ephemeral"}),
        "{b}"
    );
    // the provider record is in provider_cache, the chat in none: no mark is sent
    let b = sent_body(
        MODEL,
        json!({"caching": "provider_cache"}),
        64000,
        json!({"caching": "none"}),
    )
    .await;
    assert!(b["system"].is_string(), "{b}");
    assert_eq!(markers(&b), 0, "{b}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_with_no_max_tokens_sends_the_bounded_default() {
    assert_eq!(sent_max_tokens(MODEL, json!({}), 64000).await, json!(64000));
    assert_eq!(
        sent_max_tokens(MODEL, json!({}), 1_000_000).await,
        json!(260_000)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thinking_budget_adds_to_the_default() {
    assert_eq!(
        sent_max_tokens(OLD, json!({"thinking_budget": 2048}), 1_000_000).await,
        json!(2048 + 260_000)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_max_tokens_wins() {
    assert_eq!(
        sent_max_tokens(MODEL, json!({"max_tokens": 40000}), 64000).await,
        json!(40000)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_is_never_above_the_models_own_maximum() {
    assert_eq!(sent_max_tokens(MODEL, json!({}), 8000).await, json!(8000));
    assert_eq!(
        sent_max_tokens(OLD, json!({"thinking_budget": 4000}), 10000).await,
        json!(10000)
    );
}

/// A model that never stops of its own accord: it answers with one delta per 1024
/// tokens of the request's `max_tokens`, then ends on `max_tokens` — the API's own
/// cut at the bound it was given.
struct Runaway;
impl wiremock::Respond for Runaway {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let bound = body["max_tokens"].as_i64().unwrap_or(0);
        let mut events = vec![start()];
        events.extend((0..bound / 1024).map(|_| text("\n</invoke")));
        events.extend(stop("max_tokens"));
        ResponseTemplate::new(200).set_body_raw(sse(&events), "text/event-stream")
    }
}

#[tokio::test]
async fn a_stream_that_never_stops_is_cut_by_the_bound_long_before_the_send_timeout() {
    let srv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(Runaway)
        .mount(&srv)
        .await;
    let ep = format!("{}/v1/messages", srv.uri());
    let (_m, models_ep) = models(200, entry_max(64000)).await;
    let read = server::read(&rec(json!({"endpoint": models_ep})), &NoKey)
        .await
        .expect("read");
    let mut p = provider(&ep, Arc::new(NoKey));
    p.max_tokens = read["max_tokens"].as_i64().unwrap();
    let started = std::time::Instant::now();
    let e = drain(&p, None).await.unwrap_err();
    assert!(e.contains("cut at max_tokens=64000"), "{e}");
    assert_eq!(p.report().stop_reason.as_deref(), Some("max_tokens"));
    assert!(
        started.elapsed().as_secs_f64() < fantastic_ai_core::constants::SEND_TIMEOUT_SECS / 10.0,
        "took {:?}",
        started.elapsed()
    );
}

// ─── prompt caching and usage ───────────────────────────────────────────────

fn plain() -> Vec<Value> {
    vec![
        json!({"role": "system", "content": "s"}),
        json!({"role": "user", "content": "hi"}),
    ]
}

fn usage_start(input: i64, write: i64, read: i64) -> Value {
    json!({"type": "message_start", "message": {"usage": {
        "input_tokens": input,
        "cache_creation_input_tokens": write,
        "cache_read_input_tokens": read,
    }}})
}

fn usage_start_as(model: &str, input: i64, write: i64, read: i64) -> Value {
    let mut v = usage_start(input, write, read);
    v["message"]["model"] = json!(model);
    v
}

fn usage_stop(out: i64) -> [Value; 2] {
    [
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": out}}),
        json!({"type": "message_stop"}),
    ]
}

fn fresh_usage() -> Arc<Mutex<Usage>> {
    Arc::new(Mutex::new(Usage::default()))
}

/// The number of `cache_control` markers a body sends.
fn markers(body: &Value) -> usize {
    body.to_string().matches("\"cache_control\"").count()
}

/// The conversation's `messages` as ai-core flags them, with an unmarked system row
/// first. A flag names the key set to true on a row (`cache_mark`).
fn flagged(rows: &[(&str, &str, Option<&str>)]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": "S"})];
    for (role, text, flag) in rows {
        let mut m = json!({"role": role, "content": text});
        if let Some(f) = flag {
            m[*f] = json!(true);
        }
        out.push(m);
    }
    out
}

/// `flagged`, with `cache_mark` on the system row.
fn flagged_sys(rows: &[(&str, &str, Option<&str>)]) -> Vec<Value> {
    let mut out = flagged(rows);
    out[0]["cache_mark"] = json!(true);
    out
}

#[test]
fn the_backend_adds_no_mark_of_its_own() {
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let expected = json!({"model": MODEL, "max_tokens": 8000, "messages": [{"role": "user", "content": "hi"}],
                          "stream": true, "system": "s"});
    assert_eq!(p.body(&plain(), None), expected);
    let b = p.body(
        &flagged(&[("user", "a", None), ("assistant", "b", None)]),
        None,
    );
    assert_eq!(markers(&b), 0, "{b}");
    assert_eq!(b["system"], json!("S"), "the plain system string");
    assert_eq!(
        b["messages"],
        json!([
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
        ])
    );
}

#[test]
fn every_agent_mark_is_forwarded_where_it_was_put() {
    // [primer rows][fold][fold][task rows]: the primer's row is the assistant turn
    // "p2"; the fold rows and the task rows are one merged user turn, and the marker
    // lands at the end of the last fold's text, not at the end of that merged turn.
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let msgs = flagged_sys(&[
        ("user", "p1", None),
        ("assistant", "p2", Some("cache_mark")),
        ("user", "f1", None),
        ("user", "f2", Some("cache_mark")),
        ("user", "t1", None),
        ("assistant", "t2", None),
        ("user", "live", None),
    ]);
    let b = p.body(&msgs, None);
    let marked = json!({"type": "ephemeral"});
    assert_eq!(b["system"][0]["cache_control"], marked, "M1: {b}");
    assert_eq!(
        b["messages"],
        json!([
            {"role": "user", "content": "p1"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "p2", "cache_control": marked},
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "f1\n\nf2", "cache_control": marked},
                {"type": "text", "text": "\n\nt1"},
            ]},
            {"role": "assistant", "content": "t2"},
            {"role": "user", "content": "live"},
        ]),
        "M2 ends the primer's text, M3 ends the last fold's text: {b}"
    );
    assert_eq!(markers(&b), 3, "M1, M2 and M3: {b}");
}

#[test]
fn the_split_turns_concatenate_to_exactly_the_merged_string() {
    // The block texts of a merged turn, joined, are the string the turn was before
    // any split: no byte added or lost, wherever a flag falls. And no block is blank:
    // the API refuses a whitespace-only text block, so a flagged row followed by empty
    // rows must not leave a trailing "\n\n" block of its own.
    let cases: [(&[&str], &[Option<&str>]); 12] = [
        (&["one", "", "three"], &[Some("cache_mark"), None, None]),
        (&["one", "", "three"], &[None, Some("cache_mark"), None]),
        (
            &["one", "", "three"],
            &[None, Some("cache_mark"), Some("cache_mark")],
        ),
        (
            &["one", "", "three"],
            &[Some("cache_mark"), Some("cache_mark"), None],
        ),
        (&["one", "", "three"], &[None, None, Some("cache_mark")]),
        (&["one", ""], &[Some("cache_mark"), None]),
        (&["one", "", ""], &[Some("cache_mark"), None, None]),
        (&["", "one", ""], &[None, Some("cache_mark"), None]),
        (&["one", ""], &[Some("cache_mark"), Some("cache_mark")]),
        // a flagged row that is only whitespace, first in its turn, opens no block
        // of its own (the API refuses a blank block): it joins the next text
        (&[" ", "two"], &[Some("cache_mark"), None]),
        (
            &[" ", "two", "three"],
            &[Some("cache_mark"), Some("cache_mark"), None],
        ),
        // ...and after text, it closes the block it is part of, bytes unchanged
        (&["one", " "], &[None, Some("cache_mark")]),
    ];
    for (texts, shape) in cases {
        let rows: Vec<(&str, &str, Option<&str>)> = texts
            .iter()
            .zip(shape)
            .map(|(t, f)| ("user", *t, *f))
            .collect();
        let msgs = flagged(&rows);
        let p = provider("http://x/v1/messages", Arc::new(NoKey));
        let b = p.body(&msgs, None);
        let plain = to_messages(&msgs).1;
        let merged = plain[0]["content"]
            .as_str()
            .expect("plain turn")
            .to_string();
        let blocks = b["messages"][0]["content"].clone();
        let joined: String = match &blocks {
            Value::String(s) => s.clone(),
            Value::Array(a) => {
                for block in a {
                    let text = block["text"].as_str().unwrap();
                    assert!(
                        !text.trim().is_empty(),
                        "shape {shape:?}: a blank block is refused by the API: {b}"
                    );
                }
                a.iter().map(|x| x["text"].as_str().unwrap()).collect()
            }
            other => panic!("unexpected content {other}"),
        };
        assert_eq!(joined, merged, "shape {shape:?}: {b}");
        assert!(markers(&b) <= 4, "shape {shape:?}: {b}");
    }
}

#[test]
fn a_flagged_row_followed_by_empty_rows_keeps_its_marker_and_no_blank_block() {
    // The trailing blank separator joins the flagged row's block: the marker lands
    // after the "\n\n" that followed "one", and the bytes are the merged string.
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let msgs = flagged(&[("user", "one", Some("cache_mark")), ("user", "", None)]);
    let b = p.body(&msgs, None);
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "one\n\n", "cache_control": {"type": "ephemeral"}},
        ]}]),
        "{b}"
    );
    assert_eq!(markers(&b), 1, "{b}");
}

#[test]
fn a_flagged_row_with_no_text_carries_no_marker() {
    // An empty block is refused by the API, so an empty flagged row opens none.
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let msgs = flagged(&[("user", "a", None), ("user", "", Some("cache_mark"))]);
    let b = p.body(&msgs, None);
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "a\n\n"}]}]),
        "{b}"
    );
    assert_eq!(markers(&b), 0, "{b}");
}

#[test]
fn a_marked_system_message_marks_only_the_system_block() {
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let b = p.body(
        &flagged_sys(&[("user", "a", None), ("assistant", "b", None)]),
        None,
    );
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}])
    );
    assert_eq!(markers(&b), 1, "M1: {b}");
}

#[test]
fn a_marked_system_message_is_one_marked_block() {
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.temperature = Some(0.2);
    p.thinking_budget = Some(2048);
    let msgs = vec![
        json!({"role": "system", "content": "s", "cache_mark": true}),
        json!({"role": "user", "content": "hi"}),
    ];
    assert_eq!(
        p.body(&msgs, None),
        json!({
            "model": MODEL,
            "max_tokens": 8000,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "temperature": 0.2,
            "thinking": {"type": "enabled", "budget_tokens": 2048},
        })
    );
    // a side call's own bound is marked the same way
    let b = p.body(&msgs, Some(1500));
    assert_eq!(b["max_tokens"], json!(1500));
    assert_eq!(markers(&b), 1, "{b}");
}

#[test]
fn two_system_messages_one_marked_send_one_marked_block() {
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let b = p.body(
        &[
            json!({"role": "system", "content": "a"}),
            json!({"role": "system", "content": "b", "cache_mark": true}),
            json!({"role": "user", "content": "hi"}),
        ],
        None,
    );
    assert_eq!(
        b["system"],
        json!([{"type": "text", "text": "a\n\nb", "cache_control": {"type": "ephemeral"}}])
    );
}

#[test]
fn an_empty_marked_system_sends_no_system_block() {
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let b = p.body(
        &[
            json!({"role": "system", "content": "", "cache_mark": true}),
            json!({"role": "user", "content": "hi"}),
        ],
        None,
    );
    assert!(b.get("system").is_none(), "{b}");
    assert_eq!(markers(&b), 0, "{b}");
}

#[test]
fn a_tail_flagged_row_is_marked_and_the_backend_adds_no_mark_of_its_own() {
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let b = p.body(
        &flagged(&[
            ("user", "t1", None),
            ("assistant", "t2", Some("cache_mark")),
            ("user", "live", None),
        ]),
        None,
    );
    assert_eq!(
        b["messages"][1]["content"],
        json!([{"type": "text", "text": "t2", "cache_control": {"type": "ephemeral"}}]),
        "{b}"
    );
    assert_eq!(markers(&b), 1, "the tail row only: {b}");
}

#[tokio::test]
async fn a_request_over_the_long_context_line_is_billed_long() {
    let mut events = vec![usage_start(50, 50, 50), text("ok")];
    events.extend(usage_stop(10));
    let (_s, ep) = api(200, sse(&events)).await;
    let mut p = provider(&ep, Arc::new(NoKey));
    p.usage = fresh_usage();
    p.long_context_tokens = Some(100);
    drain(&p, None).await.unwrap();
    let u = p.usage.lock().unwrap();
    assert_eq!(u.long_requests, 1);
    assert_eq!(u.requests, 0);
    let s = u.snapshot(
        "",
        &Prices {
            input: Some(1.0),
            output: Some(5.0),
            input_long: Some(2.0),
            output_long: Some(10.0),
            ..Prices::default()
        },
    );
    assert_eq!(s["long_context_requests"], json!(1));
    let cost = (50.0 + 1.25 * 50.0 + 0.1 * 50.0) * 2.0 / 1e6 + 10.0 * 10.0 / 1e6;
    assert!((s["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-12);
}

#[tokio::test]
async fn the_report_carries_the_cache_counts_the_stream_names() {
    let mut events = vec![usage_start(50, 40, 7), text("ok")];
    events.extend(usage_stop(10));
    let (_s, ep) = api(200, sse(&events)).await;
    let p = provider(&ep, Arc::new(NoKey));
    drain(&p, None).await.unwrap();
    let r = p.report();
    assert_eq!(r.prompt_tokens, Some(97));
    assert_eq!(r.completion_tokens, Some(10));
    assert_eq!(r.cache_read, Some(7));
    assert_eq!(r.cache_write, Some(40));
    // a pass is a fresh reading
    p.clear_prompt_tokens();
    assert_eq!(
        (p.report().cache_read, p.report().cache_write),
        (None, None)
    );
}

#[tokio::test]
async fn a_stream_without_cache_counts_leaves_them_absent() {
    let mut events = vec![
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
        text("ok"),
    ];
    events.extend(usage_stop(10));
    let (_s, ep) = api(200, sse(&events)).await;
    let p = provider(&ep, Arc::new(NoKey));
    drain(&p, None).await.unwrap();
    let r = p.report();
    assert_eq!(r.prompt_tokens, Some(100));
    assert_eq!((r.cache_read, r.cache_write), (None, None));
}

#[tokio::test]
async fn a_request_under_the_line_is_billed_base() {
    let mut events = vec![usage_start(50, 40, 0), text("ok")];
    events.extend(usage_stop(10));
    let (_s, ep) = api(200, sse(&events)).await;
    let mut p = provider(&ep, Arc::new(NoKey));
    p.usage = fresh_usage();
    p.long_context_tokens = Some(100);
    drain(&p, None).await.unwrap();
    let u = p.usage.lock().unwrap();
    assert_eq!(u.long_requests, 0);
    assert_eq!(u.requests, 1);
    let s = u.snapshot(
        "",
        &Prices {
            input: Some(1.0),
            output: Some(5.0),
            input_long: Some(2.0),
            output_long: Some(10.0),
            ..Prices::default()
        },
    );
    assert_eq!(s["long_context_requests"], json!(0));
    let cost = (50.0 + 1.25 * 40.0) * 1.0 / 1e6 + 10.0 * 5.0 / 1e6;
    assert!((s["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-12);
}

#[tokio::test]
async fn without_long_prices_the_long_bucket_uses_the_base_prices() {
    let mut events = vec![usage_start(50, 50, 50), text("ok")];
    events.extend(usage_stop(10));
    let (_s, ep) = api(200, sse(&events)).await;
    let mut p = provider(&ep, Arc::new(NoKey));
    p.usage = fresh_usage();
    p.long_context_tokens = Some(100);
    drain(&p, None).await.unwrap();
    let u = p.usage.lock().unwrap();
    let s = u.snapshot(
        "",
        &Prices {
            input: Some(1.0),
            output: Some(5.0),
            input_long: None,
            output_long: None,
            ..Prices::default()
        },
    );
    let cost = (50.0 + 1.25 * 50.0 + 0.1 * 50.0) * 1.0 / 1e6 + 10.0 * 5.0 / 1e6;
    assert!((s["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-12);
}

#[tokio::test]
async fn the_usage_log_gets_one_line_per_request() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("usage.jsonl");
    let mut first = vec![usage_start(100, 400, 1000), text("a")];
    first.extend(usage_stop(35));
    let mut second = vec![usage_start(50, 0, 900), text("b")];
    second.extend(usage_stop(20));
    let (_s1, ep1) = api(200, sse(&first)).await;
    let (_s2, ep2) = api(200, sse(&second)).await;
    let mut p = provider(&ep1, Arc::new(NoKey));
    p.usage = fresh_usage();
    p.usage_log = Some(path.clone());
    p.agent_id = "a1".into();
    drain(&p, None).await.unwrap();
    p.endpoint = ep2;
    drain(&p, None).await.unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["input_tokens"], json!(100));
    assert_eq!(lines[0]["output_tokens"], json!(35));
    assert_eq!(lines[1]["input_tokens"], json!(50));
    assert_eq!(lines[1]["cache_read_tokens"], json!(900));
    assert_eq!(lines[1]["output_tokens"], json!(20));
    assert_eq!(lines[1]["long_context"], json!(false));
    assert_eq!(lines[1]["agent"], json!("a1"));
}

#[test]
fn the_new_fields_are_validated() {
    let rec = serde_json::Map::new();
    let check = |m: Value| validate(m.as_object().unwrap(), &rec);
    assert_eq!(
        check(json!({"long_context_tokens": 0})).as_deref(),
        Some("long_context_tokens must be a positive int")
    );
    assert_eq!(
        check(json!({"long_context_tokens": "x"})).as_deref(),
        Some("long_context_tokens must be a positive int")
    );
    assert_eq!(
        check(json!({"price_input_long_per_mtok": -1})).as_deref(),
        Some("price_input_long_per_mtok must be a number, zero or more")
    );
    assert_eq!(
        check(json!({"usage_log": ""})).as_deref(),
        Some("usage_log must be a non-empty string")
    );
    assert_eq!(
        check(json!({"price_cache_write_1h_per_mtok": -1})).as_deref(),
        Some("price_cache_write_1h_per_mtok must be a number, zero or more")
    );
}

#[tokio::test]
async fn the_stream_sets_last_prompt_tokens_and_the_counts() {
    let mut events = vec![usage_start(100, 400, 1000), text("ok")];
    events.extend(usage_stop(35));
    let (_s, ep) = api(200, sse(&events)).await;
    let mut p = provider(&ep, Arc::new(NoKey));
    p.usage = fresh_usage();
    assert_eq!(drain(&p, None).await.unwrap(), "ok");
    let report = p.report();
    assert_eq!(report.prompt_tokens, Some(1500));
    assert_eq!(report.completion_tokens, Some(35));
    let u = p.usage.lock().unwrap();
    assert_eq!(
        (u.requests, u.input, u.cache_write, u.cache_read, u.output),
        (1, 100, 400, 1000, 35)
    );
}

#[tokio::test]
async fn two_passes_accumulate_and_a_side_call_counts_too() {
    let mut first = vec![usage_start(100, 400, 0), text("a")];
    first.extend(usage_stop(10));
    let mut second = vec![usage_start(50, 0, 900), text("b")];
    second.extend(usage_stop(20));
    let (_s1, ep1) = api(200, sse(&first)).await;
    let (_s2, ep2) = api(200, sse(&second)).await;
    let usage = fresh_usage();
    let mut p = provider(&ep1, Arc::new(NoKey));
    p.usage = Arc::clone(&usage);
    drain(&p, None).await.unwrap();
    p.endpoint = ep2;
    drain(&p, Some(500)).await.unwrap();
    let u = usage.lock().unwrap();
    assert_eq!(
        (u.requests, u.input, u.cache_write, u.cache_read, u.output),
        (2, 150, 400, 900, 30)
    );
}

#[test]
fn hit_rate_and_saved_tokens_follow_the_formulas() {
    let u = Usage {
        requests: 1,
        input: 100,
        cache_write: 400,
        cache_read: 1000,
        output: 0,
        ..Usage::default()
    };
    let s = u.snapshot("", &Prices::default());
    assert_eq!(s["prompt_tokens"], json!(1500));
    assert_eq!(s["saved_tokens"], json!(800)); // 0.9*1000 - 0.25*400
    let hit = s["hit_rate"].as_f64().unwrap();
    assert!((hit - 1000.0 / 1500.0).abs() < 1e-12, "{hit}");
    assert!(s.get("cost_usd").is_none() && s.get("saved_usd").is_none());
    // a write that is never read is a loss: negative, reported, never clamped
    let w = Usage {
        input: 10,
        cache_write: 400,
        ..Usage::default()
    };
    let s = w.snapshot("", &Prices::default());
    assert_eq!(s["saved_tokens"], json!(-100));
    assert_eq!(s["hit_rate"].as_f64(), Some(0.0));
    // nothing counted: no prompt, no hit rate
    assert_eq!(
        Usage::default().snapshot("", &Prices::default())["hit_rate"],
        json!(0)
    );
    // halves round to even
    assert_eq!(saved_tokens(5, 0), 4);
    assert_eq!(saved_tokens(0, 2), 0);
    assert_eq!(saved_tokens(0, 3), -1);
    assert_eq!(saved_tokens(1, 0), 1);
}

#[test]
fn prices_on_the_record_add_cost_and_saving_in_usd() {
    let u = Usage {
        requests: 1,
        input: 100,
        cache_write: 400,
        cache_read: 1000,
        output: 50,
        ..Usage::default()
    };
    let s = u.snapshot(
        "",
        &Prices {
            input: Some(3.0),
            output: Some(15.0),
            input_long: None,
            output_long: None,
            ..Prices::default()
        },
    );
    let cost = (100.0 + 1.25 * 400.0 + 0.1 * 1000.0) * 3.0 / 1e6 + 50.0 * 15.0 / 1e6;
    assert!((s["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-12);
    assert!((s["saved_usd"].as_f64().unwrap() - 800.0 * 3.0 / 1e6).abs() < 1e-12);
    // one price without the other: neither field
    let s = u.snapshot(
        "",
        &Prices {
            input: Some(3.0),
            output: None,
            input_long: None,
            output_long: None,
            ..Prices::default()
        },
    );
    assert!(s.get("cost_usd").is_none() && s.get("saved_usd").is_none());
}

#[test]
fn sonnet_cache_reads_bill_at_a_twentieth_of_input() {
    let u = Usage {
        requests: 1,
        cache_read: 1_000_000,
        ..Usage::default()
    };
    let p = Prices {
        input: Some(3.0),
        output: Some(15.0),
        input_long: None,
        output_long: None,
        ..Prices::default()
    };
    let s = u.snapshot("claude-sonnet-5-5", &p);
    assert!(
        (s["cost_usd"].as_f64().unwrap() - 0.15).abs() < 1e-12,
        "{s}"
    );
    // Opus 5.5 too, and a dated id is the same model
    let s = u.snapshot("claude-opus-5-5-20260601", &p);
    assert!(
        (s["cost_usd"].as_f64().unwrap() - 0.15).abs() < 1e-12,
        "{s}"
    );
}

#[test]
fn haiku_cache_reads_bill_at_a_tenth() {
    let u = Usage {
        requests: 1,
        cache_read: 1_000_000,
        ..Usage::default()
    };
    let p = Prices {
        input: Some(1.0),
        output: Some(5.0),
        input_long: None,
        output_long: None,
        ..Prices::default()
    };
    let s = u.snapshot("claude-haiku-5-5", &p);
    assert!(
        (s["cost_usd"].as_f64().unwrap() - 0.10).abs() < 1e-12,
        "{s}"
    );
    // the long-context bucket: every rate x5 with its own input price
    let u = Usage {
        requests: 1,
        long_requests: 1,
        long_cache_read: 1_000_000,
        ..Usage::default()
    };
    let p = Prices {
        input: Some(1.0),
        output: Some(5.0),
        input_long: Some(5.0),
        output_long: Some(25.0),
        ..Prices::default()
    };
    let s = u.snapshot("claude-haiku-5-5", &p);
    assert!(
        (s["cost_usd"].as_f64().unwrap() - 0.50).abs() < 1e-12,
        "{s}"
    );
}

#[test]
fn one_hour_cache_writes_bill_at_twice_input() {
    let p = Prices {
        input: Some(3.0),
        output: Some(15.0),
        input_long: None,
        output_long: None,
        ..Prices::default()
    };
    let u = Usage {
        requests: 1,
        cache_write_1h: 1_000_000,
        ..Usage::default()
    };
    let s = u.snapshot("claude-sonnet-5-5", &p);
    assert!((s["cost_usd"].as_f64().unwrap() - 6.0).abs() < 1e-12, "{s}");
    assert_eq!(s["cache_write_tokens"], json!(1_000_000));
    let five = Usage {
        requests: 1,
        cache_write: 1_000_000,
        ..Usage::default()
    };
    let s = five.snapshot("claude-sonnet-5-5", &p);
    assert!(
        (s["cost_usd"].as_f64().unwrap() - 3.75).abs() < 1e-12,
        "{s}"
    );
}

#[tokio::test]
async fn a_one_hour_cache_write_is_counted_apart_from_the_five_minute_one() {
    let mut start = usage_start(50, 100, 0);
    start["message"]["usage"]["cache_creation"] =
        json!({"ephemeral_5m_input_tokens": 60, "ephemeral_1h_input_tokens": 40});
    let mut events = vec![start, text("ok")];
    events.extend(usage_stop(10));
    let (_s, ep) = api(200, sse(&events)).await;
    let mut p = provider(&ep, Arc::new(NoKey));
    p.usage = fresh_usage();
    drain(&p, None).await.unwrap();
    let u = p.usage.lock().unwrap();
    assert_eq!((u.cache_write, u.cache_write_1h), (60, 40));
    assert_eq!(
        u.snapshot("", &Prices::default())["cache_write_tokens"],
        json!(100)
    );
}

/// Adds counts to `model`'s share of the agent's since-boot usage, and to the total.
fn spend(id: &AgentId, model: &str, f: impl Fn(&mut Usage)) {
    usage_of(id).lock().unwrap().bump(model, None, f);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_without_cache_prices_uses_the_model_defaults() {
    let tmp = TempDir::new().unwrap();
    let base = json!({"model": "claude-sonnet-5-5", "prices": {"claude-sonnet-5-5": {"input": 3.0, "output": 15.0}}});
    let kernel = mk_with_id(
        &tmp,
        "http://unused.test/v1/messages",
        base.clone(),
        "f314a",
    )
    .await;
    spend(&AgentId::from("f314a"), "claude-sonnet-5-5", |u| {
        u.cache_read = 1_000_000
    });
    let got = call(&kernel, "f314a", json!({"type": "usage"})).await;
    assert!(
        (got["cost_usd"].as_f64().unwrap() - 0.15).abs() < 1e-12,
        "{got}"
    );
    // an explicit cache-read price on the record wins over the model default
    let tmp2 = TempDir::new().unwrap();
    let mut explicit = base;
    explicit["prices"]["claude-sonnet-5-5"]["cache_read"] = json!(0.30);
    let kernel = mk_with_id(&tmp2, "http://unused.test/v1/messages", explicit, "f314b").await;
    spend(&AgentId::from("f314b"), "claude-sonnet-5-5", |u| {
        u.cache_read = 1_000_000
    });
    let got = call(&kernel, "f314b", json!({"type": "usage"})).await;
    assert!(
        (got["cost_usd"].as_f64().unwrap() - 0.30).abs() < 1e-12,
        "{got}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_usage_verb_returns_the_counts_and_reset_zeroes_them() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_with(
        &tmp,
        "http://unused.test/v1/messages",
        json!({"prices": {MODEL: {"input": 3.0, "output": 15.0}}}),
    )
    .await;
    spend(&AgentId::from("a1"), MODEL, |u| {
        (u.requests, u.input, u.cache_write, u.cache_read) = (1, 100, 400, 1000);
    });
    let got = call(&kernel, "a1", json!({"type": "usage"})).await;
    assert_eq!(got["cache_read_tokens"], json!(1000));
    assert_eq!(got["saved_tokens"], json!(800));
    assert!(got.get("cost_usd").is_some() && got.get("saved_usd").is_some());
    assert_eq!(
        call(&kernel, "a1", json!({"type": "usage", "reset": true})).await,
        got
    );
    let zero = call(&kernel, "a1", json!({"type": "usage"})).await;
    assert_eq!(zero["requests"], json!(0));
    assert_eq!(zero["saved_tokens"], json!(0));
    let bad = call(&kernel, "a1", json!({"type": "usage", "reset": "yes"})).await;
    assert_eq!(bad["reason"], json!("bad_arg"));
    assert_eq!(bad["arg"], json!("reset"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_carries_the_usage_and_the_caching_mode() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_with(
        &tmp,
        "http://unused.test/v1/messages",
        json!({"caching": "provider_cache"}),
    )
    .await;
    let r = call(&kernel, "a1", json!({"type": "reflect"})).await;
    assert_eq!(r["caching"], json!("provider_cache"));
    assert!(r.get("prompt_cache").is_none(), "{r}");
    assert!(r["usage"]["prompt_tokens"].is_number(), "{r}");
    assert!(r["usage"]["saved_tokens"].is_number(), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_shows_cache_primer_until_only_when_set() {
    let tmp = TempDir::new().unwrap();
    let endpoint = "http://unused.test/v1/messages";
    let set = mk_with(
        &tmp,
        endpoint,
        json!({"caching": "provider_cache", "cache_primer_until": 3}),
    )
    .await;
    let r = call(&set, "a1", json!({"type": "reflect"})).await;
    assert_eq!(r["cache_primer_until"], json!(3), "{r}");
    let tmp = TempDir::new().unwrap();
    let unset = mk_with(&tmp, endpoint, json!({"caching": "provider_cache"})).await;
    let r = call(&unset, "a1", json!({"type": "reflect"})).await;
    assert!(r.get("cache_primer_until").is_none(), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_an_agent_drops_its_counts() {
    let tmp = TempDir::new().unwrap();
    let endpoint = "http://unused.test/v1/messages";
    let kernel = mk_with_id(&tmp, endpoint, json!({}), "gone1").await;
    {
        let counts = usage_of(&AgentId::from("gone1"));
        let mut u = counts.lock().unwrap();
        (u.requests, u.input, u.cache_write, u.cache_read) = (1, 100, 400, 1000);
    }
    let r = call(
        &kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": "gone1"}),
    )
    .await;
    assert_eq!(r["deleted"], json!(true), "{r}");
    assert!(!usage_table().lock().unwrap().contains_key("gone1"));
    let kernel = mk_with_id(&tmp, endpoint, json!({}), "gone1").await;
    let zero = call(&kernel, "gone1", json!({"type": "usage"})).await;
    assert_eq!(zero["requests"], json!(0), "{zero}");
    assert_eq!(zero["cache_read_tokens"], json!(0), "{zero}");
}

#[test]
fn the_record_refuses_a_bad_caching_mode_or_primer_or_price() {
    let check = |meta: Value| validate(meta.as_object().unwrap(), &rec(json!({})));
    assert_eq!(check(json!({"caching": "provider_cache"})), None);
    assert_eq!(check(json!({"caching": "none"})), None);
    assert_eq!(check(json!({"cache_primer_until": 12})), None);
    assert_eq!(
        check(json!({"prices": {"m": {"input": 3, "output": 15}}})),
        None
    );
    assert_eq!(
        check(json!({"caching": "always"})).as_deref(),
        Some("caching='always' is not one of the legal values: none, cleanup, provider_cache")
    );
    assert_eq!(
        check(json!({"caching": true})).as_deref(),
        Some("caching=True is not one of the legal values: none, cleanup, provider_cache")
    );
    assert_eq!(
        check(json!({"cache_primer_until": 0})).as_deref(),
        Some("cache_primer_until=0 must be a row id (a positive int)")
    );
    assert_eq!(
        check(json!({"prompt_cache": true})),
        None,
        "retired: an unknown key is not this backend's to refuse"
    );
    assert!(check(json!({"prices": {"m": {"input": 3, "output": "x"}}}))
        .unwrap()
        .contains("must be a number"));
    assert!(
        check(json!({"prices": {"m": {"input": true, "output": 1}}}))
            .unwrap()
            .contains("must be a number")
    );
    assert!(check(json!({"prices": {"m": {"input": -1, "output": 1}}}))
        .unwrap()
        .contains("zero or more"));
    assert!(check(json!({"prices": {"m": {"input": 1}}}))
        .unwrap()
        .contains("prices.m.output"));
    assert!(
        check(json!({"prices": {"m": {"input": 1, "output": 1, "cache_read": -1}}}))
            .unwrap()
            .contains("prices.m.cache_read")
    );
    assert!(check(json!({"prices": [1]}))
        .unwrap()
        .contains("prices must be a map"));
    let names: Vec<&str> = record_schema()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(names.contains(&"caching") && names.contains(&"cache_primer_until"));
    assert!(names.contains(&"effort") && names.contains(&"thinking_display"));
    assert!(!names.contains(&"prompt_cache"));
    assert!(names.contains(&"prices"));
    for gone in [
        "price_input_per_mtok",
        "price_output_per_mtok",
        "price_cache_read_per_mtok",
        "price_cache_write_5m_per_mtok",
    ] {
        assert!(!names.contains(&gone), "{gone} is replaced by prices");
    }
}

/// A kernel with the agent [`STALL_AGENT`] on a memory store, its record naming `endpoint` and
/// the given extra fields — the whole turn runs through ai-core, the read clock
/// included (#177).
async fn mk_with_store(tmp: &TempDir, endpoint: &str, extra: Value) -> Arc<Kernel> {
    let mut kernel = Kernel::new();

    kernel.bundles.register(
        fantastic_ai_core::chat_bundle::HANDLER_MODULE,
        fantastic_ai_core::chat_bundle::AiChatBundle,
    );
    kernel.bundles.register(HANDLER_MODULE, AnthropicBundle);
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
    let memory =
        fantastic_ram_store::test_support::memory_meta(&kernel, STALL_AGENT, json!({"cli": "*"}))
            .await;
    let mut payload = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": STALL_AGENT,
        "endpoint": endpoint,
        "model": MODEL,
        "max_tokens": 1000,
        "context_window": 200_000,
    });
    for (k, v) in memory
        .into_iter()
        .chain(extra.as_object().cloned().unwrap_or_default())
    {
        payload[k] = v;
    }
    let r = call(&kernel, "kernel_state", payload).await;
    assert_eq!(r["id"], json!(STALL_AGENT), "{r}");
    kernel
}

/// The process-global turn state is keyed by agent id, so this turn has an id of
/// its own: no other test's `a1` can hold its lock or its in-flight entry.
const STALL_AGENT: &str = "stall_turn";

/// THE WHOLE PATH (#177): a stream that answers its first events and then goes
/// silent ends its turn after the record's `stream_idle_seconds`. The turn stores
/// an error row naming the stall, `status` reports the turn done with
/// `reason='error'` and nothing generating.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_api_stream_ends_the_turn_with_an_error_row() {
    let (ep, server) = stalling_endpoint(Some(sse(&[start(), text("half")]))).await;
    let tmp = TempDir::new().unwrap();
    // `misfire_limit: 2`: the failed turn is retried once (#533), then the chain gives up
    let kernel = mk_with_store(
        &tmp,
        &ep,
        json!({"stream_idle_seconds": 0.2, "misfire_limit": 2}),
    )
    .await;
    let started = std::time::Instant::now();
    let r = call(
        &kernel,
        STALL_AGENT,
        json!({"type": "send", "text": "hi", "client_id": "t"}),
    )
    .await;
    server.abort();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(r["raised"], json!(true), "{r}");
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("stream_stalled"), "{err}");
    assert!(err.contains("sent no bytes for 0.2s"), "{err}");
    assert!(err.contains(&ep), "{err}");
    let rows = call(&kernel, STALL_AGENT, json!({"type": "rows"})).await;
    let errors: Vec<&Value> = rows["rows"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter(|r| r.get("kind").and_then(Value::as_str) == Some("error"))
                .collect()
        })
        .unwrap_or_default();
    // the retry (#533) may have stored a second error row by now
    assert!(!errors.is_empty(), "{rows}");
    let content = errors[0]["content"].as_str().unwrap_or_default();
    assert!(content.contains("stream_stalled"), "{content}");
    // the chain gives up after the retry fails too, and nothing is generating
    let mut status = json!({});
    for _ in 0..500 {
        status = call(&kernel, STALL_AGENT, json!({"type": "status"})).await;
        if status["generating"] == json!(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status["generating"], json!(false), "{status}");
}

// ─── the models verb ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_models_verb_lists_the_keys_models() {
    let tmp = TempDir::new().unwrap();
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({"data": [
                    {"id": "claude-haiku-5-5", "display_name": "Haiku",
                     "max_input_tokens": 200000, "max_tokens": 64000},
                    {"id": "x"}
                ]})
                .to_string(),
            ),
        )
        .mount(&srv)
        .await;
    let ep = format!("{}/v1/messages", srv.uri());
    let kernel = mk(&tmp, &ep).await;
    let r = call(&kernel, "a1", json!({"type": "models"})).await;
    // a record that lists no models: the provider asks its API, and answers the names
    assert_eq!(r["models"], json!(["claude-haiku-5-5", "x"]), "{r}");
    let reqs = srv.received_requests().await.unwrap_or_default();
    assert!(
        reqs[0]
            .url
            .query()
            .unwrap_or_default()
            .contains("limit=1000"),
        "{}",
        reqs[0].url
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_models_verb_names_a_refusing_api() {
    let tmp = TempDir::new().unwrap();
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&srv)
        .await;
    let ep = format!("{}/v1/messages", srv.uri());
    let kernel = mk(&tmp, &ep).await;
    let r = call(&kernel, "a1", json!({"type": "models"})).await;
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("401"), "{r}");
    assert!(err.contains("credentials"), "{err}");
}

#[test]
fn effort_and_thinking_display_shape_the_body() {
    let msgs = [json!({"role": "user", "content": "hi"})];
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.effort = Some("high".into());
    let b = p.body(&msgs, None);
    assert_eq!(b["output_config"], json!({"effort": "high"}));
    assert!(b.get("thinking").is_none(), "{b}");

    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.thinking_display = Some("summarized".into());
    p.thinking_budget = Some(2048);
    let b = p.body(&msgs, None);
    assert_eq!(
        b["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert!(b.get("output_config").is_none(), "{b}");

    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let b = p.body(&msgs, None);
    assert!(
        b.get("output_config").is_none() && b.get("thinking").is_none(),
        "{b}"
    );
}

#[test]
fn a_bad_effort_or_display_or_both_modes_are_refused() {
    for (meta, fragment) in [
        (
            json!({"effort": "ultra"}),
            "effort must be one of low, medium, high, xhigh, max",
        ),
        (
            json!({"thinking_display": "verbose"}),
            "thinking_display must be summarized or omitted",
        ),
        (
            json!({"thinking_display": "summarized", "thinking_budget": 2048}),
            "two thinking modes — set one",
        ),
    ] {
        let err = validate(meta.as_object().unwrap(), &rec(json!({}))).unwrap_or_default();
        assert!(err.contains(fragment), "{meta}: {err}");
    }
    assert_eq!(
        validate(
            json!({"effort": "xhigh", "thinking_display": "omitted"})
                .as_object()
                .unwrap(),
            &rec(json!({}))
        ),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_models_verb_refuses_a_body_that_is_not_a_model_list() {
    let tmp = TempDir::new().unwrap();
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>proxy</html>"))
        .mount(&srv)
        .await;
    let ep = format!("{}/v1/messages", srv.uri());
    let kernel = mk(&tmp, &ep).await;
    let r = call(&kernel, "a1", json!({"type": "models"})).await;
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.contains("not a model list"), "{r}");
}

#[test]
fn budget_thinking_refused_reads_the_model_family() {
    for m in [
        "claude-sonnet-5-5",
        "claude-opus-4-7",
        "claude-fable-5-1",
        "claude-haiku-5-5-20260101",
    ] {
        assert!(budget_thinking_refused(m), "{m}");
    }
    for m in ["claude-opus-4-6", "claude-3-5-sonnet-20241022", "my-model"] {
        assert!(!budget_thinking_refused(m), "{m}");
    }
}

#[test]
fn thinking_budget_is_refused_for_a_5x_model() {
    let err = validate(
        json!({"model": "claude-sonnet-5-5", "thinking_budget": 2048})
            .as_object()
            .unwrap(),
        &rec(json!({})),
    )
    .unwrap_or_default();
    assert!(
        err.contains("thinking_budget is refused by claude-sonnet-5-5")
            && err.contains("thinking_display (summarized|omitted) and effort instead"),
        "{err}"
    );
    assert_eq!(
        validate(
            json!({"model": "claude-opus-4-6", "thinking_budget": 2048})
                .as_object()
                .unwrap(),
            &rec(json!({}))
        ),
        None
    );
}

// ─── the counts across a restart (#308) ─────────────────────────────────────

/// A disk-backed kernel on `tmp`, booted the way a daemon boots (lockless): the
/// root loader, the record writer and a store wired for records. The first boot
/// seeds the root; a later one reads what the first left on disk.
async fn boot_disk(tmp: &TempDir) -> Arc<Kernel> {
    let store = tmp.path().join(".fantastic");
    let mut reg = fantastic_kernel::BundleRegistry::new();
    fantastic_kernel::test_support::register_root_loader(&mut reg);
    fantastic_kernel::test_support::register_fake_store(&mut reg, &store);
    reg.register(HANDLER_MODULE, AnthropicBundle);
    let mut booted = fantastic_kernel::bootstrap::bootstrap(
        reg,
        fantastic_kernel::bootstrap::BootstrapOptions::one_shot(
            tmp.path().join(".fantastic"),
            tmp.path(),
        ),
    )
    .expect("boot");
    booted.write_seed().expect("seed");
    let kernel = booted.kernel;
    fantastic_kernel::test_support::boot_root(&kernel).await;
    if !kernel.agent_ids().iter().any(|id| id.as_str() == "store") {
        fantastic_kernel::test_support::wire_fake_store(&kernel, &store).await;
    }
    kernel
}

/// Creates agent `id` on a disk kernel, pointed at `endpoint`, and writes it down.
async fn create_disk_agent(kernel: &Arc<Kernel>, id: &str, endpoint: &str, extra: Value) {
    let mut payload = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE, "id": id});
    let mut record = rec(json!({"max_tokens": 8000}));
    record.extend(extra.as_object().cloned().unwrap_or_default());
    record.insert("endpoint".into(), json!(endpoint));
    for (k, v) in record {
        payload[k] = v;
    }
    let r = call(kernel, "kernel_state", payload).await;
    assert_eq!(r["id"], json!(id), "{r}");
    kernel.flush_records().await;
}

/// A restart, in-process: the counts this process held for `id` are gone, and a
/// fresh boot of the same dir reads what the disk kept.
async fn restart(tmp: &TempDir, id: &str) -> Arc<Kernel> {
    forget_counts(id);
    boot_disk(tmp).await
}

/// One request's stream: 100 uncached input, 400 written, 1000 read, `out` output.
fn turn_events(out: i64) -> String {
    let mut ev = vec![usage_start(100, 400, 1000), text("ok")];
    ev.extend(usage_stop(out));
    sse(&ev)
}

/// One turn through the agent's own provider, as a send runs it, stream drained.
async fn turn(kernel: &Arc<Kernel>, id: &str) {
    let p = build_provider(&AgentId::from(id), kernel);
    let msgs = [json!({"role": "user", "content": "hi"})];
    let mut s = p
        .chat(&msgs, None)
        .await
        .unwrap_or_else(|e| panic!("{}", e.message));
    while let Some(ev) = s.next().await {
        ev.unwrap_or_else(|e| panic!("{}", e.message));
    }
}

fn priced() -> Value {
    json!({"prices": {"claude-test-5": {"input": 3.0, "output": 15.0}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_survives_a_restart() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events(35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "u_restart", &ep, json!({})).await;
    turn(&kernel, "u_restart").await;
    let agent_dir = tmp.path().join(".fantastic/agents/u_restart");
    assert!(
        agent_dir.join("usage.json").is_file(),
        "no sidecar in {agent_dir:?}"
    );
    let kernel = restart(&tmp, "u_restart").await;
    let got = call(&kernel, "u_restart", json!({"type": "usage"})).await;
    assert_eq!(got["requests"], json!(1), "{got}");
    assert_eq!(got["output_tokens"], json!(35), "{got}");
    assert_eq!(got["cache_read_tokens"], json!(1000), "{got}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_survives_a_restart_through_the_params_store() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events(35)).await;
    let kernel = mk_unbounded_store(&tmp, &ep, "u_store", json!({"max_tokens": 8000})).await;
    turn(&kernel, "u_store").await;
    assert!(
        !usage_file_of(&AgentId::from("u_store"), &kernel).is_some_and(|p| p.is_file()),
        "this agent has no usage file"
    );
    // a restart: the counts this process held are gone, the store kept its row
    forget_counts("u_store");
    let got = call(&kernel, "u_store", json!({"type": "usage"})).await;
    assert_eq!(got["requests"], json!(1), "{got}");
    assert_eq!(got["output_tokens"], json!(35), "{got}");
    assert_eq!(got["cache_read_tokens"], json!(1000), "{got}");
    // a new turn adds to what the store held
    turn(&kernel, "u_store").await;
    forget_counts("u_store");
    let got = call(&kernel, "u_store", json!({"type": "usage"})).await;
    assert_eq!(got["requests"], json!(2), "{got}");
    // reset zeroes it in the store too
    let pre = call(&kernel, "u_store", json!({"type": "usage_reset"})).await;
    assert_eq!(pre["requests"], json!(2), "{pre}");
    forget_counts("u_store");
    let after = call(&kernel, "u_store", json!({"type": "usage"})).await;
    assert_eq!(after["requests"], json!(0), "{after}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_since_boot_starts_at_zero() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events(35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "u_since", &ep, json!({})).await;
    turn(&kernel, "u_since").await;
    let kernel = restart(&tmp, "u_since").await;
    let r = call(&kernel, "u_since", json!({"type": "reflect"})).await;
    assert_eq!(r["usage"]["requests"], json!(1), "{r}");
    assert_eq!(r["usage_since_boot"]["requests"], json!(0), "{r}");
    turn(&kernel, "u_since").await;
    let r = call(&kernel, "u_since", json!({"type": "reflect"})).await;
    assert_eq!(r["usage"]["requests"], json!(2), "{r}");
    assert_eq!(r["usage_since_boot"]["requests"], json!(1), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_reset_clears_it() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events(35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "u_reset", &ep, priced()).await;
    turn(&kernel, "u_reset").await;
    let pre = call(&kernel, "u_reset", json!({"type": "usage_reset"})).await;
    assert_eq!(pre["requests"], json!(1), "{pre}");
    let now = call(&kernel, "u_reset", json!({"type": "usage"})).await;
    assert_eq!(now["requests"], json!(0), "{now}");
    assert_eq!(now["cost_usd"].as_f64(), Some(0.0), "{now}");
    let kernel = restart(&tmp, "u_reset").await;
    let after = call(&kernel, "u_reset", json!({"type": "usage"})).await;
    assert_eq!(after["requests"], json!(0), "{after}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cost_usd_accumulates_across_a_restart() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events(35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "u_cost", &ep, priced()).await;
    turn(&kernel, "u_cost").await;
    let one = call(&kernel, "u_cost", json!({"type": "usage"})).await["cost_usd"]
        .as_f64()
        .expect("cost_usd");
    let kernel = restart(&tmp, "u_cost").await;
    turn(&kernel, "u_cost").await;
    let two = call(&kernel, "u_cost", json!({"type": "usage"})).await["cost_usd"]
        .as_f64()
        .expect("cost_usd");
    // One request: 700 input-side units at $3/Mtok plus 35 output at $15/Mtok.
    assert!((one - 0.002625).abs() < 1e-9, "{one}");
    assert!((two - 2.0 * one).abs() < 1e-12, "{two} vs {one}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_usage_file_starts_clean() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events(35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "u_corrupt", &ep, json!({})).await;
    let file = tmp.path().join(".fantastic/agents/u_corrupt/usage.json");
    std::fs::write(&file, "{ not json \u{0}").unwrap();
    let kernel = restart(&tmp, "u_corrupt").await;
    let got = call(&kernel, "u_corrupt", json!({"type": "usage"})).await;
    assert!(got.get("reason").is_none(), "{got}");
    assert_eq!(got["requests"], json!(0), "{got}");
    turn(&kernel, "u_corrupt").await;
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap())
        .expect("the turn rewrites a valid file");
    assert_eq!(written["requests"], json!(1), "{written}");
}

#[test]
fn the_sidecar_keeps_the_one_hour_write_count() {
    let u = Usage {
        cache_write_1h: 40,
        long_cache_write_1h: 7,
        ..Usage::default()
    };
    let back = Usage::from_json(&u.to_json());
    assert_eq!((back.cache_write_1h, back.long_cache_write_1h), (40, 7));
    // A sidecar written before the field existed loads with 0.
    let old = Usage::from_json(&json!({"requests": 3, "cache_write": 5}));
    assert_eq!((old.requests, old.cache_write), (3, 5));
    assert_eq!((old.cache_write_1h, old.long_cache_write_1h), (0, 0));
}

fn turn_events_as(model: &str, out: i64) -> String {
    let mut ev = vec![usage_start_as(model, 100, 400, 1000), text("ok")];
    ev.extend(usage_stop(out));
    sse(&ev)
}

#[tokio::test]
async fn two_models_count_apart_and_sum_to_the_totals() {
    let mut first = vec![usage_start_as("claude-sonnet-5-5", 100, 400, 0), text("a")];
    first.extend(usage_stop(10));
    let mut second = vec![usage_start_as("claude-haiku-5-5", 50, 0, 900), text("b")];
    second.extend(usage_stop(20));
    let (_s1, ep1) = api(200, sse(&first)).await;
    let (_s2, ep2) = api(200, sse(&second)).await;
    let usage = fresh_usage();
    let mut p = provider(&ep1, Arc::new(NoKey));
    p.usage = Arc::clone(&usage);
    drain(&p, None).await.unwrap();
    p.endpoint = ep2;
    drain(&p, None).await.unwrap();
    let u = usage.lock().unwrap();
    let (s, h) = (
        &u.by_model["claude-sonnet-5-5"],
        &u.by_model["claude-haiku-5-5"],
    );
    assert_eq!(
        (s.requests, s.input, s.cache_write, s.cache_read, s.output),
        (1, 100, 400, 0, 10)
    );
    assert_eq!(
        (h.requests, h.input, h.cache_write, h.cache_read, h.output),
        (1, 50, 0, 900, 20)
    );
    assert_eq!(u.by_model.len(), 2);
    assert_eq!(u.requests, s.requests + h.requests);
    assert_eq!(u.input, s.input + h.input);
    assert_eq!(u.cache_write, s.cache_write + h.cache_write);
    assert_eq!(u.cache_read, s.cache_read + h.cache_read);
    assert_eq!(u.output, s.output + h.output);
}

#[tokio::test]
async fn cache_counts_land_on_the_model_that_spent_them() {
    let mut first = vec![usage_start_as("claude-haiku-5-5", 20, 0, 80), text("a")];
    first.extend(usage_stop(5));
    let mut second = vec![usage_start_as("claude-sonnet-5-5", 100, 0, 0), text("b")];
    second.extend(usage_stop(5));
    let (_s1, ep1) = api(200, sse(&first)).await;
    let (_s2, ep2) = api(200, sse(&second)).await;
    let usage = fresh_usage();
    let mut p = provider(&ep1, Arc::new(NoKey));
    p.usage = Arc::clone(&usage);
    drain(&p, None).await.unwrap();
    p.endpoint = ep2;
    drain(&p, None).await.unwrap();
    let s = usage.lock().unwrap().snapshot("", &Prices::default());
    assert_eq!(
        s["by_model"]["claude-haiku-5-5"]["cache_read"],
        json!(80),
        "{s}"
    );
    assert_eq!(
        s["by_model"]["claude-haiku-5-5"]["hit_rate"].as_f64(),
        Some(0.8),
        "{s}"
    );
    assert_eq!(
        s["by_model"]["claude-sonnet-5-5"]["cache_read"],
        json!(0),
        "{s}"
    );
    assert_eq!(
        s["by_model"]["claude-sonnet-5-5"]["cache_write"],
        json!(0),
        "{s}"
    );
    assert_eq!(
        s["by_model"]["claude-sonnet-5-5"]["hit_rate"].as_f64(),
        Some(0.0),
        "{s}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn by_model_survives_a_restart() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events_as("claude-haiku-5-5", 35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "m_restart", &ep, json!({})).await;
    turn(&kernel, "m_restart").await;
    let before = call(&kernel, "m_restart", json!({"type": "usage"})).await;
    let kernel = restart(&tmp, "m_restart").await;
    let after = call(&kernel, "m_restart", json!({"type": "usage"})).await;
    assert_eq!(after["by_model"], before["by_model"], "{after}");
    assert_eq!(
        after["by_model"]["claude-haiku-5-5"]["requests"],
        json!(1),
        "{after}"
    );
    // an old-format sidecar, without by_model, still loads
    let file = tmp.path().join(".fantastic/agents/m_restart/usage.json");
    let mut old: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("by_model");
    std::fs::write(&file, old.to_string()).unwrap();
    let kernel = restart(&tmp, "m_restart").await;
    let got = call(&kernel, "m_restart", json!({"type": "usage"})).await;
    assert_eq!(got["requests"], json!(1), "{got}");
    assert_eq!(got["by_model"], json!({}), "{got}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_reset_true_zeroes_the_boot_counts_per_model() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events_as("claude-haiku-5-5", 35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "m_reset", &ep, json!({})).await;
    turn(&kernel, "m_reset").await;
    // a restart: that turn becomes the base; the next one is this boot's own
    let kernel = restart(&tmp, "m_reset").await;
    turn(&kernel, "m_reset").await;
    let got = call(&kernel, "m_reset", json!({"type": "usage", "reset": true})).await;
    assert_eq!(
        got["by_model"]["claude-haiku-5-5"]["requests"],
        json!(2),
        "{got}"
    );
    assert_eq!(
        got["usage_since_boot"]["by_model"]["claude-haiku-5-5"]["requests"],
        json!(1),
        "{got}"
    );
    let after = call(&kernel, "m_reset", json!({"type": "usage"})).await;
    assert_eq!(after["usage_since_boot"]["by_model"], json!({}), "{after}");
    // the total keeps the base (the turn before the restart): only the boot counts went
    assert_eq!(after["requests"], json!(1), "{after}");
    assert_eq!(
        after["by_model"]["claude-haiku-5-5"]["requests"],
        json!(1),
        "{after}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_reset_verb_zeroes_by_model_and_the_file() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events_as("claude-haiku-5-5", 35)).await;
    let kernel = boot_disk(&tmp).await;
    create_disk_agent(&kernel, "m_zero", &ep, json!({})).await;
    turn(&kernel, "m_zero").await;
    let pre = call(&kernel, "m_zero", json!({"type": "usage_reset"})).await;
    assert_eq!(
        pre["by_model"]["claude-haiku-5-5"]["requests"],
        json!(1),
        "{pre}"
    );
    let after = call(&kernel, "m_zero", json!({"type": "usage"})).await;
    assert_eq!(after["by_model"], json!({}), "{after}");
    let kernel = restart(&tmp, "m_zero").await;
    let got = call(&kernel, "m_zero", json!({"type": "usage"})).await;
    assert_eq!(got["by_model"], json!({}), "{got}");
}

fn books(prices: Value) -> PriceBook {
    PriceBook::of(prices.as_object().unwrap())
}

#[tokio::test]
async fn two_models_price_apart_and_an_unpriced_one_is_never_zero() {
    let mut first = vec![usage_start_as("claude-sonnet-5-5", 100, 0, 0), text("a")];
    first.extend(usage_stop(10));
    let mut second = vec![usage_start_as("claude-haiku-5-5", 1000, 0, 0), text("b")];
    second.extend(usage_stop(20));
    let (_s1, ep1) = api(200, sse(&first)).await;
    let (_s2, ep2) = api(200, sse(&second)).await;
    let usage = fresh_usage();
    let mut p = provider(&ep1, Arc::new(NoKey));
    p.usage = Arc::clone(&usage);
    drain(&p, None).await.unwrap();
    p.endpoint = ep2;
    drain(&p, None).await.unwrap();
    let u = usage.lock().unwrap();
    // only sonnet has an entry
    let one = u.snapshot_priced(&books(
        json!({"prices": {"claude-sonnet-5-5": {"input": 3.0, "output": 15.0}}}),
    ));
    let sonnet = (100.0 * 3.0 + 10.0 * 15.0) / 1e6;
    assert!(
        (one["by_model"]["claude-sonnet-5-5"]["cost_usd"]
            .as_f64()
            .unwrap()
            - sonnet)
            .abs()
            < 1e-12,
        "{one}"
    );
    assert!(
        one["by_model"]["claude-haiku-5-5"]
            .get("cost_usd")
            .is_none(),
        "{one}"
    );
    assert_eq!(
        one["by_model"]["claude-haiku-5-5"]["unpriced"],
        json!(true),
        "{one}"
    );
    assert!(
        (one["cost_usd"].as_f64().unwrap() - sonnet).abs() < 1e-12,
        "{one}"
    );
    assert_eq!(one["unpriced_models"], json!(["claude-haiku-5-5"]), "{one}");
    // both priced: the total is the sum, nothing unpriced
    let both = u.snapshot_priced(&books(json!({"prices": {
        "claude-sonnet-5-5": {"input": 3.0, "output": 15.0},
        "claude-haiku-5-5": {"input": 1.0, "output": 5.0, "cache_read": 0.5}}})));
    let haiku = (1000.0 * 1.0 + 20.0 * 5.0) / 1e6;
    assert!(
        (both["by_model"]["claude-haiku-5-5"]["cost_usd"]
            .as_f64()
            .unwrap()
            - haiku)
            .abs()
            < 1e-12,
        "{both}"
    );
    assert!(
        (both["cost_usd"].as_f64().unwrap() - (sonnet + haiku)).abs() < 1e-12,
        "{both}"
    );
    assert!(both.get("unpriced_models").is_none(), "{both}");
    // no prices at all: no cost anywhere
    let none = u.snapshot_priced(&PriceBook::default());
    assert!(
        none.get("cost_usd").is_none()
            && none["by_model"]["claude-haiku-5-5"]
                .get("unpriced")
                .is_none(),
        "{none}"
    );
    // every model that spent is unpriced: no total, not $0
    let other = u.snapshot_priced(&books(
        json!({"prices": {"claude-opus-5-5": {"input": 5.0, "output": 25.0}}}),
    ));
    assert!(other.get("cost_usd").is_none(), "{other}");
    // a dated response id uses its base id's entry
    let dated = Usage::default();
    let mut d = dated;
    d.bump("claude-sonnet-5-5-20260101", None, |x| x.input = 1_000_000);
    let s = d.snapshot_priced(&books(
        json!({"prices": {"claude-sonnet-5-5": {"input": 3.0, "output": 15.0}}}),
    ));
    assert!((s["cost_usd"].as_f64().unwrap() - 3.0).abs() < 1e-12, "{s}");
}

#[tokio::test]
async fn two_calling_chats_count_apart_in_by_agent() {
    let mut first = vec![usage_start_as("claude-sonnet-5-5", 100, 400, 0), text("a")];
    first.extend(usage_stop(10));
    let mut second = vec![usage_start_as("claude-haiku-5-5", 50, 0, 900), text("b")];
    second.extend(usage_stop(20));
    let (_s1, ep1) = api(200, sse(&first)).await;
    let (_s2, ep2) = api(200, sse(&second)).await;
    let usage = fresh_usage();
    let mut p = provider(&ep1, Arc::new(NoKey));
    p.usage = Arc::clone(&usage);
    p.caller = Some("chat_a".into());
    drain(&p, None).await.unwrap();
    drain(&p, None).await.unwrap();
    p.endpoint = ep2;
    p.caller = Some("chat_b".into());
    drain(&p, None).await.unwrap();
    let u = usage.lock().unwrap();
    let book = books(json!({"prices": {
        "claude-sonnet-5-5": {"input": 3.0, "output": 15.0},
        "claude-haiku-5-5": {"input": 1.0, "output": 5.0}}}));
    let s = u.snapshot_priced(&book);
    let (a, b) = (&s["by_agent"]["chat_a"], &s["by_agent"]["chat_b"]);
    assert_eq!(
        (
            &a["requests"],
            &a["input"],
            &a["cache_write"],
            &a["cache_read"],
            &a["output"]
        ),
        (&json!(2), &json!(200), &json!(800), &json!(0), &json!(20)),
        "{s}"
    );
    assert_eq!(
        (
            &b["requests"],
            &b["input"],
            &b["cache_write"],
            &b["cache_read"],
            &b["output"]
        ),
        (&json!(1), &json!(50), &json!(0), &json!(900), &json!(20)),
        "{s}"
    );
    let sonnet = (200.0 * 3.0 + 800.0 * 3.75 + 20.0 * 15.0) / 1e6;
    assert!(
        (a["cost_usd"].as_f64().unwrap() - sonnet).abs() < 1e-12,
        "{s}"
    );
    // the agents' counts and costs sum to the totals
    assert_eq!(s["requests"].as_i64().unwrap(), 3);
    let sum_cost = a["cost_usd"].as_f64().unwrap() + b["cost_usd"].as_f64().unwrap();
    assert!(
        (s["cost_usd"].as_f64().unwrap() - sum_cost).abs() < 1e-12,
        "{s}"
    );
    // an unpriced model leaves the agent's cost out: absent, not $0
    let bare = u.snapshot_priced(&books(
        json!({"prices": {"claude-sonnet-5-5": {"input": 3.0, "output": 15.0}}}),
    ));
    assert!(
        bare["by_agent"]["chat_b"].get("cost_usd").is_none(),
        "{bare}"
    );
    // a call with no caller counts in the totals alone
    drop(u);
    p.caller = None;
    let (_s3, ep3) = api(200, sse(&first)).await;
    p.endpoint = ep3;
    drain(&p, None).await.unwrap();
    let s = usage.lock().unwrap().snapshot_priced(&book);
    assert_eq!(s["requests"], json!(4), "{s}");
    assert_eq!(s["by_agent"].as_object().unwrap().len(), 2, "{s}");
}

#[test]
fn the_caller_is_the_rid_prefix() {
    assert_eq!(
        caller_of(&json!({"rid": "chat_ai~7"})).as_deref(),
        Some("chat_ai")
    );
    assert_eq!(caller_of(&json!({"rid": "a~b~7"})).as_deref(), Some("a~b"));
    assert_eq!(caller_of(&json!({"rid": "7"})), None);
    assert_eq!(caller_of(&json!({"rid": "~7"})), None);
    assert_eq!(caller_of(&json!({})), None);
}

async fn complete_as(kernel: &Arc<Kernel>, provider: &str, chat: &str) {
    let r = call(
        kernel,
        provider,
        json!({"type": "complete", "rid": format!("{chat}~1"), "model": MODEL,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn by_agent_keeps_a_deleted_agent_survives_a_restart_and_resets() {
    let tmp = TempDir::new().unwrap();
    let (_srv, ep) = api(200, turn_events(35)).await;
    let kernel = boot_disk(&tmp).await;
    let extra = json!({"context_window": 200000, "prices": {"claude-test-5": {"input": 3.0, "output": 15.0}}});
    create_disk_agent(&kernel, "g_prov", &ep, extra).await;
    create_disk_agent(&kernel, "g_chat", &ep, json!({})).await;
    complete_as(&kernel, "g_prov", "g_chat").await;
    complete_as(&kernel, "g_prov", "g_other").await;
    complete_as(&kernel, "g_prov", "g_other").await;
    let got = call(&kernel, "g_prov", json!({"type": "usage"})).await;
    assert_eq!(got["by_agent"]["g_chat"]["requests"], json!(1), "{got}");
    assert_eq!(got["by_agent"]["g_chat"]["output"], json!(35), "{got}");
    assert_eq!(got["by_agent"]["g_other"]["requests"], json!(2), "{got}");
    assert!(
        (got["by_agent"]["g_chat"]["cost_usd"].as_f64().unwrap() - 0.002625).abs() < 1e-9,
        "{got}"
    );
    assert_eq!(
        got["usage_since_boot"]["by_agent"]["g_other"]["requests"],
        json!(2),
        "{got}"
    );
    // the caller is deleted: its spend stays
    let r = call(
        &kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": "g_chat"}),
    )
    .await;
    assert_eq!(r["deleted"], json!(true), "{r}");
    let after = call(&kernel, "g_prov", json!({"type": "usage"})).await;
    assert_eq!(after["by_agent"], got["by_agent"], "{after}");
    // a restart keeps it, from the usage file
    kernel.flush_records().await;
    let file = tmp.path().join(".fantastic/agents/g_prov/usage.json");
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(
        written["by_agent"]["g_chat"]["requests"],
        json!(1),
        "{written}"
    );
    let kernel = restart(&tmp, "g_prov").await;
    let back = call(&kernel, "g_prov", json!({"type": "usage"})).await;
    assert_eq!(back["by_agent"], got["by_agent"], "{back}");
    assert_eq!(back["usage_since_boot"]["by_agent"], json!({}), "{back}");
    // an old sidecar without by_agent still loads
    let mut old = written.clone();
    old.as_object_mut().unwrap().remove("by_agent");
    std::fs::write(&file, old.to_string()).unwrap();
    let kernel = restart(&tmp, "g_prov").await;
    let old_got = call(&kernel, "g_prov", json!({"type": "usage"})).await;
    assert_eq!(old_got["by_agent"], json!({}), "{old_got}");
    assert_eq!(old_got["requests"], json!(3), "{old_got}");
    // usage_reset zeroes it, and the file
    complete_as(&kernel, "g_prov", "g_other").await;
    let pre = call(&kernel, "g_prov", json!({"type": "usage_reset"})).await;
    assert_eq!(pre["by_agent"]["g_other"]["requests"], json!(1), "{pre}");
    let zero = call(&kernel, "g_prov", json!({"type": "usage"})).await;
    assert_eq!(zero["by_agent"], json!({}), "{zero}");
    let kernel = restart(&tmp, "g_prov").await;
    let zero = call(&kernel, "g_prov", json!({"type": "usage"})).await;
    assert_eq!(zero["by_agent"], json!({}), "{zero}");
    // usage reset:true zeroes the since-boot by_agent, as it does the counts
    complete_as(&kernel, "g_prov", "g_other").await;
    let got = call(&kernel, "g_prov", json!({"type": "usage", "reset": true})).await;
    assert_eq!(
        got["usage_since_boot"]["by_agent"]["g_other"]["requests"],
        json!(1),
        "{got}"
    );
    let after = call(&kernel, "g_prov", json!({"type": "usage"})).await;
    assert_eq!(after["usage_since_boot"]["by_agent"], json!({}), "{after}");
}

// ─── the imageless golden (#622) ────────────────────────────────────────────

/// A fixed imageless conversation, marks and merged turns included.
fn golden_messages() -> Vec<Value> {
    let mut m = flagged_sys(&[
        ("user", "p1", None),
        ("assistant", "p2", Some("cache_mark")),
        ("user", "f1", None),
        ("user", "f2", Some("cache_mark")),
        ("user", "t1 \u{e9}\u{4e2d}", None),
        ("assistant", "t2", None),
        ("user", "live", None),
    ]);
    m.push(json!({"role": "system", "content": "extra system"}));
    m
}

/// The bytes the request body of [`golden_messages`] had on main before images (#622).
const GOLDEN_BODY: &str = r#"{"model":"claude-test-5","max_tokens":8000,"messages":[{"role":"user","content":"p1"},{"role":"assistant","content":[{"type":"text","text":"p2","cache_control":{"type":"ephemeral"}}]},{"role":"user","content":[{"type":"text","text":"f1\n\nf2","cache_control":{"type":"ephemeral"}},{"type":"text","text":"\n\nt1 é中"}]},{"role":"assistant","content":"t2"},{"role":"user","content":"live"}],"stream":true,"system":[{"type":"text","text":"S\n\nextra system","cache_control":{"type":"ephemeral"}}]}"#;

#[test]
fn an_imageless_request_body_is_byte_identical_to_before_images() {
    let p = provider("http://x/v1/messages", Arc::new(NoKey));
    let body = p.body(&golden_messages(), None).to_string();
    assert_eq!(body, GOLDEN_BODY);
}

fn one_image(r: i64, media: &str, bytes: &[u8]) -> Images {
    let mut i = Images::default();
    i.0.insert(r.to_string(), (media.to_string(), bytes.to_vec()));
    i
}

#[test]
fn an_image_part_becomes_a_base64_block_before_the_text_of_its_message() {
    let messages = [
        json!({"role": "system", "content": "s"}),
        json!({"role": "user", "content": "before"}),
        json!({"role": "user", "content": "look", "images": [
            {"type": "image", "ref": 7, "media_type": "image/png", "width": 2, "height": 1}]}),
        json!({"role": "assistant", "content": "ok"}),
    ];
    let (system, _, turns) = to_wire(&messages, true, &one_image(7, "image/png", b"\x89PNG"));
    assert_eq!(system, "s");
    assert_eq!(
        turns,
        vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "before"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                    "data": "iVBORw=="}},
                {"type": "text", "text": "look"},
            ]}),
            json!({"role": "assistant", "content": "ok"}),
        ]
    );
}

#[test]
fn an_image_row_with_no_text_sends_no_blank_text_block_and_keeps_its_mark() {
    let messages = [
        json!({"role": "user", "content": "", "images": [{"ref": "a"}]}),
        json!({"role": "user", "content": "x", "cache_mark": true}),
    ];
    let mut imgs = Images::default();
    imgs.0
        .insert("a".into(), ("image/jpeg".into(), vec![1, 2, 3]));
    let (_, _, turns) = to_wire(&messages, true, &imgs);
    let blocks = turns[0]["content"].as_array().expect("blocks");
    assert_eq!(blocks[0]["type"], json!("image"));
    assert_eq!(blocks[1]["text"], json!("\n\nx"));
    assert_eq!(blocks[1]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(blocks.len(), 2, "{blocks:?}");
}

#[test]
fn a_message_without_images_renders_as_before_with_or_without_the_images_in_hand() {
    let messages = [
        json!({"role": "user", "content": "u1"}),
        json!({"role": "user", "content": "u2", "cache_mark": true}),
        json!({"role": "assistant", "content": "x", "images": []}),
    ];
    let plain = to_wire(&messages, true, &Images::default());
    let held = to_wire(&messages, true, &one_image(1, "image/png", b"zz"));
    assert_eq!(plain, held);
    assert_eq!(plain.2[0]["content"][0]["text"], json!("u1\n\nu2"));
    assert_eq!(plain.2[1]["content"], json!("x"));
}

#[test]
fn reflect_names_the_capabilities_of_its_models_and_an_unknown_model_sees_no_images() {
    let sees = static_capability("claude-sonnet-4-5");
    assert_eq!(sees["images"], json!(true));
    for k in ["max_images", "max_bytes", "max_pixels"] {
        assert!(sees[k].as_i64().is_some_and(|n| n > 0), "{k}: {sees}");
    }
    for m in [
        "gpt-x",
        "claude-2.1",
        "claude-instant-1.2",
        "claude-unknown-9",
        "",
    ] {
        assert_eq!(static_capability(m), json!({"images": false}), "{m}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_carries_capabilities_per_model() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_with(
        &tmp,
        "http://unused.test/v1/messages",
        json!({"models": ["claude-opus-4-1", "mystery-1"]}),
    )
    .await;
    let r = call(&kernel, "a1", json!({"type": "reflect"})).await;
    let caps = &r["capabilities"];
    assert_eq!(caps["claude-opus-4-1"]["images"], json!(true), "{r}");
    assert_eq!(caps["mystery-1"], json!({"images": false}), "{r}");
    assert_eq!(
        caps[MODEL]["images"],
        static_capability(MODEL)["images"],
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capabilities_setting_in_the_params_store_overrides_the_static_entry() {
    let tmp = TempDir::new().unwrap();
    let id = "capset1";
    let kernel = mk_unbounded_store(&tmp, "http://unused.test/v1/messages", id, json!({})).await;
    let store = helpers::agent_meta(&AgentId::from(id), &kernel)["memory_store_id"]
        .as_str()
        .expect("store")
        .to_string();
    let kept =
        json!({"claude-opus-4-1": {"images": false}, "tiny": {"images": true, "max_images": 1}});
    let r = fantastic_kernel::send::as_agent(&AgentId::from(id), async {
        kernel
            .send(vec![Item::new(
                AgentId::from(store.as_str()),
                json!({"type": "append", "rows": [{"kind": "setting", "name": "capabilities", "value": kept}]}),
            )])
            .await
            .remove(0)
            .result
    })
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let r = call(&kernel, id, json!({"type": "reflect"})).await;
    assert_eq!(r["capabilities"]["tiny"]["max_images"], json!(1), "{r}");
    assert!(r["capabilities"]["tiny"].get("max_bytes").is_none(), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_bytes_on_the_binary_path_reach_the_wire_as_base64() {
    let tmp = TempDir::new().unwrap();
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/models/{MODEL}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({"id": MODEL, "max_input_tokens": 200_000, "max_tokens": 1000}).to_string(),
        ))
        .mount(&srv)
        .await;
    let mut events = vec![start(), text("seen")];
    events.extend(stop("end_turn"));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse(&events)))
        .mount(&srv)
        .await;
    let kernel = mk_with_id(
        &tmp,
        &format!("{}/v1/messages", srv.uri()),
        json!({}),
        "img1",
    )
    .await;
    let rid = "chat~1";
    let item = Item::new(
        AgentId::from("img1"),
        json!({"type": "complete", "rid": rid, "image": {"ref": 5, "media_type": "image/png"}}),
    )
    .with_binary_key("data", vec![0xff, 0x00, 0x10]);
    let r = kernel.send(vec![item]).await.remove(0).result;
    assert_eq!(r["bytes"], json!(3), "{r}");
    assert!(srv.received_requests().await.unwrap_or_default().is_empty());
    let r = call(
        &kernel,
        "img1",
        json!({"type": "complete", "rid": rid, "model": MODEL, "messages": [
            {"role": "user", "content": "what", "images": [{"type": "image", "ref": 5}]}]}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let reqs = srv.received_requests().await.unwrap_or_default();
    let post = reqs
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .expect("a POST");
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "/wAQ"}},
            {"type": "text", "text": "what"},
        ])
    );
    // the bytes were taken: a second complete naming the ref has none
    let r = call(
        &kernel,
        "img1",
        json!({"type": "complete", "rid": rid, "model": MODEL, "messages": [
            {"role": "user", "content": "again", "images": [{"ref": 5}]}]}),
    )
    .await;
    assert!(
        r["error"]
            .as_str()
            .is_some_and(|e| e.contains("image ref 5")),
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_item_without_bytes_or_media_type_is_refused() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_with_id(&tmp, "http://unused.test/v1/messages", json!({}), "img2").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("img2"),
            json!({"type": "complete", "rid": "c~1", "image": {"ref": 1, "media_type": "image/png"}}),
        )
        .with_binary_key("other", vec![1])])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("img2"),
            json!({"type": "complete", "rid": "c~1", "image": {"ref": 1}}),
        )
        .with_binary_key("data", vec![1])])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_paired_chat_may_reflect_its_provider() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_with_id(&tmp, "http://unused.test/v1/messages", json!({}), "pair1").await;
    let chat =
        fantastic_ai_core::remote_provider::pair_chat(&kernel, &AgentId::from("pair1")).await;
    let grants = helpers::agent_meta(&chat, &kernel)["can_call"]["pair1"].clone();
    assert_eq!(grants, json!(["complete", "watch", "reflect"]));
    let admits =
        helpers::agent_meta(&AgentId::from("pair1"), &kernel)["callable_by"][chat.as_str()].clone();
    assert_eq!(admits, json!(["complete", "watch", "reflect"]));
}
