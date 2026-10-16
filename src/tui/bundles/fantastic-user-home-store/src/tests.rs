use std::sync::Arc;

use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

use super::*;

/// A target bundle whose own rule a saved set-up must pass: it needs a `model`.
struct NeedsModel;

#[async_trait]
impl Bundle for NeedsModel {
    fn name(&self) -> &str {
        "demo_backend.tools"
    }
    fn validate_meta(&self, meta: &Map<String, Value>, _record: &Map<String, Value>) -> Option<String> {
        (!meta.contains_key("model")).then(|| "model is required".to_string())
    }
    async fn handle(&self, _a: &AgentId, _p: &Value, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Ok(None)
    }
}

fn agent(id: &str, hm: Option<&str>, parent: Option<&str>, meta: Value) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        hm.map(str::to_string),
        parent.map(AgentId::from),
        meta.as_object().cloned().unwrap_or_default(),
        std::path::PathBuf::new(),
        false,
    )
}

fn kernel(home: &Path) -> Arc<Kernel> {
    let mut k = Kernel::new();
    k.bundles.register(HANDLER_MODULE, UserHomeStore::new(Ok(home.to_path_buf())));
    k.bundles.register("demo_backend.tools", NeedsModel);
    let k = Arc::new(k);
    let root = agent("kernel_state", None, None, json!({"guid": "g1"}));
    k.register(Arc::clone(&root));
    k.set_root(root);
    for a in ["user", "ai"] {
        k.register(agent(a, None, Some("kernel_state"), json!({"can_call": {"home": "*"}})));
    }
    k.register(agent(
        "home",
        Some(HANDLER_MODULE),
        Some("kernel_state"),
        json!({"callable_by": {"user": "*"}}),
    ));
    k
}

async fn call(k: &Arc<Kernel>, from: &str, p: Value) -> Value {
    let item = Item::new(AgentId::from("home"), p);
    as_agent(AgentId::from(from), k.send(vec![item])).await.remove(0).result
}

fn err(v: &Value) -> &str {
    v.get("error").and_then(Value::as_str).unwrap_or("")
}

#[tokio::test]
async fn a_saved_setup_saves_lists_and_forgets_as_a_private_file() {
    let td = tempfile::tempdir().unwrap();
    let home = td.path().join("home"); // created by the store, 0700
    let k = kernel(&home);
    let rec = json!({"handler_module": "demo_backend.tools", "model": "m-1", "endpoint": "http://127.0.0.1:9"});
    let saved = call(&k, "user", json!({"type": "persist_record", "name": "opus", "record": rec})).await;
    assert!(saved.get("version").is_some(), "{saved}");
    let file = home.join("agents/ai_agent/saved/opus.yaml");
    assert_eq!(crate::sys::mode_of(&file).unwrap(), 0o600);
    assert_eq!(crate::sys::mode_of(&home.join("agents/ai_agent/saved")).unwrap(), 0o700);
    let tree = call(&k, "user", json!({"type": "load_tree"})).await;
    assert_eq!(tree["saved"][0]["name"], "opus");
    assert_eq!(tree["saved"][0]["record"]["model"], "m-1");
    assert_eq!(tree["preferred"], Value::Null, "nothing is a default until the owner says so");
    let gone = call(&k, "user", json!({"type": "forget_record", "name": "opus"})).await;
    assert_eq!(gone["forgotten"], true, "{gone}");
    assert!(!file.exists());
}

