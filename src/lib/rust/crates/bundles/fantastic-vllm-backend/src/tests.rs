//! vllm_backend — the wire it speaks, the turns it must not lose, and the
//! KV-cache session table it keeps per endpoint. Twin of python
//! `vllm_backend/tests/test_vllm_backend.py`, case for case.
//!
//! Every behaviour here traces to a measured failure on a live vLLM (the
//! production deployment, Qwen3.8-27B-FP8, vLLM 0.27.1): a dropped thought
//! channel, a turn spent thinking with no answer, a stream that died without
//! saying so, a system message the template refused, and a cache that only pays
//! off while the live set stays under the server's KV ceiling.

use super::*;
use fantastic_ai_core::credentials::CredentialError;
use fantastic_ai_core::think::{THINK_CLOSE, THINK_OPEN};
use fantastic_kernel::{Agent, Item};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(chunks: &[Value], done: bool) -> String {
    let mut lines: Vec<String> = chunks.iter().map(|c| format!("data: {c}")).collect();
    if done {
        lines.push("data: [DONE]".to_string());
    }
    lines.join("\n\n") + "\n\n"
}

fn delta(finish: Option<&str>, d: Value) -> Value {
    json!({"choices": [{"delta": d, "finish_reason": finish}]})
}

/// A mock vLLM under its OWN path prefix. The session tables are process-global
/// and keyed by endpoint (python clears them per test); a port the OS reuses
/// across tests would otherwise hand one test another's live conversations.
async fn server_with(body: String, status: u16) -> (MockServer, String) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/t{n}/v1/chat/completions")))
        .respond_with(ResponseTemplate::new(status).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let endpoint = format!("{}/t{n}/v1", server.uri());
    (server, endpoint)
}

/// The reader of a record with no `credentials`: no key, no header (python's
/// `_nokey`).
struct NoKey;

#[async_trait]
impl KeySource for NoKey {
    async fn key(&self, _endpoint: &str) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }
}

/// A reader handing out `keys` in turn, recording the endpoint each read was
/// for (python's `key` + `asked`).
struct Keys {
    keys: Mutex<VecDeque<&'static str>>,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl KeySource for Keys {
    async fn key(&self, endpoint: &str) -> Result<Option<String>, CredentialError> {
        self.asked.lock().unwrap().push(endpoint.to_string());
        Ok(self.keys.lock().unwrap().pop_front().map(str::to_string))
    }
}

fn provider(endpoint: &str) -> VllmProvider {
    let (sampling, _) = sampling(&Map::new());
    VllmProvider {
        agent_id: format!("agent@{endpoint}"),
        endpoint: endpoint.trim_end_matches('/').to_string(),
        model: "qwen3.8-27b".to_string(),
        key: Arc::new(NoKey),
        max_tokens: 8000,
        window: None,
        kv_slots: 7,
        kv_overflow: "evict".to_string(),
        kv_ttl: KV_TTL,
        sampling,
        explicit_budget: false,
        connect_timeout: CONNECT_TIMEOUT,
        read_clock: READ_TIMEOUT,
        report: ReportCell::default(),
    }
}

fn hi() -> Vec<Value> {
    vec![json!({"role": "user", "content": "hi"})]
}

async fn drain(
    p: &VllmProvider,
    messages: &[Value],
    max_tokens: Option<i64>,
) -> Result<String, ProviderError> {
    let mut stream = p.chat(messages, max_tokens).await?;
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        match ev? {
            ProviderEvent::Token(t) => out.push_str(&t),
        }
    }
    Ok(out)
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).expect("json body"))
        .collect()
}

// ─── the stream ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_thought_channel_is_wrapped_once_not_dropped() {
    let body = sse(
        &[
            delta(None, json!({"reasoning_content": "let me "})),
            // the other spelling a version may use
            delta(None, json!({"reasoning": "think"})),
            delta(None, json!({"content": "answer"})),
            delta(Some("stop"), json!({})),
            // the usage-only chunk
            json!({"choices": [], "usage": {"prompt_tokens": 42}}),
        ],
        true,
    );
    let (_server, endpoint) = server_with(body, 200).await;
    let p = provider(&endpoint);
    let out = drain(&p, &hi(), None).await.expect("stream");
    assert_eq!(out, format!("{THINK_OPEN}let me think{THINK_CLOSE}answer"));
    let r = p.report();
    assert_eq!(
        (r.stop_reason.as_deref(), r.prompt_tokens),
        (Some("stop"), Some(42))
    );
}

