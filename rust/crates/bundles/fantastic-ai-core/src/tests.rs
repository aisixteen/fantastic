//! Shared verb/loop/assembly/state tests, driven through `kernel.send`
//! against a `MockProvider` (no HTTP). A real `fantastic-file` agent
//! handles persistence. Each test uses a unique agent id derived from
//! its tempdir so the process-global state map doesn't race.

use super::*;
use crate::agent_loop::BackendConfig;
use crate::events::CallerRoute;
use crate::helpers::safe_client;
use crate::provider::{Provider, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Agent, AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc::Receiver;

const HANDLER_MODULE: &str = "mock_backend.tools";

/// A provider that replays a fixed script of events, optionally after a
/// per-call delay (to exercise interrupt / status mid-flight). Each
/// `chat()` call pops the next scripted pass; once exhausted it yields
/// an empty pass (terminates the loop).
struct MockProvider {
    passes: std::sync::Mutex<std::collections::VecDeque<Vec<ScriptEvent>>>,
    delay: Duration,
}

#[derive(Clone)]
enum ScriptEvent {
    Token(String),
    Tool {
        id: String,
        name: String,
        args: Value,
    },
}

impl MockProvider {
    fn boxed(passes: Vec<Vec<ScriptEvent>>, delay: Duration) -> Arc<dyn Provider> {
        Arc::new(Self {
            passes: std::sync::Mutex::new(passes.into_iter().collect()),
            delay,
        })
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn chat(&self, _messages: &[Value], _tools: &[Value]) -> Result<ProviderStream, String> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let pass = self.passes.lock().unwrap().pop_front().unwrap_or_default();
        let evs: Vec<Result<ProviderEvent, String>> = pass
            .into_iter()
            .map(|e| {
                Ok(match e {
                    ScriptEvent::Token(t) => ProviderEvent::Token(t),
                    ScriptEvent::Tool { id, name, args } => {
                        ProviderEvent::ToolCall { id, name, args }
                    }
                })
            })
            .collect();
        Ok(Box::pin(futures_util::stream::iter(evs)))
    }
    fn model(&self) -> String {
        "test-model".to_string()
    }
}

/// The test bundle: dispatches every verb through ai-core, building a
/// `MockProvider` from a process-global script keyed by agent id.
struct MockBundle;

/// A scripted generation: a sequence of provider passes + a per-call delay.
type Script = (Vec<Vec<ScriptEvent>>, Duration);
type ScriptMap = std::collections::HashMap<String, Script>;

static SCRIPTS: std::sync::OnceLock<std::sync::Mutex<ScriptMap>> = std::sync::OnceLock::new();

fn scripts() -> std::sync::MutexGuard<'static, ScriptMap> {
    SCRIPTS
        .get_or_init(|| std::sync::Mutex::new(ScriptMap::new()))
        .lock()
        .unwrap()
}

const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::CliRoundTrip,
    tool_args_as_json: false,
    parallel_tools: true,
    name: "mock_backend",
};

