//! Integration test: a proxy_agent host acting as a chat backend can
//! pull the current tool registry via `kernel.send("tools",
//! {list_for_llm})`. Proves the brain-kernel step-5 chain:
//!
//!   `kernel.send("fm", {send, text})`
//!     → ProxyAgent bundle forwards to Swift-style host
//!     → host's `handle` spawns a task that calls
//!       `kernel.send("tools", {list_for_llm})`
//!     → captured tools_json shape matches what Swift would feed
//!       Apple-FM `LanguageModelSession.tools`
//!
//! Replaces the old fm_tools_roundtrip.rs after FM bundle removal.
//! Same coverage, generic mechanism — works on any platform.
#![allow(clippy::await_holding_lock)]

use fantastic_kernel::{Agent, AgentId, Kernel, StorageMode};
use fantastic_proxy_agent::{
    clear_hosts, register_host, ProxyAgentBundle, ProxyAgentHost, HANDLER_MODULE as PROXY_HM,
};
use fantastic_tools::{clear as tools_clear, ToolsBundle, HANDLER_MODULE as TOOLS_HM};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock as StdOnceLock};

/// Serialize tests — both proxy_agent HOSTS and tools TOOLS are
/// process-global statics. Same isolation pattern as the bundle's
/// own tests.
fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: StdOnceLock<Mutex<()>> = StdOnceLock::new();
    let m = LOCK.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// A chat-backend proxy_agent host that, on `send`, pulls
/// `list_for_llm` from the tools agent and records what it got.
/// Stands in for the Swift `FoundationModelsProxyHost` in tests.
struct ChatBackendHost {
    kernel_and_id: Mutex<Option<(Arc<Kernel>, AgentId)>>,
    captured: Arc<Mutex<Option<Value>>>,
    pulled: Arc<AtomicBool>,
}

impl ChatBackendHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            kernel_and_id: Mutex::new(None),
            captured: Arc::new(Mutex::new(None)),
            pulled: Arc::new(AtomicBool::new(false)),
        })
    }
    fn attach(&self, kernel: Arc<Kernel>, agent_id: AgentId) {
        *self.kernel_and_id.lock().unwrap() = Some((kernel, agent_id));
    }
}

impl ProxyAgentHost for ChatBackendHost {
    fn handle(&self, payload_json: String) -> String {
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb != "send" {
            return json!({"ok": true}).to_string();
        }
        let kid = self.kernel_and_id.lock().unwrap().clone();
        let captured = Arc::clone(&self.captured);
        let pulled = Arc::clone(&self.pulled);
        if let Some((k, _id)) = kid {
            tokio::spawn(async move {
                let reply = k
                    .send(&AgentId::from("tools"), json!({"type":"list_for_llm"}))
                    .await;
                *captured.lock().unwrap() = Some(reply);
                pulled.store(true, Ordering::Relaxed);
            });
        }
        json!({"queued": true, "stream_id": "stm_t"}).to_string()
    }
}

fn mk_kernel() -> Arc<Kernel> {
    tools_clear();
    clear_hosts();
    let mut kernel = Kernel::with_storage(StorageMode::InMemory);
    kernel.bundles.register(TOOLS_HM, ToolsBundle::new());
    kernel.bundles.register(PROXY_HM, ProxyAgentBundle::new());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        Default::default(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

async fn wait_for(pulled: &AtomicBool) {
    for _ in 0..100 {
        if pulled.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("chat backend did not pull tools within 200ms");
}

#[tokio::test]
async fn chat_backend_pulls_registered_tools_during_send() {
    let _g = test_lock();
    let kernel = mk_kernel();

    // Create tools agent + register one tool.
    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":TOOLS_HM,"id":"tools"}),
        )
        .await;
    let _ = kernel
        .send(
            &AgentId::from("tools"),
            json!({
                "type":"register",
                "name":"get_weather",
                "agent_id":"weather_agent",
                "verb":"lookup",
                "description":"Returns the current weather for a city.",
                "parameters_schema":{
                    "type":"object",
                    "properties":{"city":{"type":"string","minLength":1}},
                    "required":["city"],
                    "additionalProperties":false,
                },
                "sender":"app",
            }),
        )
        .await;

    // Create the chat-backend agent on proxy_agent + attach host.
    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":PROXY_HM,"id":"fm"}),
        )
        .await;
    let host = ChatBackendHost::new();
    host.attach(Arc::clone(&kernel), AgentId::from("fm"));
    register_host(AgentId::from("fm"), host.clone());

    // Fire send — host's spawned task pulls list_for_llm.
    let r = kernel
        .send(
            &AgentId::from("fm"),
            json!({"type":"send","text":"hi","client_id":"cli"}),
        )
        .await;
    assert_eq!(r["queued"], true, "fm send queued: {r}");

    wait_for(&host.pulled).await;

    let captured = host.captured.lock().unwrap().clone().expect("pulled tools");
    let tools = captured["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(
        tools[0]["description"],
        "Returns the current weather for a city."
    );
    assert!(tools[0]["parameters"].is_object());
    // LLM-facing shape MUST NOT leak implementation details.
    assert!(tools[0].get("sender").is_none());
    assert!(tools[0].get("agent_id").is_none());
    assert!(tools[0].get("verb").is_none());
}

#[tokio::test]
async fn chat_backend_sees_empty_array_when_no_tools_registered() {
    let _g = test_lock();
    let kernel = mk_kernel();

    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":TOOLS_HM,"id":"tools"}),
        )
        .await;
    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":PROXY_HM,"id":"fm"}),
        )
        .await;
    let host = ChatBackendHost::new();
    host.attach(Arc::clone(&kernel), AgentId::from("fm"));
    register_host(AgentId::from("fm"), host.clone());

    let _ = kernel
        .send(
            &AgentId::from("fm"),
            json!({"type":"send","text":"hi","client_id":"cli"}),
        )
        .await;
    wait_for(&host.pulled).await;

    let captured = host.captured.lock().unwrap().clone().expect("pulled tools");
    let tools = captured["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 0, "empty registry → empty tools array");
}
