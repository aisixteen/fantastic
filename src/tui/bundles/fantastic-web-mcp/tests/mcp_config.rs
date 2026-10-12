//! THE LEG KEEPS `.mcp.json` RIGHT — through a real file_bridge, as the leg.
//!
//! No `file_bridge_id` → nothing happens. Set → the config is read through the
//! bridge, `mcpServers.fantastic` made `{command: <this binary>, args: ["mcp"]}`,
//! written back only when it changed, every other key kept; anything that fails is
//! loud (the boot's `{error, reason: "mcp_config"}`, reflect `mcp_config_error`).

use std::sync::Arc;

use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_web_mcp::{server_entry, with_entry, WebMcpBundle, HANDLER_MODULE};
use serde_json::{json, Value};

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    let item = Item::new(AgentId::from(target), payload);
    Arc::clone(k).send(vec![item]).await.remove(0).result
}

const VERBS: [&str; 2] = ["read_stream", "write"];

/// A kernel running in `dir`, a bridge `cfg` over it exposing `.mcp.json`
/// (`grant`: whether it admits the leg), and the leg `mcp` with `meta`.
async fn setup(dir: &std::path::Path, grant: bool, meta: Value) -> (Arc<Kernel>, Value) {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, WebMcpBundle);
    reg.register(fantastic_file::HANDLER_MODULE, fantastic_file::FileBundle::default());
    reg.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
    let k = Arc::clone(&bootstrap(reg, BootstrapOptions::one_shot(dir)).expect("boots").kernel);
    let root = k.root().expect("root").id.0.clone();
    let cb = if grant { json!({"$self": "*", "mcp": VERBS}) } else { json!({"$self": "*"}) };
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "cfg",
        "handler_module": fantastic_file::HANDLER_MODULE, "allow": [".mcp.json"],
        "ingress_rule": "allow_all_ingress", "callable_by": cb})).await;
    assert!(r.get("error").is_none(), "create cfg: {r}");
    let mut create = json!({"type": "create_agent", "parent_id": root, "id": "mcp",
        "handler_module": HANDLER_MODULE, "can_call": {"$self": "*", "cfg": VERBS}});
    for (key, v) in meta.as_object().cloned().unwrap_or_default() {
        create[key] = v;
    }
    let r = op(&k, &root, create).await;
    assert!(r.get("error").is_none(), "create mcp: {r}");
    let boot = op(&k, "mcp", json!({"type": "boot"})).await;
    (k, boot)
}

fn wired() -> Value {
    json!({"file_bridge_id": "cfg", "mcp_config_file": ".mcp.json"})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_config_is_created_with_this_binary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (_k, boot) = setup(tmp.path(), true, wired()).await;
    assert_eq!(boot, Value::Null, "{boot}");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(doc["mcpServers"]["fantastic"], server_entry().unwrap());
    assert_eq!(doc["mcpServers"]["fantastic"]["args"], json!(["mcp"]));
    assert!(std::path::Path::new(doc["mcpServers"]["fantastic"]["command"].as_str().unwrap()).is_absolute());
}

#[tokio::test(flavor = "multi_thread")]
async fn other_servers_and_keys_are_kept_and_a_stale_entry_is_corrected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let before = r#"{"zeta": 1, "mcpServers": {"other": {"command": "x", "args": ["y"]}, "fantastic": {"command": "/old/fantastic", "args": ["mcp"]}}, "alpha": true}"#;
    std::fs::write(tmp.path().join(".mcp.json"), before).unwrap();
    let (k, boot) = setup(tmp.path(), true, wired()).await;
    assert_eq!(boot, Value::Null, "{boot}");
    let after = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
    let doc: Value = serde_json::from_str(&after).unwrap();
    assert_eq!(doc["mcpServers"]["other"], json!({"command": "x", "args": ["y"]}));
    assert_eq!(doc["mcpServers"]["fantastic"], server_entry().unwrap());
    let keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["zeta", "mcpServers", "alpha"], "key order kept");
    // right already: the next boot writes nothing
    let mtime = std::fs::metadata(tmp.path().join(".mcp.json")).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(op(&k, "mcp", json!({"type": "boot"})).await, Value::Null);
    assert_eq!(std::fs::metadata(tmp.path().join(".mcp.json")).unwrap().modified().unwrap(), mtime);
    assert!(op(&k, "mcp", json!({"type": "reflect"})).await.get("mcp_config_error").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_file_bridge_id_does_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (k, boot) = setup(tmp.path(), true, json!({"mcp_config_file": ".mcp.json"})).await;
    assert_eq!(boot, Value::Null);
    assert!(!tmp.path().join(".mcp.json").exists());
    assert!(op(&k, "mcp", json!({"type": "reflect"})).await.get("mcp_config_error").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn set_and_unusable_is_loud() {
    // no path on the bridge
    let tmp = tempfile::TempDir::new().unwrap();
    let (k, boot) = setup(tmp.path(), true, json!({"file_bridge_id": "cfg"})).await;
    assert_eq!(boot["reason"], "mcp_config", "{boot}");
    assert!(boot["error"].as_str().unwrap().contains("mcp_config_file"), "{boot}");
    let r = op(&k, "mcp", json!({"type": "reflect"})).await;
    assert!(r["mcp_config_error"].as_str().unwrap().contains("mcp_config_file"), "{r}");

    // the bridge does not admit the leg
    let tmp = tempfile::TempDir::new().unwrap();
    let (_k, boot) = setup(tmp.path(), false, wired()).await;
    assert_eq!(boot["reason"], "mcp_config", "{boot}");
    assert!(boot["error"].as_str().unwrap_or_default().contains("read_stream"), "{boot}");
    assert!(!tmp.path().join(".mcp.json").exists());

    // not a JSON object: never overwritten
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".mcp.json"), "[1, 2]").unwrap();
    let (_k, boot) = setup(tmp.path(), true, wired()).await;
    assert!(boot["error"].as_str().unwrap().contains("not a JSON object"), "{boot}");
    assert_eq!(std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap(), "[1, 2]");
}

#[test]
fn mcp_servers_must_be_an_object() {
    let e = with_entry(r#"{"mcpServers": []}"#, &json!({})).unwrap_err();
    assert!(e.contains("mcpServers"), "{e}");
}