#[tokio::test]
async fn a_cut_main_turn_fails_naming_the_cut() {
    // the record's bound hit on a MAIN turn: it did not finish — never an answer
    let body = sse(
        &[
            delta(None, json!({"content": "half"})),
            delta(Some("length"), json!({})),
        ],
        true,
    );
    let (_server, endpoint) = server_with(body, 200).await;
    let p = provider(&endpoint);
    let e = drain(&p, &hi(), None)
        .await
        .expect_err("a cut main turn fails");
    assert!(e.message.contains("cut at max_tokens=8000"), "{e}");
    assert_eq!(p.report().stop_reason.as_deref(), Some("max_tokens"));
}

#[tokio::test]
async fn a_cut_bounded_side_call_is_its_answer() {
    // a fold summary passes its own bound: stopping there is what it asked for
    let body = sse(
        &[
            delta(None, json!({"content": "half"})),
            delta(Some("length"), json!({})),
        ],
        true,
    );
    let (_server, endpoint) = server_with(body, 200).await;
    let p = provider(&endpoint);
    assert_eq!(drain(&p, &hi(), Some(50)).await.expect("stream"), "half");
    assert_eq!(p.report().stop_reason.as_deref(), Some("max_tokens"));
}

#[tokio::test]
async fn a_stream_that_ends_without_finish_reason_is_an_error() {
    // vLLM died mid-answer: the text so far must never read as a completion
    let body = sse(&[delta(None, json!({"content": "partial"}))], false);
    let (_server, endpoint) = server_with(body, 200).await;
    let p = provider(&endpoint);
    let e = drain(&p, &hi(), None).await.expect_err("no finish");
    assert!(e.message.contains("without a finish_reason"), "{e}");
}

#[tokio::test]
async fn an_http_error_names_the_endpoint_and_the_body() {
    let (_server, endpoint) = server_with("{\"error\":\"bad effort\"}".to_string(), 400).await;
    let p = provider(&endpoint);
    let e = drain(&p, &hi(), None).await.expect_err("400");
    assert!(e.message.contains("answered HTTP 400: "), "{e}");
    assert!(e.message.contains("bad effort"), "{e}");
}

#[tokio::test]
async fn a_refused_connection_says_vllm_is_not_listening_yet() {
    // port 1: nothing listens — the host answers RST
    let p = provider("http://127.0.0.1:1/v1");
    let e = drain(&p, &hi(), None).await.expect_err("refused");
    assert!(e.message.contains("not listening yet"), "{e}");
}

#[tokio::test]
async fn a_stream_dropped_mid_answer_says_vllm_died() {
    // Headers, then half a chunk, then the socket closes — a vLLM that died
    // while streaming.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        let _ = sock
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  transfer-encoding: chunked\r\n\r\n20\r\ndata: {\"cho",
            )
            .await;
        let _ = sock.shutdown().await;
    });
    let p = provider(&format!("http://{addr}/v1"));
    let e = drain(&p, &hi(), None).await.expect_err("dropped");
    assert!(e.message.contains("died or restarted"), "{e}");
}

#[test]
fn each_connection_failure_says_what_it_means() {
    let t = |connect, timeout| classify(connect, timeout, "http://gpu:8000/v1", 5.0, 900.0).message;
    assert!(t(true, true).contains("within connect_timeout=5s — the instance is STOPPED"));
    assert!(t(true, false).contains("not listening yet"));
    assert!(t(false, true)
        .contains("http://gpu:8000/v1 sent no bytes for 900s; this turn is NOT complete"));
    assert!(t(false, true).contains("stream_stalled"));
    assert!(t(false, false).contains("died or restarted"));
}

