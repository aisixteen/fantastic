//! Merging grant maps: add, widen, never narrow.

use super::*;
use serde_json::json;

fn m(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[test]
fn union_adds_widens_and_never_narrows() {
    let mut a = m(json!({"x": "*", "y": ["a"], "z": ["a"], "keep": ["k"]}));
    union(
        &mut a,
        &m(json!({"x": ["v"], "y": ["b", "a"], "z": "*", "new": ["n"]})),
    );
    assert_eq!(
        Value::Object(a),
        json!({"x": "*", "y": ["a", "b"], "z": "*", "keep": ["k"], "new": ["n"]})
    );
}