#[async_trait]
impl Bundle for MockBundle {
    fn name(&self) -> &str {
        "mock_backend"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => {
                json!({
                    "id": agent_id.as_str(),
                    "sentence": "Mock LLM agent.",
                    "model": helpers::meta_string_or(agent_id, kernel, "model", "test-model"),
                    "file_bridge_id": helpers::file_bridge_id(agent_id, kernel),
                    "generating": crate::state::is_generating(agent_id),
                    "verbs": {"send": "send", "history": "history"},
                    "emits": {"token": "t"},
                })
            }
            "boot" => Value::Null,
            "send" => {
                if helpers::file_bridge_id(agent_id, kernel).is_none() {
                    json!({"error": "mock_backend: file_bridge_id required"})
                } else {
                    let (passes, delay) = scripts()
                        .get(agent_id.as_str())
                        .cloned()
                        .unwrap_or_default();
                    let provider = MockProvider::boxed(passes, delay);
                    verbs::send(provider, agent_id, payload, kernel, CFG).await
                }
            }
            "history" => verbs::history(agent_id, payload, kernel, "mock_backend").await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "interrupt" => verbs::interrupt(agent_id),
            "refresh_menu" => verbs::refresh_menu(agent_id),
            "status" => verbs::status(agent_id, payload),
            other => json!({"error": format!("mock_backend: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }
}

fn id_for(tmp: &TempDir, tag: &str) -> String {
    format!(
        "mb_{}_{}",
        tag,
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

async fn mk_kernel(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, MockBundle);
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

    let backend_id = id_for(tmp, tag);
    let file_id = format!("ff_{}", backend_id);
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "id": file_id,
                "root": tmp.path().join(".fantastic").to_string_lossy(),
                // the fs edge seals by default — open it so history persists through it
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
                "model": "test-model",
            }),
        )
        .await;
    (kernel, AgentId::from(backend_id.as_str()))
}

/// Rebind the backend's inbox (CliRoundTrip emits non-cli events there).
fn rebind_backend_inbox(kernel: &Arc<Kernel>, backend: &AgentId) -> Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(kernel.inbox_bound);
    kernel.inboxes.insert(backend.clone(), tx);
    rx
}

async fn drain(rx: &mut Receiver<Value>) -> Vec<Value> {
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

#[tokio::test]
async fn reflect_reports_state_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "refl").await;
    let r = kernel.send(&backend, json!({"type": "reflect"})).await;
    for key in [
        "id",
        "sentence",
        "model",
        "file_bridge_id",
        "generating",
        "verbs",
    ] {
        assert!(r.get(key).is_some(), "reflect missing key {key:?}: {r:#?}");
    }
    assert_eq!(r["id"], backend.as_str());
    assert_eq!(r["generating"], false);
}

#[tokio::test]
async fn boot_is_noop() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "boot").await;
    let r = kernel.send(&backend, json!({"type": "boot"})).await;
    assert!(
        r.is_null(),
        "boot should be a noop returning null, got {r:?}"
    );
}

#[tokio::test]
async fn send_without_file_bridge_id_returns_error() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, MockBundle);
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
            json!({"type": "create_agent", "handler_module": HANDLER_MODULE, "id": "mb_nofile"}),
        )
        .await;
    let r = kernel
        .send(
            &AgentId::from("mb_nofile"),
            json!({"type": "send", "text": "hi"}),
        )
        .await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("file_bridge_id"),
        "expected file_bridge_id error, got {r:?}",
    );
}

#[tokio::test]
async fn send_streams_tokens_and_emits_done() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tok").await;
    scripts().insert(
        backend.as_str().to_string(),
        (
            vec![vec![
                ScriptEvent::Token("Hello".into()),
                ScriptEvent::Token(" ".into()),
                ScriptEvent::Token("world".into()),
            ]],
            Duration::ZERO,
        ),
    );
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
        "expected a done event: {events:#?}"
    );
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
    assert!(phases.contains("thinking"), "missing thinking: {phases:?}");
    assert!(
        phases.contains("streaming"),
        "missing streaming: {phases:?}"
    );
    assert!(phases.contains("done"), "missing done: {phases:?}");
}

// ── Context Protocol: budget primitives + strategies (no kernel) ─────

fn turn(role: &str, content: &str) -> Value {
    json!({"role": role, "content": content})
}

fn body(n: usize, size: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            turn(role, &format!("turn{i}-{}", "x".repeat(size)))
        })
        .collect()
}

