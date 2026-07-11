//! Call-chain guard for authored-runtime connectors (lua/js/…): every
//! connector `send` tags the payload with a `_call_stack` (own agent id
//! appended) and REFUSES dispatch on a cycle (own id already present) or
//! depth > [`MAX_CALL_DEPTH`]. Keeps authored call chains — script → agent
//! → script → … — from recursing into themselves or running away.

use serde_json::{json, Value};

/// Max authored-call chain depth before a connector refuses a `send`.
pub const MAX_CALL_DEPTH: usize = 8;

/// Guard + tag a connector `send` payload: `Err` on cycle/depth, else the
/// payload's `_call_stack` gains `self_id`. Pure — no kernel access.
pub fn guard_call_stack(self_id: &str, payload: &mut Value) -> Result<(), String> {
    let stack = payload
        .get("_call_stack")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if stack.iter().any(|v| v.as_str() == Some(self_id)) {
        return Err(format!(
            "send refused: cycle — {self_id} is already in the call chain"
        ));
    }
    if stack.len() >= MAX_CALL_DEPTH {
        return Err(format!(
            "send refused: call depth > {MAX_CALL_DEPTH} (runaway chain)"
        ));
    }
    let mut stack = stack;
    stack.push(json!(self_id));
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("_call_stack".to_string(), Value::Array(stack));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_refuses_cycles_and_depth() {
        let mut p = json!({"type": "x"});
        assert!(guard_call_stack("me", &mut p).is_ok());
        assert_eq!(p["_call_stack"], json!(["me"]));

        let mut p2 = json!({"type": "x", "_call_stack": ["a", "me"]});
        assert!(guard_call_stack("me", &mut p2).is_err());

        let deep: Vec<String> = (0..MAX_CALL_DEPTH).map(|i| format!("a{i}")).collect();
        let mut p3 = json!({"type": "x", "_call_stack": deep});
        assert!(guard_call_stack("me", &mut p3).is_err());
    }
}
