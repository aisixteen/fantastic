//! End-to-end demo for the tools bundle. Registers two tools that
//! dispatch to a mock proxy_agent target (a plain-Rust host doing
//! arithmetic), exercises the full register / list / dispatch /
//! unregister round-trip, and asserts the conditional gate (after
//! removal the tool can't be called).
//!
//! Run: `cargo run -p fantastic-tools --example tools_mock_session`

use fantastic_kernel::{Agent, AgentId, Kernel, StorageMode};
use fantastic_proxy_agent::{
    clear_hosts, register_host, ProxyAgentBundle, ProxyAgentHost, HANDLER_MODULE as PROXY_HM,
};
use fantastic_tools::{clear, ToolsBundle, HANDLER_MODULE as TOOLS_HM};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Calculator host. Answers `add` / `subtract` and counts calls so
/// the demo can prove the un-registered dispatch path doesn't reach
/// the target.
struct CalcHost {
    calls: AtomicUsize,
    last: Mutex<Option<Value>>,
}

impl CalcHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            last: Mutex::new(None),
        })
    }
}

impl ProxyAgentHost for CalcHost {
    fn handle(&self, payload_json: String) -> String {
        let p: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
        self.calls.fetch_add(1, Ordering::Relaxed);
        *self.last.lock().unwrap() = Some(p.clone());
        let a = p.get("a").and_then(Value::as_i64).unwrap_or(0);
        let b = p.get("b").and_then(Value::as_i64).unwrap_or(0);
        match p.get("type").and_then(Value::as_str).unwrap_or("") {
            "add" => json!({"result": a + b}).to_string(),
            "subtract" => json!({"result": a - b}).to_string(),
            _ => json!({"ok": true}).to_string(),
        }
    }
}

fn banner(s: &str) {
    println!("\n────────────────────────────────────────────────────────────");
    println!(" {s}");
    println!("────────────────────────────────────────────────────────────");
}

