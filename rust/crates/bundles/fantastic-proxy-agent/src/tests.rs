//! Bundle unit tests.
//!
//! Uses a plain-Rust [`MockHost`] as the host impl. Tests serialize
//! via [`test_lock`] because the bundle's host registry is
//! process-global — multiple parallel tests would otherwise see each
//! other's hosts. Same isolation pattern as the FM bundle's tests.
#![allow(clippy::await_holding_lock)]

use super::*;
use fantastic_kernel::{Agent, StorageMode};
use serde_json::Map;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock as StdOnceLock};
use tempfile::TempDir;

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: StdOnceLock<Mutex<()>> = StdOnceLock::new();
    let m = LOCK.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Mock host that records what it was called with + lets each test
/// pick the reply shape.
#[derive(Default)]
struct MockHost {
    /// Replies to return, queued. If empty, `handle` returns
    /// `{"ok": true}`.
    replies: Mutex<Vec<String>>,
    /// Every payload passed to `handle`.
    seen: Mutex<Vec<String>>,
    on_boot_calls: AtomicUsize,
    on_delete_calls: AtomicUsize,
}

impl MockHost {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn with_reply(reply: &str) -> Arc<Self> {
        let h = Self::new();
        h.replies.lock().unwrap().push(reply.to_string());
        h
    }
    fn last_payload(&self) -> Option<String> {
        self.seen.lock().unwrap().last().cloned()
    }
}

impl ProxyAgentHost for MockHost {
    fn handle(&self, payload_json: String) -> String {
        self.seen.lock().unwrap().push(payload_json);
        self.replies
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| r#"{"ok":true}"#.to_string())
    }
    fn on_boot(&self) {
        self.on_boot_calls.fetch_add(1, Ordering::Relaxed);
    }
    fn on_delete(&self) {
        self.on_delete_calls.fetch_add(1, Ordering::Relaxed);
    }
}

fn mk_kernel(storage: StorageMode) -> Arc<Kernel> {
    clear_hosts();
    let mut kernel = Kernel::with_storage(storage);
    kernel
        .bundles
        .register(HANDLER_MODULE, ProxyAgentBundle::new());
    let kernel = Arc::new(kernel);
    let workdir = kernel.storage.workdir().map(|p| p.to_path_buf());
    let root_path = workdir
        .as_ref()
        .map(|w| w.join(".fantastic"))
        .unwrap_or_default();
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root_path,
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

async fn create_proxy_agent(kernel: &Arc<Kernel>, id: &str) -> AgentId {
    let r = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":id}),
        )
        .await;
    assert_eq!(r["id"], id, "create_agent reply: {r}");
    AgentId::from(id)
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("proxy_agent"));
}

#[tokio::test]
async fn reflect_no_host_says_host_registered_false() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let r = kernel.send(&a, json!({"type":"reflect"})).await;
    assert_eq!(r["id"], "ui");
    assert_eq!(r["host_registered"], false);
    assert_eq!(r["kind"], "proxy_agent");
    assert!(r["verbs"]["*"].is_string());
}

#[tokio::test]
async fn reflect_with_host_merges_replies() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::with_reply(
        r#"{"sentence":"I am Chat UI","extra_field":"yes","host_registered":false}"#,
    );
    register_host(a.clone(), host);
    let r = kernel.send(&a, json!({"type":"reflect"})).await;
    assert_eq!(r["sentence"], "I am Chat UI");
    assert_eq!(r["extra_field"], "yes");
    // Bundle overlays host_registered=true regardless of host claim.
    assert_eq!(r["host_registered"], true);
    // Bundle injects id + kind if host didn't provide them.
    assert_eq!(r["id"], "ui");
    assert_eq!(r["kind"], "proxy_agent");
}

#[tokio::test]
async fn send_no_host_returns_structured_error() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let r = kernel
        .send(&a, json!({"type":"render_token","delta":"x"}))
        .await;
    assert_eq!(r["reason"], "no_host");
    assert!(r["error"].as_str().unwrap().contains("no host registered"));
}

