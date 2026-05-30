//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::{Agent, BundleRegistry};
use serde_json::Map;
use tempfile::TempDir;

fn mk_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, TelemetryPaneBundle);
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
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("telemetry_pane — live agent-vis GL view"));
}

#[tokio::test]
async fn get_gl_view_returns_embedded_source() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"t1"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("t1"), json!({"type": "get_gl_view"}))
        .await;
    assert_eq!(r["title"], "telemetry");
    let src = r["source"].as_str().expect("source is a string");
    // Pin a couple of load-bearing fragments of the embedded JS body
    // so accidental drift (truncated copy, wrong file) is loud.
    assert!(!src.is_empty());
    assert!(src.contains("THREE"));
    // The render path is a pure consumer of the substrate — drift
    // guard for the no-kernel-calls invariant.
    assert!(
        !src.contains("t.call(") && !src.contains("t.send(") && !src.contains("t.emit("),
        "telemetry source must not call kernel verbs from the render path"
    );
}

#[tokio::test]
async fn reflect_returns_sentence() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"t2"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("t2"), json!({"type": "reflect"}))
        .await;
    assert_eq!(r["id"], "t2");
    assert!(r["sentence"]
        .as_str()
        .unwrap()
        .contains("Live agent visualization"));
    let verbs = r["verbs"].as_object().expect("verbs is an object");
    assert!(verbs.contains_key("get_gl_view"));
    assert!(verbs.contains_key("reflect"));
}

// Silence the unused-import lint when the registry is constructed.
#[allow(dead_code)]
fn _registry_compile_check() {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, TelemetryPaneBundle);
}
