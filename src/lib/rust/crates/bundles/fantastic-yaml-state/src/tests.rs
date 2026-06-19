//! Unit tests for the `yaml_state` agent — CRUD round-trip, state_yaml,
//! mode sentence, disk-is-truth. Mirrors the Python test_yaml_state.
//!
//! Persistence is INVERTED: state rides THROUGH a wired `file_bridge` provider
//! (a `FakeStore` rooted at the store dir), referenced by `file_bridge_id` —
//! this bundle owns no `std::fs` surface. `set`/`delete`/`replace` failfast until
//! that provider is wired.

use super::*;
use fantastic_kernel::test_support::{register_fake_store, wire_fake_store};
use serde_json::json;
use std::path::Path;

/// Build a kernel with a wired `.fantastic` store (FakeStore at `store_root`) +
/// a `yaml_state` agent bound to it via `file_bridge_id`. State lands at
/// `store_root/agents/agent/state.yaml` (store-relative, next to its agent.json).
async fn mk_agent(store_root: &Path, mode: &str) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    register_fake_store(&mut kernel.bundles, store_root);
    let kernel = Arc::new(kernel);
    // Root — so create_agent (wire_fake_store) has a parent.
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        store_root.to_path_buf(),
        false,
    );
    let _ = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // The `.fantastic` store (the file_bridge provider) under root.
    wire_fake_store(&kernel, store_root).await;
    // The yaml_state agent, wired to the store via file_bridge_id.
    let mut meta = Map::new();
    meta.insert("mode".to_string(), json!(mode));
    meta.insert("file_bridge_id".to_string(), json!("store"));
    let agent = Agent::new(
        AgentId::from("agent"),
        Some("yaml_state.tools".to_string()),
        Some(AgentId::from("core")),
        meta,
        store_root.join("agents/agent"),
        false,
    );
    let _ = kernel.register(Arc::clone(&agent));
    (kernel, AgentId::from("agent"))
}

/// A yaml_state agent with NO provider wired — for the failfast path.
fn mk_unwired(mode: &str) -> (Arc<Kernel>, AgentId) {
    let kernel = Arc::new(Kernel::new());
    let mut meta = Map::new();
    meta.insert("mode".to_string(), json!(mode));
    let agent = Agent::new(
        AgentId::from("agent"),
        Some("yaml_state.tools".to_string()),
        None,
        meta,
        std::path::PathBuf::from("/tmp/unused"),
        false,
    );
    let _ = kernel.register(Arc::clone(&agent));
    (kernel, AgentId::from("agent"))
}

async fn call(kernel: &Arc<Kernel>, id: &AgentId, payload: Value) -> Value {
    YamlStateBundle
        .handle(id, &payload, kernel)
        .await
        .expect("handle ok")
        .expect("some reply")
}

#[tokio::test]
async fn set_get_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data").await;
    call(
        &k,
        &id,
        json!({"type":"set","key":"user.name","value":"Ada"}),
    )
    .await;
    let r = call(&k, &id, json!({"type":"read","key":"user.name"})).await;
    assert_eq!(r["value"], "Ada");
    let miss = call(&k, &id, json!({"type":"read","key":"nope"})).await;
    assert_eq!(miss["value"], Value::Null);
}

#[tokio::test]
async fn get_whole_doc() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data").await;
    call(&k, &id, json!({"type":"set","key":"a","value":1})).await;
    call(&k, &id, json!({"type":"set","key":"b","value":"two"})).await;
    let r = call(&k, &id, json!({"type":"read"})).await;
    assert_eq!(r["doc"], json!({"a":1,"b":"two"}));
}

#[tokio::test]
async fn keys_survey_sorted_with_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data").await;
    call(&k, &id, json!({"type":"set","key":"z","value":"hello"})).await;
    call(&k, &id, json!({"type":"set","key":"a","value":[1,2,3]})).await;
    let r = call(&k, &id, json!({"type":"keys"})).await;
    let names: Vec<&str> = r["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["key"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a", "z"]); // sorted
    assert!(r["keys"][0]["size"].is_u64());
}

