//! Unit tests for [`crate::reflect`].

use super::*;
use crate::agent::Agent;
use std::path::Path;

fn mk_agent(id: &str, parent: Option<AgentId>) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        None,
        parent,
        Map::new(),
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

#[test]
fn tree_node_includes_id_and_empty_children() {
    let a = mk_agent("leaf", Some(AgentId::from("p")));
    let v = tree_node(&a);
    assert_eq!(v["id"], "leaf");
    assert_eq!(v["parent_id"], "p");
    assert_eq!(v["children"], json!([]));
}

#[tokio::test]
async fn root_reflect_returns_primer_shape() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = reflect(&kernel, &root, &json!({"type": "reflect"}));
    assert!(v.is_object());
    // Required primer keys.
    for k in [
        "sentence",
        "primitive",
        "envelope",
        "universal_verb",
        "transports",
        "tree",
        "available_bundles",
        "agent_count",
        "binary_protocol",
        "browser_bus",
    ] {
        assert!(v.get(k).is_some(), "missing primer key {k}");
    }
    assert_eq!(v["tree"]["id"], "core");
    assert_eq!(v["agent_count"], 1);
}

#[tokio::test]
async fn child_reflect_returns_node_summary() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("core")));
    let _rx2 = kernel.register(Arc::clone(&child));
    let v = reflect(&kernel, &child, &json!({"type": "reflect"}));
    assert_eq!(v["id"], "kid_1");
    assert_eq!(v["parent_id"], "core");
    // No primer keys on per-agent summaries.
    assert!(v.get("transports").is_none());
    assert!(v.get("agent_count").is_none());
}