#[tokio::test]
async fn arbitrary_verb_forwards_to_host() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::with_reply(r#"{"rendered":true,"chars":3}"#);
    register_host(a.clone(), host.clone());

    let r = kernel
        .send(&a, json!({"type":"render_token","delta":"hi!"}))
        .await;
    assert_eq!(r["rendered"], true);
    assert_eq!(r["chars"], 3);

    // Host saw the original payload (JSON-stringified).
    let last = host.last_payload().expect("host called");
    let p: Value = serde_json::from_str(&last).unwrap();
    assert_eq!(p["type"], "render_token");
    assert_eq!(p["delta"], "hi!");
}

#[tokio::test]
async fn boot_with_host_fires_on_boot() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::new();
    register_host(a.clone(), host.clone());
    let r = kernel.send(&a, json!({"type":"boot"})).await;
    assert_eq!(r["ok"], true);
    assert_eq!(host.on_boot_calls.load(Ordering::Relaxed), 1);
    // Boot verb was forwarded to host.handle too.
    assert!(host.last_payload().unwrap().contains("\"boot\""));
}

#[tokio::test]
async fn boot_without_host_is_ok() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let r = kernel.send(&a, json!({"type":"boot"})).await;
    assert_eq!(r["ok"], true);
    assert_eq!(r["host_registered"], false);
}

#[tokio::test]
async fn shutdown_with_host_forwards() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::with_reply(r#"{"closed":true}"#);
    register_host(a.clone(), host);
    let r = kernel.send(&a, json!({"type":"shutdown"})).await;
    assert_eq!(r["closed"], true);
}

#[tokio::test]
async fn cascade_delete_fires_on_delete_and_drops_host() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::new();
    register_host(a.clone(), host.clone());
    let r = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"delete_agent","id":"ui"}),
        )
        .await;
    assert_eq!(r["deleted"], true);
    assert_eq!(host.on_delete_calls.load(Ordering::Relaxed), 1);
    // Host is dropped from the registry.
    assert!(host_for(&AgentId::from("ui")).is_none());
}

#[tokio::test]
async fn two_proxies_are_isolated() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui_a").await;
    let b = create_proxy_agent(&kernel, "ui_b").await;
    let host_a = MockHost::with_reply(r#"{"who":"A"}"#);
    let host_b = MockHost::with_reply(r#"{"who":"B"}"#);
    register_host(a.clone(), host_a);
    register_host(b.clone(), host_b);

    let r_a = kernel.send(&a, json!({"type":"ping"})).await;
    let r_b = kernel.send(&b, json!({"type":"ping"})).await;
    assert_eq!(r_a["who"], "A");
    assert_eq!(r_b["who"], "B");
}

#[tokio::test]
async fn unregister_returns_to_no_host_behaviour() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::with_reply(r#"{"hello":"world"}"#);
    register_host(a.clone(), host);
    let r1 = kernel.send(&a, json!({"type":"x"})).await;
    assert_eq!(r1["hello"], "world");

    unregister_host(&a);
    let r2 = kernel.send(&a, json!({"type":"x"})).await;
    assert_eq!(r2["reason"], "no_host");
}

#[tokio::test]
async fn host_returning_malformed_json_is_wrapped() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::with_reply("not even close to json");
    register_host(a.clone(), host);
    let r = kernel.send(&a, json!({"type":"x"})).await;
    assert_eq!(r["reason"], "host_reply_malformed");
    assert_eq!(r["reply_raw"], "not even close to json");
}

#[tokio::test]
async fn disk_mode_persists_record() {
    let _g = test_lock();
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(StorageMode::Disk(tmp.path().to_path_buf()));
    let _ = create_proxy_agent(&kernel, "ui").await;
    let agent_json = tmp.path().join(".fantastic/agents/ui/agent.json");
    assert!(agent_json.exists(), "proxy_agent record on disk");
    let raw = std::fs::read_to_string(&agent_json).unwrap();
    assert!(raw.contains("proxy_agent.tools"));
}

#[tokio::test]
async fn in_memory_mode_no_fs_leakage() {
    let _g = test_lock();
    let tmp = TempDir::new().unwrap();
    let prev = std::env::current_dir().ok();
    std::env::set_current_dir(tmp.path()).unwrap();

    let kernel = mk_kernel(StorageMode::InMemory);
    let _ = create_proxy_agent(&kernel, "ui").await;
    assert!(!tmp.path().join(".fantastic").exists());

    if let Some(p) = prev {
        let _ = std::env::set_current_dir(p);
    }
}

