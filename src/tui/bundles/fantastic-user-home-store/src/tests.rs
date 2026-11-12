use std::sync::Arc;

use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};

use super::*;

/// What a saved set-up for the stand-in backend may carry: it needs a `model`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct NeedsModelRecord {
    /// The model to use.
    pub model: String,
    /// Where the backend listens.
    pub endpoint: Option<String>,
}

fantastic_kernel::record!(NeedsModelRecord);

/// A target bundle whose declared record a saved set-up must pass: it needs a `model`.
struct NeedsModel;

#[async_trait]
impl Handler for NeedsModel {
    type Call = fantastic_kernel::test_support::NoCall;
    fn name(&self) -> &str {
        "demo_backend.tools"
    }
    fn man(&self) -> &'static str {
        "A test stand-in backend."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        fantastic_kernel::test_support::no_verbs()
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        fantastic_kernel::test_support::no_dynamic(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &RECORD
    }
    async fn handle(&self, _a: &AgentId, _c: Self::Call, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
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
    let file = home.join("user/setups/ai_agent/opus.yaml");
    assert_eq!(crate::sys::mode_of(&file).unwrap(), 0o600);
    for d in ["user", "user/setups", "user/setups/ai_agent"] {
        assert_eq!(crate::sys::mode_of(&home.join(d)).unwrap(), 0o700, "{d}");
    }
    assert!(!home.join("agents").exists(), "nothing at the old place");
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
    assert!(err(&r).contains("needs field 'model'"), "the target bundle's declared record: {r}");
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

#[tokio::test]
async fn every_verb_is_checked_by_its_declaration() {
    let td = tempfile::tempdir().unwrap();
    let k = kernel(&td.path().join("home"));
    let r = call(&k, "user", json!({"type": "zzz_nope"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    for v in verbs::VERBS.iter().map(|v| v.name).chain(["reflect"]) {
        assert!(err(&r).contains(v), "{v} in {r}");
    }
    for v in verbs::VERBS {
        let r = call(&k, "user", json!({"type": v.name, "zzz_unknown": 1})).await;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("zzz_unknown")), "{}: {r}", v.name);
    }
    for (verb, arg) in [("persist_record", "name"), ("forget_record", "name")] {
        let r = call(&k, "user", json!({"type": verb})).await;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("missing_arg"), Some(arg)), "{verb}: {r}");
    }
    let r = call(&k, "user", json!({"type": "persist_record", "name": "x"})).await;
    assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("missing_arg"), Some("record")), "{r}");
    let r = call(&k, "user", json!({"type": "persist_record", "name": "x", "record": {}})).await;
    assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("missing_arg"), Some("record.handler_module")), "{r}");
    for (p, arg) in [
        (json!({"type": "persist_record", "name": "../x", "record": {"handler_module": "demo_backend.tools"}}), "name"),
        (json!({"type": "persist_record", "name": "x", "record": [], "if_match": 1}), "record"),
        (json!({"type": "forget_record", "name": 5}), "name"),
        (json!({"type": "prefer", "name": "a b"}), "name"),
        (json!({"type": "prefer", "name": 5}), "name"),
    ] {
        let r = call(&k, "user", p.clone()).await;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some(arg)), "{p}: {r}");
    }
    // a saved set-up's fields are the target bundle's declared ones: an unknown field is refused by name
    let r = call(&k, "user", json!({"type": "persist_record", "name": "x", "record": {"handler_module": "demo_backend.tools", "model": "m", "stray": 1}})).await;
    assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("stray")), "{r}");
    let r = call(&k, "user", json!({"type": "persist_record", "name": "x", "record": {"handler_module": "demo_backend.tools", "model": 3}})).await;
    assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("model")), "{r}");
    // reflect renders the declared verbs
    let r = call(&k, "user", json!({"type": "reflect"})).await;
    assert_eq!(r["handler_module"], HANDLER_MODULE, "{r}");
}
