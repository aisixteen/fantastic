//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use tempfile::TempDir;

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("terminal_webapp"));
}

#[test]
fn terminal_html_embedded() {
    // Pin a few load-bearing fragments of the embedded xterm page.
    // Mostly an early-warning system for accidental drift.
    assert!(!TERMINAL_HTML.is_empty());
    assert!(TERMINAL_HTML.contains("<!DOCTYPE html>"));
    assert!(TERMINAL_HTML.contains("xterm"));
}

#[tokio::test]
async fn render_html_returns_embedded_page() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, TerminalWebappBundle);
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
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"tw1"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("tw1"), json!({"type": "render_html"}))
        .await;
    assert!(r["html"].as_str().unwrap().contains("<!DOCTYPE html>"));
}

#[tokio::test]
async fn get_webapp_returns_url() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, TerminalWebappBundle);
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
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"tw2"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("tw2"), json!({"type": "get_webapp"}))
        .await;
    assert_eq!(r["url"], "/tw2/");
    assert_eq!(r["title"], "xterm");
    assert_eq!(r["default_width"], 600);
    assert_eq!(r["default_height"], 400);
    // Header chip contract — the iframe wires this to its autoscroll
    // toggle via the browser bus.
    let buttons = r["header_buttons"]
        .as_array()
        .expect("header_buttons array");
    assert_eq!(buttons.len(), 1);
    assert_eq!(buttons[0]["id"], "autoscroll");
    assert_eq!(buttons[0]["toggle"], true);
}

#[tokio::test]
async fn reflect_includes_upstream_id() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, TerminalWebappBundle);
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
    // Pre-create the upstream backend the webapp will be bound to.
    // Without this, the webapp's auto-fired boot would refuse to
    // recognise the stale `upstream_id` and auto-pair a fresh
    // backend, replacing it (matches Python parity — see
    // terminal_webapp::boot_reply). The realistic test is: bound to
    // an EXISTING backend, boot is a no-op, upstream_id survives.
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type":"create_agent",
                "handler_module":"terminal_backend.tools",
                "id":"tb3",
            }),
        )
        .await;
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"tw3","upstream_id":"tb3"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("tw3"), json!({"type": "reflect"}))
        .await;
    assert_eq!(r["id"], "tw3");
    assert_eq!(r["upstream_id"], "tb3");
    assert!(r["verbs"].is_object());
    assert!(r["verbs"]["render_html"].is_string());
}
