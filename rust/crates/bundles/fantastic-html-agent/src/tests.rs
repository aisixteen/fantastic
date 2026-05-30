//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::{Agent, BundleRegistry};
use serde_json::Map;
use std::sync::Arc;
use tempfile::TempDir;

fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, Arc<Agent>) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, HtmlAgentBundle);
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
    assert!(README.contains("html_agent — UI as a record"));
}

#[tokio::test]
async fn render_html_returns_stub_when_no_body() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    // Create an html_agent.
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"h1"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("h1"), json!({"type": "render_html"}))
        .await;
    let html = r["html"].as_str().unwrap();
    assert!(html.contains("html_agent"));
}

#[tokio::test]
async fn set_html_writes_and_render_returns_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"h2"}),
        )
        .await;
    let r = kernel
        .send(
            &AgentId::from("h2"),
            json!({"type": "set_html", "html": "<h1>hello</h1>"}),
        )
        .await;
    assert_eq!(r["id"], "h2");
    assert_eq!(r["html_bytes"], 14);
    let read = kernel
        .send(&AgentId::from("h2"), json!({"type": "render_html"}))
        .await;
    assert_eq!(read["html"], "<h1>hello</h1>");
}

#[tokio::test]
async fn boot_migrates_html_field_into_index_html() {
    // create_agent with `html` on the payload puts it on the record's
    // meta. boot must move it into index.html + strip from meta.
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"h3","html":"<p>seed</p>"}),
        )
        .await;
    // `create_agent` auto-fires boot (Python parity — see
    // lifecycle::create_from_payload), so by this point boot has
    // already migrated the `html` meta field into `index.html` and
    // stripped it from the record. Verify the migration happened in
    // the single combined create+boot pass.
    let agent_dir = tmp.path().join(".fantastic/agents/h3");
    let body = std::fs::read_to_string(agent_dir.join("index.html"))
        .expect("create_agent should auto-fire boot which writes index.html");
    assert_eq!(body, "<p>seed</p>");
    // Idempotent re-boot — should be a no-op (the field's already
    // stripped from meta + the file's already written).
    let _ = kernel
        .send(&AgentId::from("h3"), json!({"type": "boot"}))
        .await;
    // render_html now returns the body.
    let r = kernel
        .send(&AgentId::from("h3"), json!({"type": "render_html"}))
        .await;
    assert_eq!(r["html"], "<p>seed</p>");
}

#[tokio::test]
async fn reflect_reports_has_html_flag() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _root) = mk_kernel(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"h4"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("h4"), json!({"type": "reflect"}))
        .await;
    assert_eq!(r["has_html"], false);
    // Set HTML.
    kernel
        .send(
            &AgentId::from("h4"),
            json!({"type": "set_html", "html": "<x/>"}),
        )
        .await;
    let r2 = kernel
        .send(&AgentId::from("h4"), json!({"type": "reflect"}))
        .await;
    assert_eq!(r2["has_html"], true);
}

// Silence the unused-import lint when reg is constructed.
#[allow(dead_code)]
fn _registry_compile_check() {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, HtmlAgentBundle);
}