#[tokio::test]
async fn host_can_return_ok_ack_by_default() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "ui").await;
    let host = MockHost::new(); // no canned replies → default {"ok":true}
    register_host(a.clone(), host);
    let r = kernel.send(&a, json!({"type":"render","frame":7})).await;
    assert_eq!(r["ok"], true);
}

// ── Worked example: chat backend on proxy_agent ─────────────────────
//
// These tests prove the chat-backend verb shape works on top of the
// generic proxy_agent. The pattern is what an embedding host uses to
// wrap an on-device LLM:
//
//   handle({type:"send"})   → returns {queued, stream_id} immediately,
//                              then a background task streams tokens
//                              via kernel.emit on the host's own inbox
//   handle({type:"history"})    → returns stored messages
//   handle({type:"interrupt"})  → flips a cancel flag, returns
//                                  {interrupted: true}
//   handle({type:"backend_state"}) → returns availability probes
//
// The tools-pull path (kernel.send("tools", list_for_llm) called from
// the chat backend during streaming) is covered separately in
// fantastic-tools/tests/chat_backend_pulls_tools.rs — that test needs
// both bundles + the tools registry which would create a dev-dep
// cycle here. The streaming path (kernel.emit on the host's inbox →
// watcher fanout) is verified below.

/// Mock chat-backend host. Shared state lives behind Arc<Mutex<_>>
/// so the spawned streaming task can mutate it. Models an embedding
/// host that holds an LLM session plus per-turn bookkeeping.
struct ChatBackendMockHost {
    history: Arc<Mutex<Vec<Value>>>,
    cancel_flag: Arc<AtomicUsize>,
    /// Set on `attach` so handle()'s spawned task can address its own
    /// agent id + drive kernel.emit / kernel.send.
    kernel_and_id: Mutex<Option<(Arc<Kernel>, AgentId)>>,
    /// Signaled when the streaming task finishes — tests wait on this
    /// before asserting emitted state.
    stream_done: Arc<tokio::sync::Notify>,
}

impl ChatBackendMockHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            history: Arc::new(Mutex::new(Vec::new())),
            cancel_flag: Arc::new(AtomicUsize::new(0)),
            kernel_and_id: Mutex::new(None),
            stream_done: Arc::new(tokio::sync::Notify::new()),
        })
    }
    fn attach(&self, kernel: Arc<Kernel>, agent_id: AgentId) {
        *self.kernel_and_id.lock().unwrap() = Some((kernel, agent_id));
    }
}

impl ProxyAgentHost for ChatBackendMockHost {
    fn handle(&self, payload_json: String) -> String {
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "send" => {
                let text = payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.history.lock().unwrap().push(json!({
                    "role":"user","content":text,"complete":true
                }));
                // Stream two tokens + a done event from a spawned task,
                // simulating what an embedding host does via the kernel
                // proxy-emit path inside its LLM session callback.
                let kid = self.kernel_and_id.lock().unwrap().clone();
                let notify = Arc::clone(&self.stream_done);
                if let Some((k, id)) = kid {
                    tokio::spawn(async move {
                        k.emit(
                            &id,
                            json!({"type":"token","stream_id":"stm_t","delta":"hi "}),
                        )
                        .await;
                        k.emit(
                            &id,
                            json!({"type":"token","stream_id":"stm_t","delta":"there"}),
                        )
                        .await;
                        k.emit(&id, json!({"type":"done","stream_id":"stm_t"}))
                            .await;
                        notify.notify_one();
                    });
                }
                json!({"queued":true,"stream_id":"stm_t","message_id":"msg_t"}).to_string()
            }
            "history" => {
                let msgs = self.history.lock().unwrap().clone();
                json!({"messages": msgs, "client_id": "cli"}).to_string()
            }
            "interrupt" => {
                self.cancel_flag.fetch_add(1, Ordering::Relaxed);
                json!({"interrupted": true}).to_string()
            }
            "backend_state" => json!({
                "apple_intelligence_available": true,
                "model_available": true,
                "backend_registered": true,
                "in_flight": false,
            })
            .to_string(),
            "reflect" => json!({
                "sentence":"Mock chat backend on proxy_agent",
                "provider":"mock",
                "verbs":{"send":"...","history":"...","interrupt":"...","backend_state":"...","reflect":"..."},
            })
            .to_string(),
            _ => json!({"error":"unknown verb","reason":"unknown_verb"}).to_string(),
        }
    }
}

