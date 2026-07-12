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
async fn root_reflect_carries_a_32hex_guid() {
    let kernel = Arc::new(Kernel::new());
    let mut meta = Map::new();
    meta.insert(
        "guid".to_string(),
        json!("0123456789abcdef0123456789abcdef"),
    );
    let root = mk_agent_meta("core", None, meta);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"}));
    let guid = v["guid"].as_str().expect("root reflect has a guid");
    assert_eq!(guid.len(), 32);
    assert!(guid
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
}

#[tokio::test]
async fn child_reflect_has_no_guid() {
    // guid is persistence-scoped + root-only. A child record never carries a
    // `guid` in its meta (only the root record does), so a child reflect must
    // not surface one — `apply_reflect_flags` only injects it on the root.
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("core")));
    let _rx2 = kernel.register(Arc::clone(&child));
    let v = bare_reflect(&kernel, &child, &json!({"type": "reflect"}));
    assert!(v.get("guid").is_none(), "guid must be root-only");
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

// ─── keyed / tiered doc tree ───────────────────────────────────

#[test]
fn est_tokens_matches_ceil_div_and_desc_prefers_quote() {
    assert_eq!(est_tokens(&"x".repeat(400)), 100); // 400/4
    assert_eq!(est_tokens(&"x".repeat(401)), 101); // ceil
    assert_eq!(est_tokens(""), 0);
    assert_eq!(read_desc("> the desc\n\n# Title\nbody"), "the desc");
    assert_eq!(read_desc("# Title\n\nfirst prose line"), "first prose line");
    assert_eq!(read_desc("# only a heading"), "");
}

/// Seed a minimal orientation + docs tree (with one nested tier) at `<base>`.
fn seed_docs(base: &Path) {
    let docs = base.join("docs");
    std::fs::create_dir_all(docs.join("frontend")).unwrap();
    std::fs::write(
        base.join("readme.md"),
        "# This is a Fantastic kernel.\n\nkeyed doc tree — reflect readme=index",
    )
    .unwrap();
    std::fs::write(
        docs.join("self-extension.md"),
        "> author your own tools\n\n# Self-extension\nauthor durable tools here",
    )
    .unwrap();
    std::fs::write(
        docs.join("streams.md"),
        "> byte streams\n\n# Streams\nchunked bytes",
    )
    .unwrap();
    std::fs::write(
        docs.join("frontend.md"),
        "> two kernels\n\n# Frontend\nhost + browser",
    )
    .unwrap();
    std::fs::write(
        docs.join("frontend").join("routing.md"),
        "> routing rule\n\n# Routing\nonly the js kernel",
    )
    .unwrap();
}

#[tokio::test]
async fn readme_doctree_frame_index_view_and_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".fantastic");
    seed_docs(&base);
    let kernel = Arc::new(Kernel::new());
    let root = Agent::new(AgentId::from("core"), None, None, Map::new(), base, true);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // Always-on frame: `readmes` index + `docs` note on the root reflect.
    let plain = bare_reflect(&kernel, &root, &json!({"type":"reflect","tree":"none"}));
    let readmes = plain["readmes"].as_array().unwrap();
    assert!(readmes.iter().any(|e| e["key"] == "self-extension"));
    assert!(plain["docs"].as_str().unwrap().contains("readme=index"));

    // readme=true → the SHORT orientation string.
    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":true}),
    );
    assert!(r["readme"].as_str().unwrap().contains("Fantastic kernel"));

    // readme=index → token-annotated tree; nesting preserved; NO body.
    let idx = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"index"}),
    );
    let arr = idx["readme"]["index"].as_array().unwrap();
    for e in arr {
        assert!(e.get("body").is_none());
        assert!(e["tokens"].as_u64().unwrap() > 0);
    }
    let fe = arr.iter().find(|e| e["key"] == "frontend").unwrap();
    assert_eq!(fe["children"][0]["key"], "frontend/routing");

    // readme=<key> → body + (leaf) no children.
    let doc = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"self-extension"}),
    );
    assert_eq!(doc["readme"]["key"], "self-extension");
    assert!(doc["readme"]["body"].as_str().unwrap().contains("author"));

    // A nested key is reachable by its slashed path.
    let nested = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"frontend/routing"}),
    );
    assert_eq!(nested["readme"]["key"], "frontend/routing");

    // Unknown key → {error, keys}; a `..` escape is refused.
    let miss = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"nope"}),
    );
    assert!(miss["readme"]["error"].is_string());
    assert!(miss["readme"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k == "self-extension"));
    let esc = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"../readme"}),
    );
    assert!(esc["readme"]["error"].is_string());
}

#[tokio::test]
async fn non_root_has_no_readmes_frame() {
    let kernel = Arc::new(Kernel::new());
    let child = mk_agent("kid", Some(AgentId::from("core")));
    let _rx = kernel.register(Arc::clone(&child));
    let r = bare_reflect(&kernel, &child, &json!({"type":"reflect","tree":"none"}));
    assert!(r.get("readmes").is_none());
    assert!(r.get("docs").is_none());
}