/// A provider that yields one fixed summary token (for `compact`).
struct SummaryProvider(&'static str);
#[async_trait]
impl Provider for SummaryProvider {
    async fn chat(&self, _m: &[Value], _t: &[Value]) -> Result<ProviderStream, String> {
        let ev = Ok(ProviderEvent::Token(self.0.to_string()));
        Ok(Box::pin(futures_util::stream::iter(vec![ev])))
    }
    fn model(&self) -> String {
        "sum".into()
    }
}

#[test]
fn context_estimate_and_budget() {
    use crate::context::{budget, estimate_one, estimate_tokens, resolve_context_window};
    assert!(estimate_one(&turn("user", &"x".repeat(400))) >= 100);
    assert!(estimate_tokens(&body(4, 100)) > estimate_tokens(&body(2, 100)));
    let mut m = Map::new();
    m.insert("context_window".into(), json!(8000));
    m.insert("num_ctx".into(), json!(2000));
    assert_eq!(resolve_context_window(&m), 8000);
    let mut m2 = Map::new();
    m2.insert("num_ctx".into(), json!(2000));
    assert_eq!(resolve_context_window(&m2), 2000);
    assert_eq!(resolve_context_window(&Map::new()), 4096);
    let mut tiny = Map::new();
    tiny.insert("context_window".into(), json!(100));
    tiny.insert("output_reserve".into(), json!(1000));
    assert_eq!(budget(&tiny), 256); // floor
}

#[tokio::test]
async fn compact_returns_projection_with_summary() {
    let b = body(10, 80);
    let provider: Arc<dyn Provider> = Arc::new(SummaryProvider("SUMMARY"));
    let proj = crate::strategies::compact(&b, 4, 1_000_000, &provider).await;
    assert_eq!(proj.summary.as_deref(), Some("SUMMARY"));
    assert!(!proj.omitted_marker);
    // recent verbatim, NO fabricated summary turn in the body
    assert_eq!(proj.body, b[b.len() - 4..].to_vec());
}

#[test]
fn truncate_returns_projection_no_summary() {
    let b = body(10, 80);
    let proj = crate::strategies::truncate(&b, 1_000_000);
    assert!(proj.omitted_marker && proj.summary.is_none());
    assert_eq!(proj.body[0], b[0]); // first kept
    assert_eq!(proj.body[proj.body.len() - 1], b[b.len() - 1]); // live turn kept
}

#[test]
fn context_notice_shapes() {
    let n = crate::projection::context_notice("compact", Some("MY SUMMARY"), false, 7);
    assert_eq!(n["role"], "user");
    assert!(n.get("tool_calls").is_none());
    let c = n["content"].as_str().unwrap();
    assert!(c.contains("[context-notice]") && c.contains("MY SUMMARY"));
    assert!(c.contains("recall") && c.contains("memory agent"));
    assert!(c.contains("7 earlier turn"));
    let t = crate::projection::context_notice("truncate", None, true, 3);
    let tc = t["content"].as_str().unwrap();
    assert!(tc.contains("omitted in place") && !tc.contains("Summary of the dropped span"));
}

// ── Context Protocol: recall + context_status (kernel-driven) ────────

#[tokio::test]
async fn recall_pages_durable_store_and_context_status_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "recall").await;
    scripts().insert(
        backend.as_str().to_string(),
        (
            vec![vec![ScriptEvent::Token("noted".into())]],
            Duration::ZERO,
        ),
    );
    // Plant a distinctive fact in the durable store via a normal send.
    let _ = kernel
        .send(
            &backend,
            json!({"type": "send", "text": "my project codename is HALCYON", "client_id": "chat"}),
        )
        .await;
    // recall finds it by substring (case-insensitive), full durable history.
    let r = kernel
        .send(
            &backend,
            json!({"type": "recall", "query": "codename", "client_id": "chat"}),
        )
        .await;
    let msgs = r["messages"].as_array().expect("recall messages");
    assert!(
        msgs.iter().any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("halcyon")),
        "recall did not page the planted fact back: {r:#?}"
    );
    assert_eq!(r["client_id"], "chat");

    // context_status shape — default strategy, projection recorded.
    let st = kernel
        .send(&backend, json!({"type": "context_status"}))
        .await;
    for key in [
        "context_window",
        "output_reserve",
        "budget",
        "strategy",
        "last_projection",
        "last_reaction",
    ] {
        assert!(
            st.get(key).is_some(),
            "context_status missing {key:?}: {st:#?}"
        );
    }
    assert_eq!(st["strategy"], "compact");
    // a normal (under-budget) send does not fire compaction
    assert_eq!(st["last_projection"]["fired"], false);
}