#[test]
fn a_timeout_is_printed_like_python_g() {
    assert_eq!(py_g(5.0), "5");
    assert_eq!(py_g(900.0), "900");
    assert_eq!(py_g(2.5), "2.5");
    assert_eq!(py_g(0.25), "0.25");
}

// ─── the request ────────────────────────────────────────────────────────────

/// python `test_a_cap_that_is_the_whole_window_sends_no_max_tokens`: vLLM counts
/// prompt AND output against one window, so a turn capped at the window asks for
/// the rest of it — no `max_tokens`; a side call keeps its own bound; a cap below
/// the window is sent as set.
#[tokio::test]
async fn a_cap_that_is_the_whole_window_sends_no_max_tokens() {
    let (server, endpoint) = server_with(sse(&[delta(Some("stop"), json!({}))], true), 200).await;
    let mut p = provider(&endpoint);
    p.max_tokens = 262144;
    p.window = Some(262144);
    drain(&p, &hi(), None).await.expect("turn");
    drain(&p, &hi(), Some(2000)).await.expect("side call");
    let sent = bodies(&server).await;
    assert!(sent[0].get("max_tokens").is_none(), "{:?}", sent[0]);
    assert_eq!(sent[1]["max_tokens"], json!(2000));
    let (server2, endpoint2) = server_with(sse(&[delta(Some("stop"), json!({}))], true), 200).await;
    let mut q = provider(&endpoint2);
    q.window = Some(262144);
    drain(&q, &hi(), None).await.expect("turn");
    assert_eq!(bodies(&server2).await[0]["max_tokens"], json!(8000));
}

/// python `test_a_cap_the_prompt_leaves_no_room_for_sends_no_max_tokens`: 64000 on
/// a 65536 window with a 1537-token prompt is refused by vLLM. The prompt's bound
/// plus the cap past the window asks for the rest of it; a prompt that leaves the
/// room keeps the cap.
#[tokio::test]
async fn a_cap_the_prompt_leaves_no_room_for_sends_no_max_tokens() {
    let (server, endpoint) = server_with(sse(&[delta(Some("stop"), json!({}))], true), 200).await;
    let mut p = provider(&endpoint);
    p.max_tokens = 64000;
    p.window = Some(65536);
    let big = vec![json!({"role": "user", "content": "x".repeat(6000)})];
    drain(&p, &big, None).await.expect("big");
    assert_eq!(prompt_bound(&big), 4 + 6000 + MESSAGE_OVERHEAD);
    drain(&p, &hi(), None).await.expect("small");
    let sent = bodies(&server).await;
    assert!(sent[0].get("max_tokens").is_none(), "{:?}", sent[0]);
    assert_eq!(sent[1]["max_tokens"], json!(64000));
}

#[tokio::test]
async fn the_body_carries_the_defaults_and_only_what_the_record_set() {
    let (server, endpoint) = server_with(sse(&[delta(Some("stop"), json!({}))], true), 200).await;
    let mut p = provider(&endpoint);
    p.sampling.insert("seed".into(), json!(7));
    drain(&p, &hi(), None).await.expect("stream");
    let body = &bodies(&server).await[0];
    assert_eq!(body["model"], json!("qwen3.8-27b"));
    assert_eq!(body["max_tokens"], json!(8000));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(
        (&body["temperature"], &body["top_p"], &body["top_k"]),
        (&json!(0.2), &json!(0.8), &json!(20))
    );
    assert_eq!(body["reasoning_effort"], json!("medium"));
    assert_eq!(body["seed"], json!(7));
    // never defaulted: a penalty is sent only when the record sets one
    assert!(body.get("presence_penalty").is_none());
    assert!(body.get("frequency_penalty").is_none());
}

