//! End-to-end demo for the proxy_agent bundle. Two host-implemented
//! agents (`chat_ui` + `settings_ui`) talk to each other through the
//! kernel — same flow an embedding host would drive, with a plain-Rust
//! mock host as the host impl.
//!
//! Run: `cargo run -p fantastic-proxy-agent --example proxy_mock_session`

use fantastic_kernel::{Agent, AgentId, Kernel, StorageMode};
use fantastic_proxy_agent::{
    clear_hosts, register_host, ProxyAgentBundle, ProxyAgentHost, HANDLER_MODULE,
};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Mock host. Records every payload + lets the demo print what each
/// side received.
struct PrintingHost {
    label: String,
    seen: Mutex<Vec<Value>>,
    boots: AtomicUsize,
    deletes: AtomicUsize,
}

impl PrintingHost {
    fn new(label: &str) -> Arc<Self> {
        Arc::new(Self {
            label: label.to_string(),
            seen: Mutex::new(Vec::new()),
            boots: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        })
    }
}

impl ProxyAgentHost for PrintingHost {
    fn handle(&self, payload_json: String) -> String {
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or_else(|_| Value::Null);
        println!("  [{}] host.handle ← {}", self.label, payload);
        self.seen.lock().unwrap().push(payload.clone());
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "reflect" => json!({
                "sentence": format!("PrintingHost backing {}", self.label),
                "received_count": self.seen.lock().unwrap().len(),
            })
            .to_string(),
            "render_token" => json!({"rendered": true, "label": self.label}).to_string(),
            "ping" => json!({"pong": true, "from": self.label}).to_string(),
            _ => json!({"ok": true, "label": self.label}).to_string(),
        }
    }
    fn on_boot(&self) {
        self.boots.fetch_add(1, Ordering::Relaxed);
        println!("  [{}] host.on_boot fired", self.label);
    }
    fn on_delete(&self) {
        self.deletes.fetch_add(1, Ordering::Relaxed);
        println!("  [{}] host.on_delete fired", self.label);
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
    clear_hosts();
    banner("Step 1: boot an in-memory kernel + register the proxy_agent bundle");
    let mut kernel = Kernel::with_storage(StorageMode::InMemory);
    kernel
        .bundles
        .register(HANDLER_MODULE, ProxyAgentBundle::new());
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
    println!("  kernel ready, storage=InMemory");

    banner("Step 2: create two proxy_agent instances (chat_ui + settings_ui)");
    for id in ["chat_ui", "settings_ui"] {
        let r = kernel
            .send(
                &AgentId::from("core"),
                json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":id}),
            )
            .await;
        assert_or_die(r["id"] == id, &format!("created {id}"));
    }

    banner("Step 3: register embedding hosts (PrintingHost) for both");
    let chat = AgentId::from("chat_ui");
    let settings = AgentId::from("settings_ui");
    let chat_host = PrintingHost::new("chat_ui");
    let settings_host = PrintingHost::new("settings_ui");
    register_host(chat.clone(), chat_host.clone());
    register_host(settings.clone(), settings_host.clone());

    banner("Step 4: fire boot on each — host.on_boot should run");
    let _ = kernel.send(&chat, json!({"type":"boot"})).await;
    let _ = kernel.send(&settings, json!({"type":"boot"})).await;
    assert_or_die(
        chat_host.boots.load(Ordering::Relaxed) == 1,
        "chat_ui on_boot fired once",
    );
    assert_or_die(
        settings_host.boots.load(Ordering::Relaxed) == 1,
        "settings_ui on_boot fired once",
    );

    banner("Step 5: send arbitrary verbs — forwarded to host.handle");
    let r1 = kernel
        .send(&chat, json!({"type":"render_token","delta":"Hello"}))
        .await;
    println!("  chat_ui reply: {r1}");
    assert_or_die(r1["rendered"] == true, "chat_ui rendered=true");
    assert_or_die(r1["label"] == "chat_ui", "label routed correctly");

    let r2 = kernel.send(&settings, json!({"type":"ping"})).await;
    println!("  settings_ui reply: {r2}");
    assert_or_die(r2["pong"] == true, "settings_ui pong=true");

    banner("Step 6: reflect — merges host's response with bundle identity");
    let r3 = kernel.send(&chat, json!({"type":"reflect"})).await;
    println!("  chat_ui reflect: {r3}");
    assert_or_die(r3["host_registered"] == true, "host_registered=true");
    assert_or_die(
        r3["sentence"].as_str().unwrap().contains("chat_ui"),
        "sentence from host preserved",
    );
    assert_or_die(r3["id"] == "chat_ui", "id injected by bundle");
    assert_or_die(r3["kind"] == "proxy_agent", "kind injected by bundle");

    banner("Step 7: unknown agent + no host = graceful_degrade");
    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"orphan"}),
        )
        .await;
    let r4 = kernel
        .send(&AgentId::from("orphan"), json!({"type":"anything"}))
        .await;
    println!("  orphan reply: {r4}");
    assert_or_die(r4["reason"] == "no_host", "orphan has no host");

    banner("Step 8: cascade-delete chat_ui — on_delete fires, host dropped");
    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"delete_agent","id":"chat_ui"}),
        )
        .await;
    assert_or_die(
        chat_host.deletes.load(Ordering::Relaxed) == 1,
        "chat_ui on_delete fired",
    );
    assert_or_die(
        fantastic_proxy_agent::host_for(&chat).is_none(),
        "chat_ui host gone from registry",
    );

    banner("Step 9: settings_ui still works — isolation");
    let r5 = kernel.send(&settings, json!({"type":"ping"})).await;
    assert_or_die(r5["pong"] == true, "settings_ui untouched");

    banner("ALL ASSERTIONS GREEN — proxy_agent round-trip healthy");
}