#[tokio::test]
async fn tool_call_dispatches_and_persists_full_history() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tool").await;
    // Pass 1: a tool_call to `core` list_agents. Pass 2: final text.
    scripts().insert(
        backend.as_str().to_string(),
        (
            vec![
                vec![ScriptEvent::Tool {
                    id: "call_1".into(),
                    name: "send".into(),
                    args: json!({"target_id": "core", "payload": {"type": "list_agents"}}),
                }],
                vec![ScriptEvent::Token("done".into())],
            ],
            Duration::ZERO,
        ),
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_tool";
    let reply = kernel
        .send(
            &backend,
            json!({"type": "send", "text": "go", "client_id": client_id}),
        )
        .await;
    assert_eq!(reply["response"], "done");

    let events = drain(&mut rx).await;
    assert!(
        events.iter().any(|e| {
            e.get("type").and_then(Value::as_str) == Some("say")
                && e.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .contains("[tool core ->")
        }),
        "expected a say(tool core) event: {events:#?}"
    );

    // Persisted history must include the tool turns (full messages[1:]).
    let h = kernel
        .send(&backend, json!({"type": "history", "client_id": client_id}))
        .await;
    let msgs = h["messages"].as_array().unwrap();
    assert!(
        msgs.iter().any(|m| m["role"] == "tool"),
        "expected a role:tool message in persisted history: {msgs:#?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m["role"] == "assistant" && m.get("tool_calls").is_some()),
        "expected an assistant turn carrying tool_calls: {msgs:#?}"
    );
}

#[tokio::test]
async fn history_persists_and_round_trips() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hist").await;
    scripts().insert(
        backend.as_str().to_string(),
        (vec![vec![ScriptEvent::Token("ack".into())]], Duration::ZERO),
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_hist";
    let _ = kernel
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

#[tokio::test]
async fn interrupt_cancels_in_flight() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "intr").await;
    scripts().insert(
        backend.as_str().to_string(),
        (
            vec![vec![ScriptEvent::Token("slow".into())]],
            Duration::from_secs(2),
        ),
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_intr";
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let c = client_id.to_string();
    let send_join = tokio::spawn(async move {
        k.send(&b, json!({"type": "send", "text": "hi", "client_id": c}))
            .await
    });
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
    let (kernel, backend) = mk_kernel(&tmp, "rmen").await;
    let state = crate::state::state_for(&backend);
    *state.menu.lock().unwrap() = Some(vec![json!({"id": "sentinel"})]);
    assert!(state.menu.lock().unwrap().is_some());
    let r = kernel.send(&backend, json!({"type": "refresh_menu"})).await;
    assert_eq!(r["refreshed"], true);
    assert!(state.menu.lock().unwrap().is_none());
}

#[tokio::test]
async fn status_during_in_flight_shows_phase() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "stat").await;
    scripts().insert(
        backend.as_str().to_string(),
        (
            vec![vec![ScriptEvent::Token("x".into())]],
            Duration::from_millis(500),
        ),
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_stat";
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let c = client_id.to_string();
    let send_join = tokio::spawn(async move {
        k.send(&b, json!({"type": "send", "text": "hi", "client_id": c}))
            .await
    });
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
    let phase = cur["phase"].as_str().unwrap_or("");
    assert!(
        ["thinking", "streaming", "tool_calling"].contains(&phase),
        "unexpected phase {phase:?}: {snap:#?}",
    );
    let _ = send_join.await.expect("send join");
}
