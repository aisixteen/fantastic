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
        caching: Caching::Cleanup,
        usage: Arc::new(Mutex::new(Usage::default())),
        base: Arc::new(Mutex::new(Usage::default())),
        usage_file: None,
        report: ReportCell::default(),
        stream_idle,
        long_context_tokens: None,
        usage_log: None,
        agent_id: "a1".into(),
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
    mk_with(tmp, endpoint, json!({})).await
}

async fn mk_with(tmp: &TempDir, endpoint: &str, extra: Value) -> Arc<Kernel> {
    mk_with_id(tmp, endpoint, extra, "a1").await
}

async fn mk_with_id(tmp: &TempDir, endpoint: &str, extra: Value, id: &str) -> Arc<Kernel> {
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

/// The conversation's `messages` as ai-core flags them, with a system row first.
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

#[test]
fn cleanup_and_none_send_no_marker_even_when_a_message_is_flagged() {
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    let expected = json!({"model": MODEL, "max_tokens": 8000, "messages": [{"role": "user", "content": "hi"}],
                          "stream": true, "system": "s"});
    assert_eq!(p.body(&plain(), None), expected);
    let flags = flagged(&[
        ("user", "a", Some("cache_primer")),
        ("assistant", "b", Some("cache_fold")),
    ]);
    for mode in [Caching::Cleanup, Caching::None] {
        p.caching = mode;
        let b = p.body(&flags, None);
        assert_eq!(markers(&b), 0, "{mode:?}: {b}");
        assert_eq!(b["system"], json!("S"), "{mode:?}: the plain system string");
        assert_eq!(
            b["messages"],
            json!([
                {"role": "user", "content": "a"},
                {"role": "assistant", "content": "b"},
            ]),
            "{mode:?}: the flags do not reach the wire"
        );
    }
}

#[test]
fn provider_cache_splits_the_merged_turns_at_the_flagged_rows() {
    // [primer rows][fold][fold][task rows]: the primer's row is the assistant turn
    // "p2"; the fold rows and the task rows are one merged user turn, and the marker
    // lands at the end of the last fold's text, not at the end of that merged turn.
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.caching = Caching::ProviderCache;
    let msgs = flagged(&[
        ("user", "p1", None),
        ("assistant", "p2", Some("cache_primer")),
        ("user", "f1", None),
        ("user", "f2", Some("cache_fold")),
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
    assert_eq!(b["cache_control"], marked, "auto: {b}");
    assert_eq!(markers(&b), 4, "M1, M2, M3 and auto: {b}");
}

#[test]
fn the_split_turns_concatenate_to_exactly_the_merged_string() {
    // The block texts of a merged turn, joined, are the string the turn was before
    // any split: no byte added or lost, wherever a flag falls. And no block is blank:
    // the API refuses a whitespace-only text block, so a flagged row followed by empty
    // rows must not leave a trailing "\n\n" block of its own.
    let cases: [(&[&str], &[Option<&str>]); 12] = [
        (&["one", "", "three"], &[Some("cache_primer"), None, None]),
        (&["one", "", "three"], &[None, Some("cache_fold"), None]),
        (
            &["one", "", "three"],
            &[None, Some("cache_primer"), Some("cache_fold")],
        ),
        (
            &["one", "", "three"],
            &[Some("cache_fold"), Some("cache_fold"), None],
        ),
        (&["one", "", "three"], &[None, None, Some("cache_primer")]),
        (&["one", ""], &[Some("cache_primer"), None]),
        (&["one", "", ""], &[Some("cache_fold"), None, None]),
        (&["", "one", ""], &[None, Some("cache_fold"), None]),
        (&["one", ""], &[Some("cache_primer"), Some("cache_fold")]),
        // a flagged row that is only whitespace, first in its turn, opens no block
        // of its own (the API refuses a blank block): it joins the next text
        (&[" ", "two"], &[Some("cache_primer"), None]),
        (
            &[" ", "two", "three"],
            &[Some("cache_fold"), Some("cache_primer"), None],
        ),
        // ...and after text, it closes the block it is part of, bytes unchanged
        (&["one", " "], &[None, Some("cache_fold")]),
    ];
    for (texts, shape) in cases {
        let rows: Vec<(&str, &str, Option<&str>)> = texts
            .iter()
            .zip(shape)
            .map(|(t, f)| ("user", *t, *f))
            .collect();
        let msgs = flagged(&rows);
        let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
        p.caching = Caching::ProviderCache;
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
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.caching = Caching::ProviderCache;
    let msgs = flagged(&[("user", "one", Some("cache_primer")), ("user", "", None)]);
    let b = p.body(&msgs, None);
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "one\n\n", "cache_control": {"type": "ephemeral"}},
        ]}]),
        "{b}"
    );
    assert_eq!(markers(&b), 3, "M1, M2 and auto: {b}");
}

#[test]
fn a_flagged_row_with_no_text_carries_no_marker() {
    // An empty block is refused by the API, so an empty flagged row opens none.
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.caching = Caching::ProviderCache;
    let msgs = flagged(&[("user", "a", None), ("user", "", Some("cache_primer"))]);
    let b = p.body(&msgs, None);
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "a\n\n"}]}]),
        "{b}"
    );
    assert_eq!(markers(&b), 2, "M1 and auto only: {b}");
}

#[test]
fn a_primer_and_a_fold_on_one_message_send_one_marker() {
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.caching = Caching::ProviderCache;
    let mut msgs = flagged(&[
        ("user", "only", None),
        ("assistant", "both", Some("cache_primer")),
    ]);
    msgs[2]["cache_fold"] = json!(true);
    let b = p.body(&msgs, None);
    assert_eq!(
        b["messages"][1]["content"],
        json!([{"type": "text", "text": "both", "cache_control": {"type": "ephemeral"}}]),
        "{b}"
    );
    assert_eq!(markers(&b), 3, "M1, the one shared block, auto: {b}");
}