#[tokio::test]
async fn every_system_message_becomes_one_at_index_0() {
    let msgs = vec![
        json!({"role": "system", "content": "A"}),
        json!({"role": "user", "content": "q"}),
        json!({"role": "system", "content": "B"}),
    ];
    assert_eq!(
        one_system_message(&msgs),
        vec![
            json!({"role": "system", "content": "A\n\nB"}),
            json!({"role": "user", "content": "q"}),
        ]
    );
    let (server, endpoint) = server_with(sse(&[delta(Some("stop"), json!({}))], true), 200).await;
    let p = provider(&endpoint);
    drain(&p, &msgs, None).await.expect("stream");
    let roles: Vec<Value> = bodies(&server).await[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].clone())
        .collect();
    assert_eq!(roles, vec![json!("system"), json!("user")]);
}

#[tokio::test]
async fn the_default_budget_halves_on_a_smaller_call_and_none_below_the_floor() {
    let (server, endpoint) = server_with(sse(&[delta(Some("stop"), json!({}))], true), 200).await;
    let p = provider(&endpoint);
    drain(&p, &hi(), None).await.expect("turn"); // max_tokens 8000 → min(16000, 4000)
    drain(&p, &hi(), Some(2000)).await.expect("fold"); // → min(16000, 1000)
    drain(&p, &hi(), Some(40)).await.expect("tiny"); // half is 20, under the floor → none
    let sent: Vec<Value> = bodies(&server)
        .await
        .iter()
        .map(|b| b["thinking_token_budget"].clone())
        .collect();
    assert_eq!(sent, vec![json!(4000), json!(1000), Value::Null]);
    let last = last_budgets().lock().unwrap().get(&p.agent_id).copied();
    assert_eq!(last, Some(None));
}

#[tokio::test]
async fn an_explicit_budget_rules_the_turn_and_halves_on_a_side_call() {
    // the proxy runs' 8000: a main turn sends it as set; a fold summary bounded
    // at 2000 gets 1000 instead of being refused; a tiny call gets none
    let (server, endpoint) = server_with(sse(&[delta(Some("stop"), json!({}))], true), 200).await;
    let mut p = provider(&endpoint);
    p.sampling
        .insert("thinking_token_budget".into(), json!(8000));
    p.explicit_budget = true;
    p.max_tokens = 65536;
    drain(&p, &hi(), None).await.expect("turn");
    drain(&p, &hi(), Some(2000)).await.expect("fold");
    drain(&p, &hi(), Some(300)).await.expect("tiny");
    let sent: Vec<Value> = bodies(&server)
        .await
        .iter()
        .map(|b| b["thinking_token_budget"].clone())
        .collect();
    assert_eq!(sent, vec![json!(8000), json!(1000), Value::Null]);
}

// ─── the KV-cache session table ─────────────────────────────────────────────

#[test]
fn evict_rotates_the_least_recently_used_conversation_out() {
    let mut t = SessionTable::default();
    for (i, k) in ["a", "b", "c"].iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let now = i as f64;
        assert_eq!(t.admit(k, 3, "evict", 1800.0, now).unwrap(), None);
    }
    t.admit("a", 3, "evict", 1800.0, 10.0).unwrap(); // a used again: b is the idlest
    assert_eq!(
        t.admit("d", 3, "evict", 1800.0, 11.0).unwrap().as_deref(),
        Some("b")
    );
    assert_eq!(t.view()["conversations"], json!(["c", "a", "d"]));
    assert_eq!(t.view()["evicted"], json!(1));
}

#[test]
fn reject_refuses_the_newcomer_and_names_when_to_retry() {
    let mut t = SessionTable::default();
    t.admit("a", 1, "reject", 1800.0, 0.0).unwrap();
    let e = t.admit("b", 1, "reject", 1800.0, 100.0).unwrap_err();
    assert!(e.message.contains("retry in ~1700s"), "{e}");
    assert_eq!(e.kind, "KvSlotsFull");
    assert_eq!(t.view()["conversations"], json!(["a"]));
    assert_eq!(t.view()["rejected"], json!(1));
}

#[test]
fn an_idle_conversation_frees_its_slot_after_the_ttl() {
    let mut t = SessionTable::default();
    t.admit("a", 1, "reject", 60.0, 0.0).unwrap();
    assert_eq!(t.admit("b", 1, "reject", 60.0, 61.0).unwrap(), None);
    assert_eq!(t.view()["conversations"], json!(["b"]));
}

