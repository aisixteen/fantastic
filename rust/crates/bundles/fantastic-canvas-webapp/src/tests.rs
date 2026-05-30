//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use tempfile::TempDir;

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("canvas_webapp"));
}

#[test]
fn canvas_html_embedded() {
    // Pin a few load-bearing fragments of the embedded canvas page.
    // Mostly an early-warning system for accidental drift.
    assert!(!CANVAS_HTML.is_empty());
    assert!(CANVAS_HTML.contains("<!DOCTYPE html>"));
    assert!(CANVAS_HTML.contains("canvas"));
}

#[tokio::test]
async fn render_html_returns_embedded_page() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, CanvasWebappBundle);
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
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"cw1","upstream_id":"cb1"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("cw1"), json!({"type": "render_html"}))
        .await;
    assert!(r["html"].as_str().unwrap().contains("<!DOCTYPE html>"));
}

#[tokio::test]
async fn get_webapp_returns_iframe_url() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, CanvasWebappBundle);
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
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"cw2"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("cw2"), json!({"type": "get_webapp"}))
        .await;
    assert_eq!(r["url"], "/cw2/");
    assert_eq!(r["title"], "canvas");
}