fn assert_or_die(cond: bool, label: &str) {
    if cond {
        println!("  ✓ {label}");
    } else {
        eprintln!("  ✗ {label}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() {
    clear();
    clear_hosts();

    banner("Step 1: boot in-memory kernel + register tools + proxy_agent bundles");
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

    banner("Step 2: create the tools agent + the calc proxy_agent");
    let r = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":TOOLS_HM,"id":"tools"}),
        )
        .await;
    assert_or_die(r["id"] == "tools", "created tools agent");
    let r = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":PROXY_HM,"id":"calc"}),
        )
        .await;
    assert_or_die(r["id"] == "calc", "created calc agent");

    banner("Step 3: register CalcHost on the calc agent");
    let calc_host = CalcHost::new();
    register_host(AgentId::from("calc"), calc_host.clone());
    assert_or_die(calc_host.calls.load(Ordering::Relaxed) == 0, "no calls yet");

    banner("Step 4: reflect on tools — should report tool_count = 0");
    let r = kernel
        .send(&AgentId::from("tools"), json!({"type":"reflect"}))
        .await;
    println!("  reflect: {r}");
    assert_or_die(r["tool_count"] == 0, "tool_count = 0");
    assert_or_die(r["kind"] == "tools", "kind = tools");

    banner("Step 5: register calc_add (verb=add) and calc_subtract (verb=subtract)");
    for (name, verb) in [("calc_add", "add"), ("calc_subtract", "subtract")] {
        let r = kernel
            .send(
                &AgentId::from("tools"),
                json!({
                    "type":"register",
                    "name": name,
                    "agent_id":"calc",
                    "verb": verb,
                    "sender":"calc_owner",
                    "description": format!("Compute {verb} of two integers."),
                    "parameters_schema": {
                        "type":"object",
                        "properties": {
                            "a": {"type":"integer"},
                            "b": {"type":"integer"}
                        },
                        "required": ["a","b"],
                        "additionalProperties": false,
                    },
                }),
            )
            .await;
        assert_or_die(r["ok"] == true, &format!("registered {name}"));
    }

    banner("Step 6: list_for_llm — confirm both tools visible in LLM-facing shape");
    let r = kernel
        .send(&AgentId::from("tools"), json!({"type":"list_for_llm"}))
        .await;
    println!("  list_for_llm: {r}");
    let arr = r["tools"].as_array().unwrap();
    assert_or_die(arr.len() == 2, "two tools listed");
    let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_or_die(names.contains(&"calc_add"), "calc_add listed");
    assert_or_die(names.contains(&"calc_subtract"), "calc_subtract listed");
    // LLM-facing shape: {name, description, parameters} — NO sender/agent_id/verb.
    assert_or_die(arr[0].get("sender").is_none(), "no sender in LLM shape");
    assert_or_die(arr[0].get("agent_id").is_none(), "no agent_id in LLM shape");

    banner("Step 7: dispatch calc_add(a=2, b=3) → 5");
    let r = kernel
        .send(
            &AgentId::from("tools"),
            json!({"type":"dispatch","name":"calc_add","arguments":{"a":2,"b":3}}),
        )
        .await;
    println!("  reply: {r}");
    assert_or_die(r["result"] == 5, "calc_add reply = 5");
    assert_or_die(
        calc_host.calls.load(Ordering::Relaxed) == 1,
        "calc_host received exactly 1 call",
    );

    banner("Step 8: dispatch calc_subtract(a=10, b=4) → 6");
    let r = kernel
        .send(
            &AgentId::from("tools"),
            json!({"type":"dispatch","name":"calc_subtract","arguments":{"a":10,"b":4}}),
        )
        .await;
    println!("  reply: {r}");
    assert_or_die(r["result"] == 6, "calc_subtract reply = 6");

    banner("Step 9: dispatch unknown tool → tool_not_found, no call to target");
    let calls_before = calc_host.calls.load(Ordering::Relaxed);
    let r = kernel
        .send(
            &AgentId::from("tools"),
            json!({"type":"dispatch","name":"never_registered","arguments":{}}),
        )
        .await;
    println!("  reply: {r}");
    assert_or_die(r["reason"] == "tool_not_found", "tool_not_found");
    assert_or_die(
        calc_host.calls.load(Ordering::Relaxed) == calls_before,
        "host count unchanged",
    );

    banner("Step 10: unregister calc_add — list now shows only calc_subtract");
    let r = kernel
        .send(
            &AgentId::from("tools"),
            json!({"type":"unregister","name":"calc_add"}),
        )
        .await;
    assert_or_die(r["ok"] == true, "unregister ok");
    let r = kernel
        .send(&AgentId::from("tools"), json!({"type":"list"}))
        .await;
    assert_or_die(r["count"] == 1, "count = 1 after removal");
    assert_or_die(
        r["tools"][0]["name"] == "calc_subtract",
        "only calc_subtract",
    );

    banner("Step 11: dispatch removed calc_add — MUST NOT reach calc_host");
    let calls_before = calc_host.calls.load(Ordering::Relaxed);
    let r = kernel
        .send(
            &AgentId::from("tools"),
            json!({"type":"dispatch","name":"calc_add","arguments":{"a":1,"b":1}}),
        )
        .await;
    println!("  reply: {r}");
    assert_or_die(r["reason"] == "tool_not_found", "tool_not_found");
    assert_or_die(
        calc_host.calls.load(Ordering::Relaxed) == calls_before,
        "removed tool did NOT reach calc_host",
    );

    banner("Step 12: unregister_by_sender(calc_owner) — drops calc_subtract too");
    let r = kernel
        .send(
            &AgentId::from("tools"),
            json!({"type":"unregister_by_sender","sender":"calc_owner"}),
        )
        .await;
    println!("  reply: {r}");
    assert_or_die(r["ok"] == true, "ok");
    assert_or_die(r["removed"] == 1, "removed = 1");
    let r = kernel
        .send(&AgentId::from("tools"), json!({"type":"list"}))
        .await;
    assert_or_die(r["count"] == 0, "registry empty");

    banner("ALL ASSERTIONS GREEN — tools round-trip healthy");
}