#[tokio::test]
async fn the_table_is_shared_per_endpoint_across_agents() {
    // two agents on ONE endpoint share the count; a second endpoint does not
    let ok = sse(&[delta(Some("stop"), json!({}))], true);
    let (_one, shared) = server_with(ok.clone(), 200).await;
    let (_two, elsewhere) = server_with(ok, 200).await;
    let mut p1 = provider(&shared);
    let mut p2 = provider(&shared);
    let mut other = provider(&elsewhere);
    for p in [&mut p1, &mut p2, &mut other] {
        p.kv_slots = 1;
        p.kv_overflow = "reject".to_string();
    }
    let first = vec![json!({"role": "user", "content": "first"})];
    let second = vec![json!({"role": "user", "content": "second"})];
    drain(&p1, &first, None).await.expect("admitted");
    let e = drain(&p2, &second, None).await.expect_err("full");
    assert_eq!(e.kind, "KvSlotsFull");
    drain(&other, &second, None)
        .await
        .expect("another endpoint");
    // the SAME conversation on the full endpoint is already live: admitted
    drain(&p2, &first, None).await.expect("same conversation");
}

#[test]
fn a_conversation_is_its_prefix() {
    let base = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "U"}),
    ];
    let mut grown = base.clone();
    grown.push(json!({"role": "assistant", "content": "A"}));
    grown.push(json!({"role": "user", "content": "U2"}));
    assert_eq!(conversation_key(&base), conversation_key(&grown)); // append-only: same
    let rewritten = vec![json!({"role": "system", "content": "S2"}), base[1].clone()];
    assert_ne!(conversation_key(&rewritten), conversation_key(&base));
    // python: sha256("S\x00U").hexdigest()[:16]
    assert_eq!(conversation_key(&base).len(), 16);
}

// ─── the write ──────────────────────────────────────────────────────────────

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

#[test]
fn a_write_the_backend_cannot_honour_is_refused() {
    let cases = [
        (
            json!({"reasoning_effort": "high"}),
            json!({}),
            "legal values: low, medium, xhigh",
        ),
        (
            json!({"kv_overflow": "drop"}),
            json!({}),
            "legal values: evict",
        ),
        (
            json!({"kv_slots": 0}),
            json!({}),
            "kv_slots must be a positive int",
        ),
        (
            json!({"max_tokens": true}),
            json!({}),
            "max_tokens must be a positive int",
        ),
        (
            json!({"kv_ttl": -1}),
            json!({}),
            "kv_ttl must be a positive number",
        ),
        (
            json!({"thinking_token_budget": 8000}),
            json!({"max_tokens": 8000}),
            "not below max_tokens=8000",
        ),
        (
            json!({"max_tokens": 1000}),
            json!({"thinking_token_budget": 2000}),
            "not below max_tokens=1000",
        ),
    ];
    for (meta, record, fragment) in cases {
        let why = validate(&obj(meta.clone()), &obj(record)).unwrap_or_default();
        assert!(why.contains(fragment), "{meta}: {why}");
    }
    assert_eq!(
        validate(&obj(json!({"kv_overflow": "drop"})), &Map::new()).unwrap(),
        "kv_overflow='drop' is not a rotation policy — legal values: evict (the \
         least-recently-used conversation rotates out), reject (refuse the newcomer)"
    );
}

#[test]
fn a_record_missing_required_fields_is_refused_at_the_write_naming_all() {
    // what a model spawning a sub-AI wrote (E37): the fields it could see
    let meta = json!({
        "endpoint": "http://vllm.test/v1",
        "model": "qwen3.8-27b",
        "max_tokens": 65536,
    });
    let err = validate(&obj(meta.clone()), &Map::new()).expect("refused");
    assert!(
        err.contains("missing: kv_overflow, memory_store_id —"),
        "{err}"
    );
    // each missing field is named WITH its hint, from the schema
    assert!(err.contains("kv_overflow = all slots live: evict"), "{err}");
    // an update to a complete record is judged on the MERGED state
    let mut full = obj(meta);
    full.insert("kv_overflow".into(), json!("evict"));
    full.insert("memory_store_id".into(), json!("memory"));
    assert_eq!(validate(&obj(json!({"seed": 3})), &full), None);
}

