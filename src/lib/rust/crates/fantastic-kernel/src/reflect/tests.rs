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

// ─── self_extension: the execution model + the DERIVED executor list ───
//
// Two facts the root pointer owns, both of them paid for by a failed run:
//
// 1. The EXECUTION MODEL. Measured twice on gemma-4-12B (2026-08-06): "the
//    `code` IS the tool" reads as "the code DEFINES the tool", so the model
//    authored `def run(payload): ...` and sent {type:'run'} expecting a call.
//    Two agents shipped inert (one was valid Python), nothing was written, and
//    {queued:true} never appeared because no job stayed alive.
// 2. The EXECUTOR'S NAME. Measured on python (2026-08-05): opus-5 invented
//    `handler_module:'local_runner.script'` because the real module was never
//    named, and built nothing. An affordance you cannot NAME is not
//    discoverable — and a module named but not registered is worse than silence.

/// A do-nothing bundle, registered only so `available_bundles` reports its
/// `handler_module`. The executor advertisement is derived from the REGISTRY, so
/// a test of that derivation must install real registry entries.
struct StubBundle(&'static str);

#[async_trait::async_trait]
impl crate::bundle::Bundle for StubBundle {
    fn name(&self) -> &str {
        self.0
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
}

/// A kernel with `handler_module`s registered, its root set, and one reflect
/// taken — returns the root reflect's `self_extension`.
async fn self_extension_with(modules: &[&'static str]) -> String {
    let mut kernel = Kernel::new();
    for m in modules {
        kernel.bundles.register(m, StubBundle(m));
    }
    let kernel = Arc::new(kernel);
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"}));
    v["self_extension"]
        .as_str()
        .expect("the root reflect must carry a self_extension pointer")
        .to_string()
}

/// Every `<word>.tools` token in `s` — the shape a `handler_module` wears.
fn handler_module_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .map(|t| t.trim_end_matches('.'))
        .filter(|t| t.ends_with(".tools"))
        .map(str::to_string)
        .collect()
}

/// Mirrors python's `test_distilled_default_keeps_self_extension_ACTIONABLE`
/// in spirit: the pointer must teach the EXECUTION MODEL, not only that
/// self-extension exists. `def run(): ...` is what a model writes when told the
/// code "is the tool".
#[tokio::test]
async fn self_extension_states_the_script_execution_model() {
    let se = self_extension_with(&["exec_bridge.tools"]).await;
    let low = se.to_lowercase();
    assert!(
        low.contains("script"),
        "must say the code is a SCRIPT: {se}"
    );
    assert!(low.contains("top to bottom"), "must say HOW it runs: {se}");
    assert!(
        se.contains("`def`"),
        "must warn that a bare `def` does nothing: {se}"
    );
    assert!(
        se.contains("module level"),
        "must say where the work goes: {se}"
    );
    assert!(
        se.contains("create_agent"),
        "must show how to author, not just that you may: {se}"
    );
}

/// The advertised executor set IS the registry, intersected with [`EXECUTORS`].
/// Hand-writing the list is what once let a registered executor go unmentioned while
/// it was registered — so this asserts the mapping in both directions.
#[tokio::test]
async fn self_extension_names_only_registered_executors() {
    let mut kernel = Kernel::new();
    for m in [
        "exec_bridge.tools",
        // A NON-executor bundle: registered, and must never be advertised as a
        // place authored `code` can run.
        "yaml_state.tools",
    ] {
        kernel.bundles.register(m, StubBundle(m));
    }
    let kernel = Arc::new(kernel);
    let root = mk_agent("core", None);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"}));
    let se = v["self_extension"].as_str().unwrap();

    // Derived expectation — the same introspection the pointer renders from.
    let installed: Vec<String> = available_bundles(&kernel)
        .iter()
        .filter_map(|b| b.get("handler_module").and_then(Value::as_str))
        .map(str::to_string)
        .collect();

    // ① every module NAMED in the string is actually registered.
    for named in handler_module_tokens(se) {
        assert!(
            installed.contains(&named),
            "self_extension names {named:?}, which this build does not register: {se}"
        );
    }
    // ② every registered executor IS named — `exec_bridge.tools`, the ONE
    //    executor this kernel ships (the per-language runtimes are wiped).
    for (module, _) in EXECUTORS {
        assert!(
            se.contains(module),
            "{module} is registered but self_extension never names it: {se}"
        );
    }
    assert!(
        se.contains("exec_bridge.tools"),
        "the registered executor must be named: {se}"
    );
    // ③ a non-executor bundle is not an executor.
    assert!(
        !se.contains("yaml_state.tools"),
        "yaml_state is not a script executor: {se}"
    );
    // ④ the named executor carries its RECEIVE SHAPE — a python program
    //    registers a callback. Mis-stating it costs a run.
    assert!(se.contains("kernel.on_message(cb)"));
}

/// Empty intersection → say the build ships none. Naming an absent module is
/// the exact failure the derivation exists to prevent.
#[tokio::test]
async fn self_extension_names_no_executor_when_none_is_registered() {
    let se = self_extension_with(&[]).await;
    assert!(
        handler_module_tokens(&se).is_empty(),
        "a build with no executor must name no module: {se}"
    );
    assert!(
        se.contains("NO script executor"),
        "must say the build ships none: {se}"
    );
}

/// A registered executor that is NOT in the table changes nothing, and a
/// table entry that is not registered is never advertised.
#[tokio::test]
async fn self_extension_advertises_the_intersection_only() {
    // A registered bundle NOT in the table (yaml_state) adds nothing, and the
    // table's one entry only shows when its module is actually registered.
    let se = self_extension_with(&["exec_bridge.tools"]).await;
    assert_eq!(handler_module_tokens(&se), vec!["exec_bridge.tools"]);
    assert!(!se.contains("yaml_state.tools"));
}

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
    assert!(plain["docs"].as_str().unwrap().contains("keyed doc tree"));

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
