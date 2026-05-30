//! Bundle unit tests.
//!
//! The registry is process-global, so tests serialize via
//! [`test_lock`] (same isolation pattern as the FM + proxy_agent
//! bundles). Each test calls [`clear`] at start to start from a clean
//! slate.
#![allow(clippy::await_holding_lock)]

use super::*;
use fantastic_kernel::{Agent, StorageMode};
use fantastic_proxy_agent::{
    clear_hosts, register_host as register_proxy_host, ProxyAgentBundle, ProxyAgentHost,
    HANDLER_MODULE as PROXY_HM,
};
use serde_json::Map;
use std::sync::{Mutex, MutexGuard, OnceLock as StdOnceLock};

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: StdOnceLock<Mutex<()>> = StdOnceLock::new();
    let m = LOCK.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn mk_kernel(storage: StorageMode) -> Arc<Kernel> {
    clear();
    clear_hosts();
    let mut kernel = Kernel::with_storage(storage);
    kernel.bundles.register(HANDLER_MODULE, ToolsBundle::new());
    kernel.bundles.register(PROXY_HM, ProxyAgentBundle::new());
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

async fn create_tools_agent(kernel: &Arc<Kernel>) -> AgentId {
    let r = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"tools"}),
        )
        .await;
    assert_eq!(r["id"], "tools", "create_agent reply: {r}");
    AgentId::from("tools")
}

async fn create_proxy_agent(kernel: &Arc<Kernel>, id: &str) -> AgentId {
    let r = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":PROXY_HM,"id":id}),
        )
        .await;
    assert_eq!(r["id"], id, "create_agent reply: {r}");
    AgentId::from(id)
}

/// Records every payload it sees + lets the test pick the JSON reply.
#[derive(Default)]
struct RecordingHost {
    seen: Mutex<Vec<String>>,
    reply: Mutex<String>,
}

impl RecordingHost {
    fn new(reply: &str) -> Arc<Self> {
        let h = Arc::new(Self::default());
        *h.reply.lock().unwrap() = reply.to_string();
        h
    }
    fn last(&self) -> Option<Value> {
        let s = self.seen.lock().unwrap().last().cloned()?;
        serde_json::from_str(&s).ok()
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl ProxyAgentHost for RecordingHost {
    fn handle(&self, payload_json: String) -> String {
        self.seen.lock().unwrap().push(payload_json);
        self.reply.lock().unwrap().clone()
    }
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("tools"));
}

#[tokio::test]
async fn reflect_on_empty_registry() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let r = kernel.send(&a, json!({"type":"reflect"})).await;
    assert_eq!(r["id"], "tools");
    assert_eq!(r["kind"], "tools");
    assert_eq!(r["tool_count"], 0);
    assert!(r["verbs"]["register"].is_string());
    assert!(r["verbs"]["dispatch"].is_string());
    assert!(r["verbs"]["list_for_llm"].is_string());
}

#[tokio::test]
async fn register_minimum_payload() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let r = kernel
        .send(
            &a,
            json!({
                "type": "register",
                "name": "ping",
                "agent_id": "health_check",
                "description": "Returns pong.",
                "parameters_schema": {"type":"object","properties":{},"additionalProperties":false},
            }),
        )
        .await;
    assert_eq!(r["ok"], true);
    assert_eq!(r["name"], "ping");
    let list = kernel.send(&a, json!({"type":"list"})).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["tools"][0]["name"], "ping");
    assert_eq!(list["tools"][0]["agent_id"], "health_check");
    assert_eq!(list["tools"][0]["verb"], Value::Null);
}

#[tokio::test]
async fn register_maximum_payload_preserves_all_fields() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let schema = json!({
        "type":"object",
        "properties": {
            "query": {"type":"string","minLength":1},
            "limit": {"type":"integer","default":5,"maximum":50},
            "scope": {"type":"string","enum":["all","engineering","policy"],"default":"all"},
        },
        "required": ["query"],
        "additionalProperties": false,
        "if":  { "properties": { "scope": { "const": "policy" } } },
        "then": { "required": ["query"] },
    });
    let r = kernel
        .send(
            &a,
            json!({
                "type": "register",
                "name": "search",
                "agent_id": "doc_index",
                "verb": "do_search",
                "description": "Search docs.",
                "parameters_schema": schema.clone(),
            }),
        )
        .await;
    assert_eq!(r["ok"], true);

    let list = kernel.send(&a, json!({"type":"list"})).await;
    let entry = &list["tools"][0];
    assert_eq!(entry["name"], "search");
    assert_eq!(entry["agent_id"], "doc_index");
    assert_eq!(entry["verb"], "do_search");
    assert_eq!(entry["description"], "Search docs.");
    assert_eq!(entry["parameters_schema"], schema);
}