#[tokio::test]
async fn delete_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data").await;
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
async fn replace_and_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data").await;
    call(&k, &id, json!({"type":"set","key":"old","value":1})).await;
    call(&k, &id, json!({"type":"replace","doc":{"new":2}})).await;
    assert_eq!(
        call(&k, &id, json!({"type":"read"})).await["doc"],
        json!({"new":2})
    );
    call(&k, &id, json!({"type":"replace","doc":{}})).await;
    assert_eq!(
        call(&k, &id, json!({"type":"read"})).await["doc"],
        json!({})
    );
}

#[tokio::test]
async fn state_yaml_emits_and_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data").await;
    call(
        &k,
        &id,
        json!({"type":"set","key":"user.name","value":"Ada"}),
    )
    .await;
    let y = call(&k, &id, json!({"type":"state_yaml"})).await;
    let text = y["yaml"].as_str().unwrap();
    assert!(text.contains("user.name") && text.contains("Ada"));
    // empty store → empty string
    let tmp2 = tempfile::tempdir().unwrap();
    let (k2, id2) = mk_agent(tmp2.path(), "data").await;
    assert_eq!(
        call(&k2, &id2, json!({"type":"state_yaml"})).await["yaml"],
        ""
    );
}

#[tokio::test]
async fn reflect_mode_sentence() {
    let tmp = tempfile::tempdir().unwrap();
    let (km, mem) = mk_agent(&tmp.path().join("m"), "mem").await;
    let (kd, data) = mk_agent(&tmp.path().join("d"), "data").await;
    let rmem = call(&km, &mem, json!({"type":"reflect"})).await;
    let rdata = call(&kd, &data, json!({"type":"reflect"})).await;
    assert_eq!(rmem["mode"], "mem");
    assert!(rmem["sentence"]
        .as_str()
        .unwrap()
        .contains("durable memory"));
    assert_eq!(rdata["mode"], "data");
    assert!(rdata["sentence"]
        .as_str()
        .unwrap()
        .contains("scratch-state"));
    assert!(rmem["verbs"]["set"].is_string());
    // reflect surfaces the provider binding.
    assert_eq!(rmem["file_bridge_id"], "store");
}

#[tokio::test]
async fn disk_is_truth() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data").await;
    call(&k, &id, json!({"type":"set","key":"k","value":"v"})).await;
    // Store-relative `agents/<id>/state.yaml` under the provider's root.
    let on_disk =
        std::fs::read_to_string(tmp.path().join("agents").join("agent").join("state.yaml"))
            .unwrap();
    assert!(on_disk.contains("k: v"));
}

#[tokio::test]
async fn mutators_failfast_without_provider() {
    // No file_bridge_id wired → set/delete/replace refuse (no silent loss);
    // read/keys/state_yaml degrade to empty. Error text matches Python.
    let (k, id) = mk_unwired("data");
    let s = call(&k, &id, json!({"type":"set","key":"k","value":"v"})).await;
    assert_eq!(
        s["error"],
        "yaml_state.set: file_bridge_id required — wire (and open) a file_bridge to persist"
    );
    let d = call(&k, &id, json!({"type":"delete","key":"k"})).await;
    assert!(d["error"]
        .as_str()
        .unwrap()
        .starts_with("yaml_state.delete: file_bridge_id required"));
    let rp = call(&k, &id, json!({"type":"replace","doc":{}})).await;
    assert!(rp["error"]
        .as_str()
        .unwrap()
        .starts_with("yaml_state.replace: file_bridge_id required"));
    // reads degrade to empty, not error.
    assert_eq!(
        call(&k, &id, json!({"type":"read"})).await["doc"],
        json!({})
    );
    assert_eq!(
        call(&k, &id, json!({"type":"state_yaml"})).await["yaml"],
        ""
    );
}
