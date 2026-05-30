//! Unit tests for the `yaml_state` agent — CRUD round-trip, state_yaml,
//! mode sentence, disk-is-truth. Mirrors the Python test_yaml_state.

use super::*;
use serde_json::json;
use std::path::Path;

fn mk_agent(dir: &Path, mode: &str) -> (Arc<Kernel>, AgentId) {
    let kernel = Arc::new(Kernel::new());
    let mut meta = Map::new();
    meta.insert("mode".to_string(), json!(mode));
    let agent = Agent::new(
        AgentId::from("agent"),
        Some("yaml_state.tools".to_string()),
        None,
        meta,
        dir.join("agent"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&agent));
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
    let (k, id) = mk_agent(tmp.path(), "data");
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
    let (k, id) = mk_agent(tmp.path(), "data");
    call(&k, &id, json!({"type":"set","key":"a","value":1})).await;
    call(&k, &id, json!({"type":"set","key":"b","value":"two"})).await;
    let r = call(&k, &id, json!({"type":"read"})).await;
    assert_eq!(r["doc"], json!({"a":1,"b":"two"}));
}

#[tokio::test]
async fn keys_survey_sorted_with_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data");
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
    let (k, id) = mk_agent(tmp.path(), "data");
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
    let (k, id) = mk_agent(tmp.path(), "data");
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
    let (k, id) = mk_agent(tmp.path(), "data");
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
    let (k2, id2) = mk_agent(tmp2.path(), "data");
    assert_eq!(
        call(&k2, &id2, json!({"type":"state_yaml"})).await["yaml"],
        ""
    );
}

#[tokio::test]
async fn reflect_mode_sentence() {
    let tmp = tempfile::tempdir().unwrap();
    let (km, mem) = mk_agent(&tmp.path().join("m"), "mem");
    let (kd, data) = mk_agent(&tmp.path().join("d"), "data");
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
}

#[tokio::test]
async fn disk_is_truth() {
    let tmp = tempfile::tempdir().unwrap();
    let (k, id) = mk_agent(tmp.path(), "data");
    call(&k, &id, json!({"type":"set","key":"k","value":"v"})).await;
    let on_disk = std::fs::read_to_string(tmp.path().join("agent").join("state.yaml")).unwrap();
    assert!(on_disk.contains("k: v"));
}
