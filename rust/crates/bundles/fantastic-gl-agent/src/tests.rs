//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::{Agent, BundleRegistry};
use serde_json::Map;
use std::sync::Arc;
use tempfile::TempDir;

fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, Arc<Agent>) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, GlAgentBundle);
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
    (kernel, root)
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("gl_agent — GL-view-as-record"));
}

#[tokio::test]
async fn reflect_reports_has_source_flag() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"g1"}),
        )
        .await;
    // Empty meta → has_source=false.
    let r = kernel
        .send(&AgentId::from("g1"), json!({"type": "reflect"}))
        .await;
    assert_eq!(r["has_source"], false);
    assert_eq!(r["id"], "g1");
    assert!(r["sentence"]
        .as_str()
        .unwrap()
        .contains("GL-view-as-record"));
    // Install source → reflect flips.
    kernel
        .send(
            &AgentId::from("g1"),
            json!({"type": "set_gl_source", "source": "// hi", "title": "T"}),
        )
        .await;
    let r2 = kernel
        .send(&AgentId::from("g1"), json!({"type": "reflect"}))
        .await;
    assert_eq!(r2["has_source"], true);
    assert_eq!(r2["title"], "T");
}

#[tokio::test]
async fn set_gl_source_persists_and_emits() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"g2"}),
        )
        .await;
    // Pre-install a synthetic watcher with our own inbox so we can
    // observe the emit. `kernel.watch` auto-vivifies an inbox only if
    // one isn't already registered — pre-insert ours so try_send lands
    // in a channel whose rx we control.
    let watcher_id = AgentId::from("watch1");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(16);
    kernel.inboxes.insert(watcher_id.clone(), tx);
    kernel.watch(&AgentId::from("g2"), watcher_id.clone()).await;

    let r = kernel
        .send(
            &AgentId::from("g2"),
            json!({"type": "set_gl_source", "source": "// new"}),
        )
        .await;
    assert_eq!(r["ok"], true);
    assert_eq!(r["id"], "g2");
    assert_eq!(r["bytes"], 6);

    // Watcher's inbox receives the RAW payload (Python parity — see
    // Kernel::fanout_to_watchers in fantastic-kernel/src/send.rs).
    // State subscribers above already got the metadata envelope
    // ({type:"emit", sender, target, verb, summary}); watchers see
    // what was emitted, not metadata about it.
    let event = rx.try_recv().expect("watcher should receive the emit");
    assert_eq!(event["type"], "gl_source_changed");

    // Persistence: agent.json holds gl_source.
    let path = tmp.path().join(".fantastic/agents/g2/agent.json");
    let raw = std::fs::read_to_string(&path).expect("agent.json written");
    let rec: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(rec["gl_source"], "// new");
}

#[tokio::test]
async fn get_gl_view_returns_iframe_payload() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"g3","display_name":"DN"}),
        )
        .await;
    kernel
        .send(
            &AgentId::from("g3"),
            json!({"type": "set_gl_source", "source": "// body"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("g3"), json!({"type": "get_gl_view"}))
        .await;
    // Python parity: get_gl_view returns {source, title} only.
    assert_eq!(r["source"], "// body");
    assert_eq!(r["title"], "DN");
}

#[tokio::test]
async fn get_gl_source_returns_stub_when_missing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"g4"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("g4"), json!({"type": "get_gl_source"}))
        .await;
    let src = r["source"].as_str().unwrap();
    assert!(src.contains("no source set"));
}

// Silence the unused-import lint when the registry is constructed.
#[allow(dead_code)]
fn _registry_compile_check() {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, GlAgentBundle);
}