#[tokio::test]
async fn register_with_schema_as_string_is_parsed() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({
                "type": "register",
                "name": "ping",
                "agent_id": "x",
                "description": "d",
                "parameters_schema": r#"{"type":"object","properties":{}}"#,
            }),
        )
        .await;
    let list = kernel.send(&a, json!({"type":"list"})).await;
    // String got parsed into a JSON object, not stored verbatim as a string.
    assert!(list["tools"][0]["parameters_schema"].is_object());
    assert_eq!(list["tools"][0]["parameters_schema"]["type"], "object");
}

#[tokio::test]
async fn register_same_name_last_write_wins() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"x","agent_id":"a1","description":"first",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"x","agent_id":"a2","description":"second",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    let list = kernel.send(&a, json!({"type":"list"})).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["tools"][0]["agent_id"], "a2");
    assert_eq!(list["tools"][0]["description"], "second");
}

#[tokio::test]
async fn register_captures_sender_from_payload() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"x","agent_id":"a","description":"d",
                "sender":"owner_alpha",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    let entry = lookup("x").expect("x registered");
    assert_eq!(entry.sender, AgentId::from("owner_alpha"));
}

#[tokio::test]
async fn register_without_sender_defaults_to_anonymous() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"x","agent_id":"a","description":"d",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    let entry = lookup("x").expect("x registered");
    assert_eq!(entry.sender, AgentId::from("anonymous"));
}

#[tokio::test]
async fn register_from_different_senders_different_names_both_present() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({"type":"register","name":"alpha_tool","agent_id":"a","description":"d",
                   "sender":"alpha",
                   "parameters_schema":{"type":"object"}}),
        )
        .await;
    let _ = kernel
        .send(
            &a,
            json!({"type":"register","name":"beta_tool","agent_id":"b","description":"d",
                   "sender":"beta",
                   "parameters_schema":{"type":"object"}}),
        )
        .await;
    let list = kernel.send(&a, json!({"type":"list"})).await;
    assert_eq!(list["count"], 2);
    let names: Vec<&str> = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alpha_tool"));
    assert!(names.contains(&"beta_tool"));
    assert_eq!(lookup("alpha_tool").unwrap().sender.as_str(), "alpha");
    assert_eq!(lookup("beta_tool").unwrap().sender.as_str(), "beta");
}

#[tokio::test]
async fn unregister_by_name() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({"type":"register","name":"x","agent_id":"a","description":"d",
                   "parameters_schema":{"type":"object"}}),
        )
        .await;
    let r = kernel
        .send(&a, json!({"type":"unregister","name":"x"}))
        .await;
    assert_eq!(r["ok"], true);
    let list = kernel.send(&a, json!({"type":"list"})).await;
    assert_eq!(list["count"], 0);
}

#[tokio::test]
async fn unregister_non_existent_returns_not_found() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let r = kernel
        .send(&a, json!({"type":"unregister","name":"nope"}))
        .await;
    assert_eq!(r["reason"], "not_found");
    assert!(r["error"].as_str().unwrap().contains("nope"));
}

#[tokio::test]
async fn unregister_by_sender_drops_only_owners_tools() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    for n in &["a1", "a2"] {
        let _ = kernel
            .send(
                &a,
                json!({"type":"register","name":n,"agent_id":"a","description":"d",
                       "sender":"alpha",
                       "parameters_schema":{"type":"object"}}),
            )
            .await;
    }
    let _ = kernel
        .send(
            &a,
            json!({"type":"register","name":"b1","agent_id":"b","description":"d",
                   "sender":"beta",
                   "parameters_schema":{"type":"object"}}),
        )
        .await;

    let reply = kernel
        .send(&a, json!({"type":"unregister_by_sender","sender":"alpha"}))
        .await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["removed"], 2);
    assert_eq!(reply["sender"], "alpha");

    let list = kernel.send(&a, json!({"type":"list"})).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["tools"][0]["name"], "b1");
}

