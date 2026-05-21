//! Unit tests for [`crate::agent`].

use super::*;
use serde_json::json;
use std::path::Path;

fn make() -> Arc<Agent> {
    Agent::new(
        "test_1".into(),
        Some("file.tools".to_string()),
        Some("core".into()),
        {
            let mut m = Map::new();
            m.insert("display_name".to_string(), json!("Testy"));
            m.insert("port".to_string(), json!(8080));
            m
        },
        Path::new("/tmp/nowhere/test_1").to_path_buf(),
        false,
    )
}

#[test]
fn agent_id_round_trips() {
    let id: AgentId = "core".into();
    assert_eq!(id.as_str(), "core");
    let s: String = "file_abc123".into();
    let id2: AgentId = s.clone().into();
    assert_eq!(id2.0, s);
    assert_eq!(format!("{id2}"), "file_abc123");
}

#[test]
fn record_includes_meta_and_omits_none() {
    let a = make();
    let rec = a.record();
    assert_eq!(rec.id, "test_1");
    assert_eq!(rec.handler_module.as_deref(), Some("file.tools"));
    assert_eq!(rec.parent_id.as_deref(), Some("core"));
    assert_eq!(rec.meta.get("display_name"), Some(&json!("Testy")));
    assert_eq!(rec.meta.get("port"), Some(&json!(8080)));
    // Round-trip respects skip_serializing_if=None.
    let v = serde_json::to_value(&rec).unwrap();
    assert_eq!(v["id"], "test_1");
    assert_eq!(v["handler_module"], "file.tools");
    assert_eq!(v["parent_id"], "core");
    assert_eq!(v["display_name"], "Testy");
    assert_eq!(v["port"], 8080);
}

#[test]
fn display_name_reads_from_meta() {
    let a = make();
    assert_eq!(a.display_name().as_deref(), Some("Testy"));
}

#[test]
fn update_meta_merges_and_persists() {
    let a = make();
    let mut patch = Map::new();
    patch.insert("port".to_string(), json!(9090));
    patch.insert("note".to_string(), json!("hi"));
    let rec = a.update_meta(patch);
    assert_eq!(rec.meta["port"], json!(9090));
    assert_eq!(rec.meta["note"], json!("hi"));
    // Unchanged keys survive.
    assert_eq!(rec.meta["display_name"], json!("Testy"));
}

#[test]
fn delete_lock_flag() {
    let a = make();
    assert!(!a.is_delete_locked());
    let mut p = Map::new();
    p.insert("delete_lock".to_string(), json!(true));
    a.update_meta(p);
    assert!(a.is_delete_locked());
}

#[test]
fn record_serializes_with_optional_fields_omitted_when_none() {
    // The persisted JSON shape: optional fields (handler_module,
    // parent_id) must NOT appear in the object when None.
    let a = make();
    let v = serde_json::to_value(a.record()).unwrap();
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("id"));
    assert!(obj.contains_key("handler_module"));
    assert!(obj.contains_key("parent_id"));
    assert!(obj.contains_key("display_name"));
    assert!(obj.contains_key("port"));
    let bare = Agent::new(
        "root".into(),
        None,
        None,
        Map::new(),
        Path::new("/").to_path_buf(),
        false,
    );
    let v2 = serde_json::to_value(bare.record()).unwrap();
    let o2 = v2.as_object().unwrap();
    assert!(o2.contains_key("id"));
    assert!(!o2.contains_key("handler_module"));
    assert!(!o2.contains_key("parent_id"));
}