#[test]
fn a_consistent_write_passes() {
    let meta = json!({
        "endpoint": "http://vllm.test/v1",
        "model": "qwen3.8-27b",
        "max_tokens": 8000,
        "context_window": 262144,
        "kv_slots": 6,
        "kv_overflow": "evict",
        "thinking_token_budget": 4000,
        "reasoning_effort": "xhigh",
        "memory_store_id": "memory",
    });
    assert_eq!(validate(&obj(meta), &Map::new()), None);
}

// ─── through the kernel ─────────────────────────────────────────────────────

fn unique_id(tmp: &TempDir) -> String {
    format!(
        "v_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

async fn mk_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, VllmBundle);
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
    kernel
}

async fn create(kernel: &Arc<Kernel>, meta: Value) -> Value {
    let mut payload = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
    });
    for (k, v) in obj(meta) {
        payload[k] = v;
    }
    kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), payload)])
        .await
        .remove(0)
        .result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kernel_refuses_a_bad_record_at_the_write() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let r = create(
        &kernel,
        json!({"id": unique_id(&tmp), "max_tokens": 4000, "thinking_token_budget": 4000}),
    )
    .await;
    assert_eq!(r["reason"], json!("invalid_meta"), "{r}");
    assert!(
        r["error"]
            .as_str()
            .is_some_and(|e| e.contains("not below max_tokens=4000")),
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_shows_the_effective_posture_and_the_shared_table() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let id = unique_id(&tmp);
    // an endpoint no other test names, so the shared table starts empty
    let endpoint = format!("http://{id}.test/v1");
    let r = create(
        &kernel,
        json!({
            "id": id, "endpoint": endpoint, "model": "qwen3.8-27b", "max_tokens": 8000,
            "kv_slots": 7, "kv_overflow": "evict", "memory_store_id": "memory", "seed": 3,
            "context_window": 262144,
        }),
    )
    .await;
    assert_eq!(r["id"], json!(id), "{r}");
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(id.as_str()),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out["sentence"], json!(SENTENCE));
    // WHERE its key is read: absent here, shown as null (a pointer, never a secret).
    assert_eq!(out.get("credentials"), Some(&Value::Null), "{out}");
    assert_eq!(out["sampling"]["temperature"], json!(0.2));
    assert_eq!(out["sampling"]["seed"], json!(3));
    assert_eq!(out["thinking_token_budget_source"], json!("default"));
    assert_eq!(out["last_thinking_budget"], Value::Null);
    assert_eq!(out["timeouts"], json!({"connect": 5.0, "read": 900.0}));
    assert_eq!(
        out["kv"],
        json!({
            "slots": 7, "overflow": "evict", "ttl": 1800.0,
            "live": 0, "conversations": [], "evicted": 0, "rejected": 0,
        }),
        "{out}"
    );
}

