//! Unit tests for [`crate::reflect`].

use super::*;
use crate::agent::Agent;
use serde_json::json;
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

fn mk_agent_meta(id: &str, parent: Option<AgentId>, meta: Map<String, Value>) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        None,
        parent,
        meta,
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

/// Mirror what `send()` does for a bare agent: identity + flags.
fn bare_reflect(kernel: &Arc<Kernel>, agent: &Arc<Agent>, payload: &Value) -> Value {
    let mut r = reflect_identity(agent);
    apply_reflect_flags(kernel, agent, payload, &mut r);
    r
}

const PRIMER_KEYS_GONE: [&str; 9] = [
    "transports",
    "primitive",
    "envelope",
    "universal_verb",
    "binary_protocol",
    "browser_bus",
    "well_known",
    "agent_count",
    "available_bundles",
];

#[test]
fn tree_node_includes_id_and_empty_children() {
    let a = mk_agent("leaf", Some(AgentId::from("p")));
    let v = tree_node(&a);
    assert_eq!(v["id"], "leaf");
    assert_eq!(v["parent_id"], "p");
    assert_eq!(v["children"], json!([]));
}

#[tokio::test]
async fn root_reflect_is_uniform_no_primer_keys() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"}));
    assert_eq!(v["id"], "core");
    assert!(v["sentence"]
        .as_str()
        .unwrap()
        .starts_with("Fantastic kernel"));
    assert_eq!(v["parent_id"], Value::Null);
    // tree default = all.
    assert_eq!(v["tree"]["id"], "core");
    // bundles omitted by default.
    assert!(v.get("bundles").is_none());
    // kernel runtime identity + deployment context — root only.
    assert_eq!(v["runtime"], "rust");
    // No FANTASTIC_ENV / FANTASTIC_VERSION in the test process → host defaults.
    assert_eq!(v["env"], "host");
    assert!(v["version"].is_null());
    for k in PRIMER_KEYS_GONE {
        assert!(v.get(k).is_none(), "deleted primer key {k} still present");
    }
}

#[tokio::test]
async fn child_reflect_is_uniform() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("core")));
    let _rx2 = kernel.register(Arc::clone(&child));
    let v = bare_reflect(&kernel, &child, &json!({"type": "reflect"}));
    assert_eq!(v["id"], "kid_1");
    assert_eq!(v["parent_id"], "core");
    assert_eq!(v["tree"]["id"], "kid_1");
    // runtime + env/version are root-only — a child must not carry them.
    assert!(v.get("runtime").is_none());
    assert!(v.get("env").is_none());
    assert!(v.get("version").is_none());
    for k in PRIMER_KEYS_GONE {
        assert!(v.get(k).is_none());
    }
}

#[tokio::test]
async fn tree_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("core")));
    let _rx2 = kernel.register(Arc::clone(&child));
    root.children.insert(child.id.clone(), Arc::clone(&child));
    // tree=ids → flat list, self first.
    let ids = bare_reflect(&kernel, &root, &json!({"type": "reflect", "tree": "ids"}));
    assert_eq!(ids["tree"], json!(["core", "kid_1"]));
    // tree=none → omitted.
    let none = bare_reflect(&kernel, &root, &json!({"type": "reflect", "tree": "none"}));
    assert!(none.get("tree").is_none());
}

#[tokio::test]
async fn bundles_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // No bundles registered in this bare kernel → empty list (not absent).
    let all = bare_reflect(
        &kernel,
        &root,
        &json!({"type": "reflect", "bundles": "all"}),
    );
    assert!(all["bundles"].is_array());
    let ids = bare_reflect(
        &kernel,
        &root,
        &json!({"type": "reflect", "bundles": "ids"}),
    );
    assert!(ids["bundles"].is_array());
}

#[tokio::test]
async fn description_surfaces_top_and_tree() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut meta = Map::new();
    meta.insert("description".to_string(), json!("holds my notes"));
    let child = mk_agent_meta("kid_1", Some(AgentId::from("core")), meta);
    let _rx2 = kernel.register(Arc::clone(&child));
    root.children.insert(child.id.clone(), Arc::clone(&child));
    // top-level on the child's own reflect.
    let own = bare_reflect(&kernel, &child, &json!({"type": "reflect"}));
    assert_eq!(own["description"], "holds my notes");
    // and in the parent's tree=all node.
    let root_v = bare_reflect(&kernel, &root, &json!({"type": "reflect"}));
    let node = root_v["tree"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "kid_1")
        .unwrap();
    assert_eq!(node["description"], "holds my notes");
}

#[tokio::test]
async fn readme_flag_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // default → no readme key.
    let plain = bare_reflect(&kernel, &root, &json!({"type": "reflect"}));
    assert!(plain.get("readme").is_none());
    // readme=true with no file on disk → key present, value null.
    let r = bare_reflect(&kernel, &root, &json!({"type": "reflect", "readme": true}));
    assert!(r.get("readme").is_some());
    assert_eq!(r["readme"], Value::Null);
}