#[test]
fn provider_cache_with_no_primer_and_no_fold_marks_only_the_system_and_the_tail() {
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.caching = Caching::ProviderCache;
    let b = p.body(
        &flagged(&[("user", "a", None), ("assistant", "b", None)]),
        None,
    );
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}])
    );
    assert_eq!(markers(&b), 2, "M1 and auto: {b}");
}

#[test]
fn provider_cache_marks_the_system_block_and_the_tail() {
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.caching = Caching::ProviderCache;
    p.temperature = Some(0.2);
    p.thinking_budget = Some(2048);
    assert_eq!(
        p.body(&plain(), None),
        json!({
            "model": MODEL,
            "max_tokens": 8000,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "cache_control": {"type": "ephemeral"},
            "temperature": 0.2,
            "thinking": {"type": "enabled", "budget_tokens": 2048},
        })
    );
    // a side call's own bound is marked the same way
    let b = p.body(&plain(), Some(1500));
    assert_eq!(b["max_tokens"], json!(1500));
    assert_eq!(b["cache_control"], json!({"type": "ephemeral"}));
}

#[test]
fn provider_cache_sends_no_system_block_when_the_system_is_empty() {
    let mut p = provider("http://x/v1/messages", Arc::new(NoKey));
    p.caching = Caching::ProviderCache;
    let b = p.body(&[json!({"role": "user", "content": "hi"})], None);
    assert!(b.get("system").is_none(), "{b}");
    assert_eq!(b["cache_control"], json!({"type": "ephemeral"}));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_without_cache_prices_uses_the_model_defaults() {
    let tmp = TempDir::new().unwrap();
    let base = json!({"model": "claude-sonnet-5-5", "price_input_per_mtok": 3.0, "price_output_per_mtok": 15.0});
    let kernel = mk_with_id(
        &tmp,
        "http://unused.test/v1/messages",
        base.clone(),
        "f314a",
    )
    .await;
    usage_of(&AgentId::from("f314a")).lock().unwrap().cache_read = 1_000_000;
    let got = call(&kernel, "f314a", json!({"type": "usage"})).await;
    assert!(
        (got["cost_usd"].as_f64().unwrap() - 0.15).abs() < 1e-12,
        "{got}"
    );
    // an explicit cache-read price on the record wins over the model default
    let tmp2 = TempDir::new().unwrap();
    let mut explicit = base;
    explicit["price_cache_read_per_mtok"] = json!(0.30);
    let kernel = mk_with_id(&tmp2, "http://unused.test/v1/messages", explicit, "f314b").await;
    usage_of(&AgentId::from("f314b")).lock().unwrap().cache_read = 1_000_000;
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
        json!({"price_input_per_mtok": 3.0, "price_output_per_mtok": 15.0}),
    )
    .await;
    {
        let counts = usage_of(&AgentId::from("a1"));
        let mut u = counts.lock().unwrap();
        (u.requests, u.input, u.cache_write, u.cache_read) = (1, 100, 400, 1000);
    }
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
    assert_eq!(check(json!({"price_input_per_mtok": 3})), None);
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
    assert!(check(json!({"price_output_per_mtok": "x"}))
        .unwrap()
        .contains("must be a number"));
    assert!(check(json!({"price_input_per_mtok": true}))
        .unwrap()
        .contains("must be a number"));
    assert!(check(json!({"price_input_per_mtok": -1}))
        .unwrap()
        .contains("zero or more"));
    let names: Vec<&str> = record_schema()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(names.contains(&"caching") && names.contains(&"cache_primer_until"));
    assert!(names.contains(&"effort") && names.contains(&"thinking_display"));
    assert!(!names.contains(&"prompt_cache"));
    assert!(names.contains(&"price_input_per_mtok") && names.contains(&"price_output_per_mtok"));
}

/// A kernel with the agent [`STALL_AGENT`] on a memory store, its record naming `endpoint` and
/// the given extra fields — the whole turn runs through ai-core, the read clock
/// included (#177).
async fn mk_with_store(tmp: &TempDir, endpoint: &str, extra: Value) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
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
    let kernel = mk_with_store(&tmp, &ep, json!({"stream_idle_seconds": 0.2})).await;
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
    assert_eq!(errors.len(), 1, "{rows}");
    let content = errors[0]["content"].as_str().unwrap_or_default();
    assert!(content.contains("stream_stalled"), "{content}");
    let status = call(&kernel, STALL_AGENT, json!({"type": "status"})).await;
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
    let models = r["models"].as_array().cloned().unwrap_or_default();
    assert_eq!(models.len(), 2, "{r}");
    assert_eq!(models[0]["id"], json!("claude-haiku-5-5"), "{r}");
    assert_eq!(models[0]["display_name"], json!("Haiku"), "{r}");
    assert_eq!(models[0]["max_input_tokens"], json!(200000), "{r}");
    assert_eq!(models[0]["max_tokens"], json!(64000), "{r}");
    assert_eq!(models[1]["id"], json!("x"), "{r}");
    assert!(models[1]["display_name"].is_null(), "{r}");
    assert!(models[1]["max_input_tokens"].is_null(), "{r}");
    assert!(models[1]["max_tokens"].is_null(), "{r}");
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
        fantastic_kernel::bootstrap::BootstrapOptions::one_shot(tmp.path()),
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
    json!({"price_input_per_mtok": 3.0, "price_output_per_mtok": 15.0})
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