#[test]
fn readme_is_python_verbatim() {
    // the readme a model reads about this backend is the PYTHON bundle's, byte
    // for byte — each runtime writing its own is how one agent described itself
    // differently depending on where it ran
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/ai/vllm/vllm_backend/src/vllm_backend/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

// ─── the key (an HTTPS door in front of the GPU) ────────────────────────────

/// No `credentials`: NO header at all (a keyless local vLLM); with them: Bearer,
/// read before EACH request — a rotated key is used by the next one, and the
/// reader is told the endpoint it is for (the store pins its host).
#[tokio::test]
async fn the_key_rides_as_a_bearer_header_only_when_set() {
    let ok = sse(&[delta(Some("stop"), json!({"content": "ok"}))], true);
    let (s1, ep1) = server_with(ok.clone(), 200).await;
    assert_eq!(drain(&provider(&ep1), &hi(), None).await.unwrap(), "ok");
    let (s2, ep2) = server_with(ok.clone(), 200).await;
    let keys = Arc::new(Keys {
        keys: Mutex::new(VecDeque::from(["k-123", "k-rotated"])),
        asked: Mutex::new(Vec::new()),
    });
    let mut p = provider(&ep2);
    p.key = Arc::clone(&keys) as Arc<dyn KeySource>;
    assert_eq!(drain(&p, &hi(), None).await.unwrap(), "ok");
    assert_eq!(
        drain(&p, &[json!({"role": "user", "content": "again"})], None)
            .await
            .unwrap(),
        "ok"
    );
    assert_eq!(*keys.asked.lock().unwrap(), vec![ep2.clone(), ep2.clone()]);
    let auth = |reqs: Vec<wiremock::Request>| {
        reqs.iter()
            .map(|r| {
                r.headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(auth(s1.received_requests().await.unwrap()), vec![None]);
    assert_eq!(
        auth(s2.received_requests().await.unwrap()),
        vec![
            Some("Bearer k-123".to_string()),
            Some("Bearer k-rotated".to_string())
        ]
    );
}

#[tokio::test]
async fn a_refused_key_says_to_name_credentials() {
    let (_s, ep) = server_with("unauthorized".to_string(), 401).await;
    let err = drain(&provider(&ep), &hi(), None).await.unwrap_err();
    assert!(
        err.message.contains("HTTP 401") && err.message.contains("`credentials`"),
        "{}",
        err.message
    );
    assert_eq!(
        err.message,
        format!(
            "vllm_backend: {} answered HTTP 401 — the door wants a key: name it in \
             `credentials` ({{store, ref}}): unauthorized",
            ep.trim_end_matches('/')
        )
    );
}

#[test]
fn the_schema_takes_optional_credentials() {
    assert!(record_schema().get("key_var").is_none());
    let f = record_schema()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "credentials")
        .cloned()
        .unwrap();
    assert_eq!(f["type"], json!("credentials"));
    assert_eq!(f["required"], json!(false));
}

// ─── PINS ───────────────────────────────────────────────────────────────────

/// The unknown-type reply, as TEXT — a bare `{error}`; key order is wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let id = unique_id(&tmp);
    let r = create(
        &kernel,
        json!({"id": id, "endpoint": format!("http://{id}.test/v1"), "model": "m",
               "max_tokens": 8000, "kv_slots": 2, "kv_overflow": "evict",
               "memory_store_id": "memory", "context_window": 262144}),
    )
    .await;
    assert_eq!(r["id"], json!(id), "{r}");
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(id.as_str()),
            json!({"type": "frob"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"vllm_backend: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#
    );
}

/// The per-agent `last_thinking_budget` is THIS agent's (a sibling of the same
/// kind reads null), and `on_delete` frees it: an agent re-created under the
/// same id starts with none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_last_budget_is_per_agent_and_freed_on_delete() {
    let (_server, endpoint) = server_with(
        sse(&[delta(Some("stop"), json!({"content": "ok"}))], true),
        200,
    )
    .await;
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp).await;
    let id = unique_id(&tmp);
    let sib = format!("{id}_sib");
    let memory =
        fantastic_ram_store::test_support::memory_meta(&kernel, &id, json!({"cli": "*"})).await;
    let mut meta = json!({"id": id, "endpoint": endpoint, "model": "m", "max_tokens": 8000,
                          "kv_slots": 4, "kv_overflow": "evict", "context_window": 262144});
    for (k, v) in memory {
        meta[k] = v;
    }
    let r = create(&kernel, meta.clone()).await;
    assert_eq!(r["id"], json!(id), "{r}");
    let r = create(
        &kernel,
        json!({"id": sib, "endpoint": endpoint, "model": "m", "max_tokens": 8000,
               "kv_slots": 4, "kv_overflow": "evict", "memory_store_id": "memory",
               "context_window": 262144}),
    )
    .await;
    assert_eq!(r["id"], json!(sib), "{r}");
    let last = |who: String| {
        let kernel = Arc::clone(&kernel);
        async move {
            kernel
                .send(vec![Item::new(
                    AgentId::from(who.as_str()),
                    json!({"type": "reflect"}),
                )])
                .await
                .remove(0)
                .result["last_thinking_budget"]
                .clone()
        }
    };
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(id.as_str()),
            json!({"type": "send", "text": "hi", "client_id": "t", "reply_to": null}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out["response"], json!("ok"), "{out}");
    // The call-less answer is REROLLED in the background (the no-call reminder)
    // until the chain gives up; let it settle, or a reroll that lands after the
    // delete below records a budget for the re-created id.
    let mut quiet = 0;
    for _ in 0..500 {
        let st = kernel
            .send(vec![Item::new(
                AgentId::from(id.as_str()),
                json!({"type": "status"}),
            )])
            .await
            .remove(0)
            .result;
        quiet = if st["generating"] == json!(false) && st["current"].is_null() {
            quiet + 1
        } else {
            0
        };
        if quiet >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(quiet >= 5, "the chain never settled");
    let mine = last(id.clone()).await;
    assert!(mine.is_i64(), "the pass recorded what it sent: {mine}");
    assert_eq!(
        last(sib.clone()).await,
        Value::Null,
        "a sibling shares nothing"
    );
    let gone = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": id}),
        )])
        .await
        .remove(0)
        .result;
    assert!(gone.get("error").is_none(), "{gone}");
    let r = create(&kernel, meta).await;
    assert_eq!(r["id"], json!(id), "{r}");
    assert_eq!(last(id.clone()).await, Value::Null, "freed on delete");
}