#[tokio::test]
async fn clear_drops_all() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    for n in &["a", "b", "c"] {
        let _ = kernel
            .send(
                &a,
                json!({"type":"register","name":n,"agent_id":"x","description":"d",
                       "parameters_schema":{"type":"object"}}),
            )
            .await;
    }
    let r = kernel.send(&a, json!({"type":"clear"})).await;
    assert_eq!(r["ok"], true);
    assert_eq!(r["removed"], 3);
    let list = kernel.send(&a, json!({"type":"list"})).await;
    assert_eq!(list["count"], 0);
}

#[tokio::test]
async fn list_for_llm_returns_compact_shape() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"ping","agent_id":"hc","description":"Returns pong.",
                "parameters_schema":{"type":"object","properties":{}},
            }),
        )
        .await;
    let r = kernel.send(&a, json!({"type":"list_for_llm"})).await;
    let tools = r["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    // Shape is {name, description, parameters} — NO sender, NO agent_id, NO verb.
    assert_eq!(tools[0]["name"], "ping");
    assert_eq!(tools[0]["description"], "Returns pong.");
    assert!(tools[0]["parameters"].is_object());
    assert!(tools[0].get("sender").is_none());
    assert!(tools[0].get("agent_id").is_none());
    assert!(tools[0].get("verb").is_none());
}

#[tokio::test]
async fn dispatch_routes_to_target_with_explicit_verb() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let target = create_proxy_agent(&kernel, "calc").await;
    let host = RecordingHost::new(r#"{"result":5}"#);
    register_proxy_host(target.clone(), host.clone());

    // Register a tool that maps name="calc_add" → agent="calc" verb="add".
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"calc_add","agent_id":"calc","verb":"add",
                "description":"Add two numbers.",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    let r = kernel
        .send(
            &a,
            json!({"type":"dispatch","name":"calc_add","arguments":{"a":2,"b":3}}),
        )
        .await;
    assert_eq!(r["result"], 5);

    // Host saw {"type":"add", "a":2, "b":3} — verb was injected, args flattened.
    let seen = host.last().expect("host saw payload");
    assert_eq!(seen["type"], "add");
    assert_eq!(seen["a"], 2);
    assert_eq!(seen["b"], 3);
}

#[tokio::test]
async fn dispatch_with_no_verb_uses_tool_name() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let target = create_proxy_agent(&kernel, "echo").await;
    let host = RecordingHost::new(r#"{"ok":true}"#);
    register_proxy_host(target.clone(), host.clone());
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"echo","agent_id":"echo","description":"d",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    let _ = kernel
        .send(
            &a,
            json!({"type":"dispatch","name":"echo","arguments":{"x":1}}),
        )
        .await;
    let seen = host.last().expect("host saw payload");
    assert_eq!(seen["type"], "echo");
    assert_eq!(seen["x"], 1);
}

#[tokio::test]
async fn dispatch_unknown_tool_returns_tool_not_found() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let target = create_proxy_agent(&kernel, "calc").await;
    let host = RecordingHost::new(r#"{"ok":true}"#);
    register_proxy_host(target.clone(), host.clone());

    let r = kernel
        .send(
            &a,
            json!({"type":"dispatch","name":"never_registered","arguments":{}}),
        )
        .await;
    assert_eq!(r["reason"], "tool_not_found");
    // No traffic to the target.
    assert_eq!(host.count(), 0);
}

#[tokio::test]
async fn dispatch_reply_passes_through_unchanged() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let target = create_proxy_agent(&kernel, "stub").await;
    // Deliberately weird reply shape so we can confirm we don't wrap it.
    let host = RecordingHost::new(r#"{"a":1,"b":[true,false],"nested":{"k":"v"}}"#);
    register_proxy_host(target.clone(), host.clone());
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"stub","agent_id":"stub","description":"d",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    let r = kernel
        .send(&a, json!({"type":"dispatch","name":"stub","arguments":{}}))
        .await;
    assert_eq!(r["a"], 1);
    assert_eq!(r["b"][0], true);
    assert_eq!(r["b"][1], false);
    assert_eq!(r["nested"]["k"], "v");
}