#[tokio::test]
async fn chat_backend_send_returns_queued_with_stream_id() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "fm").await;
    let host = ChatBackendMockHost::new();
    host.attach(Arc::clone(&kernel), a.clone());
    register_host(a.clone(), host.clone());

    let r = kernel
        .send(&a, json!({"type":"send","text":"hello","client_id":"cli"}))
        .await;
    assert_eq!(r["queued"], true);
    assert_eq!(r["stream_id"], "stm_t");
    assert_eq!(r["message_id"], "msg_t");
}

#[tokio::test]
async fn chat_backend_history_returns_messages_appended_by_send() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "fm").await;
    let host = ChatBackendMockHost::new();
    host.attach(Arc::clone(&kernel), a.clone());
    register_host(a.clone(), host.clone());

    let _ = kernel
        .send(&a, json!({"type":"send","text":"first","client_id":"cli"}))
        .await;
    let _ = kernel
        .send(&a, json!({"type":"send","text":"second","client_id":"cli"}))
        .await;
    let r = kernel
        .send(&a, json!({"type":"history","client_id":"cli"}))
        .await;
    let msgs = r["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], "first");
    assert_eq!(msgs[1]["content"], "second");
}

#[tokio::test]
async fn chat_backend_interrupt_sets_flag_and_returns_interrupted() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "fm").await;
    let host = ChatBackendMockHost::new();
    host.attach(Arc::clone(&kernel), a.clone());
    register_host(a.clone(), host.clone());

    let r = kernel.send(&a, json!({"type":"interrupt"})).await;
    assert_eq!(r["interrupted"], true);
    assert_eq!(host.cancel_flag.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn chat_backend_backend_state_returns_availability_probes() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "fm").await;
    let host = ChatBackendMockHost::new();
    host.attach(Arc::clone(&kernel), a.clone());
    register_host(a.clone(), host.clone());

    let r = kernel.send(&a, json!({"type":"backend_state"})).await;
    assert_eq!(r["apple_intelligence_available"], true);
    assert_eq!(r["model_available"], true);
    assert_eq!(r["backend_registered"], true);
    assert_eq!(r["in_flight"], false);
}

#[tokio::test]
async fn chat_backend_streams_tokens_via_emit_to_own_inbox() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_proxy_agent(&kernel, "fm").await;
    let host = ChatBackendMockHost::new();
    host.attach(Arc::clone(&kernel), a.clone());
    register_host(a.clone(), host.clone());

    // Subscribe to state events; we'll see one "send" event per
    // kernel.send and one "emit" event per kernel.emit. Tokens flow as
    // emit events targeting the fm agent.
    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);
    let _token = kernel.add_state_subscriber(Arc::new(move |ev: &Value| {
        events_clone.lock().unwrap().push(ev.clone());
    }));

    let r = kernel
        .send(&a, json!({"type":"send","text":"hi","client_id":"cli"}))
        .await;
    assert_eq!(r["queued"], true);

    // Wait for the spawned streaming task to finish.
    host.stream_done.notified().await;

    let captured = events.lock().unwrap().clone();
    let emit_tokens: Vec<&Value> = captured
        .iter()
        .filter(|e| e["type"] == "emit" && e["target"] == a.as_str() && e["verb"] == "token")
        .collect();
    let emit_done: Vec<&Value> = captured
        .iter()
        .filter(|e| e["type"] == "emit" && e["target"] == a.as_str() && e["verb"] == "done")
        .collect();
    assert_eq!(
        emit_tokens.len(),
        2,
        "expected 2 token events; got {emit_tokens:?}"
    );
    assert_eq!(
        emit_done.len(),
        1,
        "expected 1 done event; got {emit_done:?}"
    );
}