#[tokio::test]
async fn a_saved_setup_carries_no_identity_wiring_or_grants_and_passes_its_bundle() {
    let td = tempfile::tempdir().unwrap();
    let home = td.path().join("home"); // created by the store, 0700
    let k = kernel(&home);
    for (field, v) in [("id", json!("x")), ("memory_store_id", json!("memory")), ("can_call", json!({"exec": "*"}))] {
        let mut rec = json!({"handler_module": "demo_backend.tools", "model": "m"});
        rec[field] = v;
        let r = call(&k, "user", json!({"type": "persist_record", "name": "t", "record": rec})).await;
        assert!(err(&r).contains(field), "{field}: {r}");
    }
    let r = call(&k, "user", json!({"type": "persist_record", "name": "t", "record": {"handler_module": "demo_backend.tools"}})).await;
    assert!(err(&r).contains("model is required"), "the target bundle's own rule: {r}");
    let r = call(&k, "user", json!({"type": "persist_record", "name": "t", "record": {"handler_module": "nope.tools", "model": "m"}})).await;
    assert!(err(&r).contains("no bundle"), "{r}");
    let r = call(&k, "user", json!({"type": "persist_record", "name": "../x", "record": {"handler_module": "demo_backend.tools", "model": "m"}})).await;
    assert_eq!(r["reason"], "bad_arg", "a name is never a path: {r}");
}

#[tokio::test]
async fn the_grants_decide_who_is_served() {
    let td = tempfile::tempdir().unwrap();
    let home = td.path().join("home"); // created by the store, 0700
    let k = kernel(&home);
    // a read creates nothing on the user's disk
    assert!(call(&k, "user", json!({"type": "load_tree"})).await["saved"].as_array().unwrap().is_empty());
    assert!(!home.exists(), "listing an empty store must not create the home dir");
    // `home`'s record does not admit `ai`: the kernel refuses it.
    let r = call(&k, "ai", json!({"type": "load_tree"})).await;
    assert!(!err(&r).is_empty(), "{r}");
    assert!(call(&k, "user", json!({"type": "load_tree"})).await.get("saved").is_some());
}

#[tokio::test]
async fn a_stale_save_is_refused_and_a_loose_file_is_refused_naming_the_chmod() {
    let td = tempfile::tempdir().unwrap();
    let home = td.path().join("home"); // created by the store, 0700
    let k = kernel(&home);
    let rec = json!({"handler_module": "demo_backend.tools", "model": "m-1"});
    let v1 = call(&k, "user", json!({"type": "persist_record", "name": "opus", "record": rec})).await["version"].clone();
    // another fantastic saves in between
    call(&k, "user", json!({"type": "persist_record", "name": "opus", "record": {"handler_module": "demo_backend.tools", "model": "m-2"}})).await;
    let r = call(&k, "user", json!({"type": "persist_record", "name": "opus", "record": rec, "if_match": v1})).await;
    assert_eq!(r["arg"], "if_match", "a lost update is refused: {r}");
    let file = saved_dir(&home).join("opus.yaml");
    crate::sys::set_mode(&file, 0o644).unwrap();
    let r = call(&k, "user", json!({"type": "load_tree"})).await;
    // `chmod 600 <file>` (unix)
    assert_eq!(r["hint"], json!(crate::sys::private_fix(&file, false)), "{r}");
}

#[tokio::test]
async fn the_default_is_explicit_and_forgetting_its_saved_setup_clears_it() {
    let td = tempfile::tempdir().unwrap();
    let home = td.path().join("home"); // created by the store, 0700
    let k = kernel(&home);
    let r = call(&k, "user", json!({"type": "prefer", "name": "opus"})).await;
    assert_eq!(r["reason"], "not_found", "no default for a saved set-up that does not exist: {r}");
    call(&k, "user", json!({"type": "persist_record", "name": "opus", "record": {"handler_module": "demo_backend.tools", "model": "m"}})).await;
    assert_eq!(call(&k, "user", json!({"type": "prefer", "name": "opus"})).await["preferred"], "opus");
    assert_eq!(call(&k, "user", json!({"type": "load_tree"})).await["preferred"], "opus");
    call(&k, "user", json!({"type": "forget_record", "name": "opus"})).await;
    assert_eq!(call(&k, "user", json!({"type": "load_tree"})).await["preferred"], Value::Null);
}