#[tokio::test]
async fn dispatch_after_unregister_returns_tool_not_found() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let target = create_proxy_agent(&kernel, "x").await;
    let host = RecordingHost::new(r#"{"ok":true}"#);
    register_proxy_host(target.clone(), host.clone());
    let _ = kernel
        .send(
            &a,
            json!({
                "type":"register","name":"x","agent_id":"x","description":"d",
                "parameters_schema":{"type":"object"},
            }),
        )
        .await;
    // First dispatch reaches the host.
    let _ = kernel
        .send(&a, json!({"type":"dispatch","name":"x","arguments":{}}))
        .await;
    assert_eq!(host.count(), 1);
    // Now unregister and dispatch again — no new traffic.
    let _ = kernel
        .send(&a, json!({"type":"unregister","name":"x"}))
        .await;
    let r = kernel
        .send(&a, json!({"type":"dispatch","name":"x","arguments":{}}))
        .await;
    assert_eq!(r["reason"], "tool_not_found");
    assert_eq!(host.count(), 1, "host MUST NOT see the second dispatch");
}

#[tokio::test]
async fn cascade_delete_clears_registry() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let _ = kernel
        .send(
            &a,
            json!({"type":"register","name":"x","agent_id":"y","description":"d",
                   "parameters_schema":{"type":"object"}}),
        )
        .await;
    assert_eq!(count(), 1);
    // Cascade-delete the tools agent.
    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"delete_agent","id":"tools"}),
        )
        .await;
    assert_eq!(count(), 0, "on_delete cleared the registry");
}

#[tokio::test]
async fn concurrent_registers_do_not_panic() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let mut handles = Vec::new();
    for i in 0..20 {
        let k = Arc::clone(&kernel);
        let a = a.clone();
        handles.push(tokio::spawn(async move {
            let n = format!("tool_{i}");
            k.send(
                &a,
                json!({
                    "type":"register","name":n,"agent_id":"a","description":"d",
                    "parameters_schema":{"type":"object"},
                }),
            )
            .await
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    assert_eq!(count(), 20);
}

#[tokio::test]
async fn unregister_by_sender_without_sender_field_returns_invalid_args() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    let r = kernel
        .send(&a, json!({"type":"unregister_by_sender"}))
        .await;
    assert_eq!(r["reason"], "invalid_args");
    assert!(r["error"].as_str().unwrap().contains("sender"));
}

#[tokio::test]
async fn register_dispatch_unregister_publish_state_events() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;

    // Subscribe to state events and collect them.
    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);
    let _token = kernel.add_state_subscriber(Arc::new(move |ev: &Value| {
        events_clone.lock().unwrap().push(ev.clone());
    }));

    let _ = kernel
        .send(
            &a,
            json!({"type":"register","name":"x","agent_id":"y","description":"d",
                   "parameters_schema":{"type":"object"}}),
        )
        .await;
    let _ = kernel
        .send(&a, json!({"type":"dispatch","name":"never","arguments":{}}))
        .await;
    let _ = kernel
        .send(&a, json!({"type":"unregister","name":"x"}))
        .await;

    // The kernel publishes one "send" event per kernel.send call.
    // Filter to events targeting the tools agent and verify all three
    // verbs are represented in order.
    let captured = events.lock().unwrap().clone();
    let tools_sends: Vec<&Value> = captured
        .iter()
        .filter(|e| e["type"] == "send" && e["target"] == a.as_str())
        .collect();
    assert_eq!(
        tools_sends.len(),
        3,
        "expected 3 send events for tools agent, got {} events total: {captured:?}",
        tools_sends.len()
    );
    assert_eq!(tools_sends[0]["verb"], "register");
    assert_eq!(tools_sends[1]["verb"], "dispatch");
    assert_eq!(tools_sends[2]["verb"], "unregister");
    // Each event has summary + sender field.
    for ev in &tools_sends {
        assert!(ev["summary"].is_string(), "summary missing on {ev}");
        assert!(ev["sender"].is_string(), "sender missing on {ev}");
    }
}

#[tokio::test]
async fn dispatch_invalid_arguments_returns_invalid_args() {
    let _g = test_lock();
    let kernel = mk_kernel(StorageMode::InMemory);
    let a = create_tools_agent(&kernel).await;
    // Register first so the lookup succeeds and we reach the args check.
    let _ = kernel
        .send(
            &a,
            json!({"type":"register","name":"x","agent_id":"y","description":"d",
                   "parameters_schema":{"type":"object"}}),
        )
        .await;
    let r = kernel
        .send(
            &a,
            json!({"type":"dispatch","name":"x","arguments":"not an object"}),
        )
        .await;
    assert_eq!(r["reason"], "invalid_args");
}
