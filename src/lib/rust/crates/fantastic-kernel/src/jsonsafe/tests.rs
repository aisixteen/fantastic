//! The non-finite number python writes must survive being embedded.

use super::{obj, record_value};
use crate::agent::AgentRecord;
use crate::test_support::non_finite;
use serde_json::{json, Map, Value};

#[test]
fn json_macro_cannot_carry_a_non_finite_number() {
    // The fault, pinned so the reason this module exists stays true:
    // `to_value` re-parses the stored text and `NaN` is not JSON.
    let nan = non_finite("NaN");
    assert_eq!(nan.to_string(), "NaN");
    assert!(
        serde_json::to_value(&nan).is_err(),
        "if this ever succeeds, `json!` is safe again and `obj` can go"
    );
}

#[test]
fn obj_carries_a_non_finite_number_verbatim() {
    let payload = json!({"type": "create_agent"});
    let Value::Object(mut m) = payload else {
        unreachable!()
    };
    m.insert("nan".to_string(), non_finite("NaN"));
    m.insert("inf".to_string(), non_finite("-Infinity"));
    let out = obj([("payload", Value::Object(m)), ("kind", Value::from("send"))]);
    assert_eq!(
        out.to_string(),
        r#"{"payload":{"type":"create_agent","nan":NaN,"inf":-Infinity},"kind":"send"}"#
    );
}

#[test]
fn a_record_with_a_non_finite_meta_value_still_serializes() {
    let mut meta = Map::new();
    meta.insert("nan".to_string(), non_finite("NaN"));
    meta.insert(
        "big".to_string(),
        serde_json::from_str::<Value>("123456789012345678901234567890").expect("a wide integer"),
    );
    let rec = AgentRecord {
        id: "coerced".to_string(),
        handler_module: Some("file_bridge.tools".to_string()),
        meta,
        parent_id: Some("kernel_state".to_string()),
    };
    // python's key order: id, handler_module, meta, parent_id — and the record
    // is the reply `create_agent` answers with, so `null` here is the bug.
    assert_eq!(
        record_value(&rec).to_string(),
        r#"{"id":"coerced","handler_module":"file_bridge.tools","nan":NaN,"big":123456789012345678901234567890,"parent_id":"kernel_state"}"#
    );
    assert!(serde_json::to_value(&rec).is_err(), "the derived impl dies");
}
