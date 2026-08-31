//! Unit tests for the `durable_memory` agent — modeless CRUD + append, persisted
//! THROUGH a wired `file_bridge` over the RAW stream channel (`read_stream` /
//! `write_stream`), referenced by `file_bridge_id`. This bundle owns no `std::fs`
//! surface; `set`/`append`/`delete` failfast until a leg is wired.

use super::*;
use fantastic_file::FileBundle;
use serde_json::json;
use std::path::Path;

/// A kernel with a real, OPEN `file_bridge` store rooted at `store_root`, plus a
/// `durable_memory` agent bound to it via `file_bridge_id`. State lands at
/// `store_root/state.yaml`.
async fn mk_agent(store_root: &Path) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register("file_bridge.tools", FileBundle);
    kernel.bundles.register(HANDLER_MODULE, DurableMemoryBundle);
    let kernel = Arc::new(kernel);
    // Root — its parent is the running dir the file_bridge clamps to, so a store
    // rooted at `store_root` must have `store_root` as that parent.
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        store_root.join(".fantastic"),
        false,
    );
    let _ = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // The file_bridge store: OPEN, rooted at store_root (inside the running dir).
    let mut store_meta = Map::new();
    store_meta.insert("root".to_string(), json!(store_root.to_string_lossy()));
    store_meta.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    let store = Agent::new(
        AgentId::from("store"),
        Some("file_bridge.tools".to_string()),
        Some(AgentId::from("core")),
        store_meta,
        store_root.to_path_buf(),
        false,
    );
    let _ = kernel.register(Arc::clone(&store));
    // The durable_memory agent, wired to the store.
    let mut meta = Map::new();
    meta.insert("file_bridge_id".to_string(), json!("store"));
    let agent = Agent::new(
        AgentId::from("mem"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("core")),
        meta,
        store_root.join("mem"),
        false,
    );
    let _ = kernel.register(Arc::clone(&agent));
    (kernel, AgentId::from("mem"))
}

/// A durable_memory agent with NO provider wired — for the failfast path.
fn mk_unwired() -> (Arc<Kernel>, AgentId) {
    let kernel = Arc::new(Kernel::new());
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::from("/tmp/unused"),
        false,
    );
    let _ = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let agent = Agent::new(
        AgentId::from("mem"),
        Some(HANDLER_MODULE.to_string()),
        Some(AgentId::from("core")),
        Map::new(),
        std::path::PathBuf::from("/tmp/unused"),
        false,
    );
    let _ = kernel.register(Arc::clone(&agent));
    (kernel, AgentId::from("mem"))
}

async fn call(kernel: &Arc<Kernel>, id: &AgentId, payload: Value) -> Value {
    DurableMemoryBundle
        .handle(id, &payload, kernel)
        .await
        .expect("handle ok")
        .expect("some reply")
}

#[tokio::test]
async fn set_read_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path()).await;
    let s = call(
        &k,
        &id,
        json!({"type":"set","key":"user.name","value":"Ada"}),
    )
    .await;
    assert_eq!(s["ok"], true, "{s}");
    let r = call(&k, &id, json!({"type":"read","key":"user.name"})).await;
    assert_eq!(r["value"], "Ada");
    let miss = call(&k, &id, json!({"type":"read","key":"nope"})).await;
    assert_eq!(miss["value"], Value::Null);
}

#[tokio::test]
async fn keys_survey_sorted() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path()).await;
    call(&k, &id, json!({"type":"set","key":"z","value":"hello"})).await;
    call(&k, &id, json!({"type":"set","key":"a","value":[1,2,3]})).await;
    let r = call(&k, &id, json!({"type":"keys"})).await;
    assert_eq!(r["keys"], json!(["a", "z"])); // sorted
}

#[tokio::test]
async fn delete_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path()).await;
    call(&k, &id, json!({"type":"set","key":"k","value":"v"})).await;
    assert_eq!(
        call(&k, &id, json!({"type":"delete","key":"k"})).await["deleted"],
        true
    );
    assert_eq!(
        call(&k, &id, json!({"type":"read","key":"k"})).await["value"],
        Value::Null
    );
    // absent key → deleted:false, no error
    assert_eq!(
        call(&k, &id, json!({"type":"delete","key":"k"})).await["deleted"],
        false
    );
}

#[tokio::test]
async fn append_builds_a_list_and_refuses_a_non_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path()).await;
    // append to an absent key creates the list.
    let a1 = call(&k, &id, json!({"type":"append","key":"log","value":"one"})).await;
    assert_eq!(a1["ok"], true);
    assert_eq!(a1["length"], 1);
    let a2 = call(&k, &id, json!({"type":"append","key":"log","value":"two"})).await;
    assert_eq!(a2["length"], 2);
    assert_eq!(
        call(&k, &id, json!({"type":"read","key":"log"})).await["value"],
        json!(["one", "two"])
    );
    // append to a NON-list key refuses.
    call(&k, &id, json!({"type":"set","key":"scalar","value":"x"})).await;
    let refused = call(&k, &id, json!({"type":"append","key":"scalar","value":"y"})).await;
    assert!(
        refused["error"].as_str().unwrap_or("").contains("non-list"),
        "{refused}"
    );
}

#[tokio::test]
async fn reflect_is_modeless_and_surfaces_the_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path()).await;
    call(&k, &id, json!({"type":"set","key":"a","value":1})).await;
    let r = call(&k, &id, json!({"type":"reflect"})).await;
    // No `mode` anywhere — the enum is gone.
    assert!(r.get("mode").is_none(), "{r}");
    assert!(r["sentence"].as_str().unwrap().contains("durable"));
    assert_eq!(r["file_bridge_id"], "store");
    assert_eq!(r["keys"], json!(["a"]));
    assert!(r["verbs"]["append"].is_string(), "{r}");
    assert!(r["verbs"]["set"].is_string(), "{r}");
}

#[tokio::test]
async fn disk_is_truth() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path()).await;
    call(&k, &id, json!({"type":"set","key":"k","value":"v"})).await;
    // state.yaml lands at the store's root.
    let on_disk = std::fs::read_to_string(tmp.path().join("state.yaml")).unwrap();
    assert!(on_disk.contains("k: v"), "{on_disk}");
}

#[tokio::test]
async fn mutators_failfast_without_a_store() {
    // NO file_bridge_id → set/append REFUSE (no silent loss); read/keys degrade to
    // empty. A `delete` of an ABSENT key never reaches a write (py parity: unwired
    // reads empty, so nothing existed to remove), so it settles `deleted:false`
    // without a failfast — only a delete that actually removes something persists.
    let (k, id) = mk_unwired();
    for (verb, payload) in [
        ("set", json!({"type":"set","key":"k","value":"v"})),
        ("append", json!({"type":"append","key":"k","value":"v"})),
    ] {
        let r = call(&k, &id, payload).await;
        let err = r["error"].as_str().unwrap_or_default();
        assert!(
            err.starts_with(&format!("durable_memory.{verb}:")),
            "{verb} must fail in this bundle's own words: {r}"
        );
        assert!(
            err.contains("no file_bridge_id wired"),
            "{verb} must name the real cause: {r}"
        );
    }
    assert_eq!(
        call(&k, &id, json!({"type":"delete","key":"k"})).await["deleted"],
        false
    );
    // reads degrade to empty, not error.
    assert_eq!(
        call(&k, &id, json!({"type":"read","key":"x"})).await["value"],
        Value::Null
    );
    assert_eq!(
        call(&k, &id, json!({"type":"keys"})).await["keys"],
        json!([])
    );
}