/// A server that sends its headers and one fragment of a line, then goes silent.
async fn quiet_server(headers_only: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        if headers_only {
            // the stall before any answer: no bytes at all
            tokio::time::sleep(Duration::from_secs(600)).await;
            return;
        }
        let _ = sock
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  transfer-encoding: chunked\r\n\r\n20\r\ndata: {\"cho",
            )
            .await;
        tokio::time::sleep(Duration::from_secs(600)).await;
    });
    format!("http://{addr}/v1")
}

/// A stream that goes quiet fails after the record's `stream_idle_seconds`, naming
/// the stall, the seconds and the endpoint (#177).
#[tokio::test]
async fn a_stream_that_goes_quiet_stalls_on_the_read_clock() {
    let ep = quiet_server(false).await;
    let mut p = provider(&ep);
    p.read_clock = 0.2;
    let e = drain(&p, &hi(), None).await.expect_err("stalled");
    assert!(e.message.contains("stream_stalled"), "{e}");
    assert!(e.message.contains("sent no bytes for 0.2s"), "{e}");
    assert!(e.message.contains(&ep), "{e}");
}

/// Headers that never come are the same stall (#177).
#[tokio::test]
async fn a_server_that_never_answers_stalls_on_the_read_clock() {
    let ep = quiet_server(true).await;
    let mut p = provider(&ep);
    p.read_clock = 0.2;
    let e = drain(&p, &hi(), None).await.expect_err("stalled");
    assert!(e.message.contains("stream_stalled"), "{e}");
    assert!(e.message.contains("sent no bytes for 0.2s"), "{e}");
}

/// `stream_idle_seconds` is a positive number of seconds, refused at the write
/// otherwise; a record without it keeps its `read_timeout` as the read clock (#177).
#[test]
fn stream_idle_seconds_is_validated_and_read_clock_defaults_to_read_timeout() {
    let meta = |v: Value| {
        let mut m = Map::new();
        m.insert("stream_idle_seconds".to_string(), v);
        m
    };
    for bad in [json!(0), json!(-1), json!("soon"), json!(true)] {
        let msg =
            validate(&meta(bad.clone()), &Map::new()).unwrap_or_else(|| panic!("{bad} is refused"));
        assert!(
            msg.contains("stream_idle_seconds must be a positive number"),
            "{msg}"
        );
    }
    assert_eq!(read_clock_of(&Map::new(), 900.0).ok(), Some(900.0));
    assert_eq!(read_clock_of(&meta(json!(2.5)), 900.0).ok(), Some(2.5));
    assert!(read_clock_of(&meta(json!(0)), 900.0).is_err());
}
