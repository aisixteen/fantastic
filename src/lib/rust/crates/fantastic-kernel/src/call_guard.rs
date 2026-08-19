//! Call-chain guard for authored-runtime connectors (lua/js/…): every
//! connector `send` tags the payload with a `_call_stack` (own agent id
//! appended) and REFUSES dispatch on a cycle (own id already present) or
//! depth > [`MAX_CALL_DEPTH`]. Keeps authored call chains — script → agent
//! → script → … — from recursing into themselves or running away.
//!
//! Two halves, ONE rule. [`check_call_stack`] is the rule: it READS an inbound
//! payload and says whether the next hop may happen. [`guard_call_stack`] is
//! the connector's read-then-tag combination on top of it. A verb handler that
//! only wants the inbound check (an AI backend's `send`, which must refuse a
//! cycle BEFORE it takes its FIFO lock, and must NOT propagate the stack onward)
//! calls the predicate directly — so there is one implementation of "is this
//! chain allowed", not a second copy that drifts.

use serde_json::{json, Value};

/// Max authored-call chain depth before a connector refuses a `send`.
pub const MAX_CALL_DEPTH: usize = 8;

/// Why a `_call_stack` refuses the next hop.
///
/// Carries the FACT, not a sentence: a connector words the refusal for a script
/// author, an AI backend words it for a model and hands it the chain it walked
/// into. One rule, two audiences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallStackRefusal {
    /// `self_id` is already in the chain — the next hop would close a loop. The
    /// vec is the chain WITH `self_id` appended: the cycle as walked.
    Cycle(Vec<Value>),
    /// The chain is already [`MAX_CALL_DEPTH`] hops deep.
    TooDeep,
}

/// May this `_call_stack` take one more hop into `self_id`? `Ok` returns the
/// chain AS IT ARRIVED (without `self_id`); `Err` names which rule refused.
///
/// Pure — no kernel access, no mutation. Callers that need to PROPAGATE the
/// chain use [`guard_call_stack`], which tags on top of this.
pub fn check_call_stack(self_id: &str, payload: &Value) -> Result<Vec<Value>, CallStackRefusal> {
    let stack = payload
        .get("_call_stack")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if stack.iter().any(|v| v.as_str() == Some(self_id)) {
        let mut cycle = stack;
        cycle.push(json!(self_id));
        return Err(CallStackRefusal::Cycle(cycle));
    }
    if stack.len() >= MAX_CALL_DEPTH {
        return Err(CallStackRefusal::TooDeep);
    }
    Ok(stack)
}

/// Guard + tag a connector `send` payload: `Err` on cycle/depth, else the
/// payload's `_call_stack` gains `self_id`. Pure — no kernel access.
pub fn guard_call_stack(self_id: &str, payload: &mut Value) -> Result<(), String> {
    let mut stack = match check_call_stack(self_id, payload) {
        Ok(stack) => stack,
        Err(CallStackRefusal::Cycle(_)) => {
            return Err(format!(
                "send refused: cycle — {self_id} is already in the call chain"
            ));
        }
        Err(CallStackRefusal::TooDeep) => {
            return Err(format!(
                "send refused: call depth > {MAX_CALL_DEPTH} (runaway chain)"
            ));
        }
    };
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

    /// The predicate the tagger is built on, checked on its own: a caller that
    /// must NOT propagate the chain (an AI backend's `send`) still gets the
    /// identical verdict, and a cycle carries the chain as walked.
    #[test]
    fn check_reports_the_chain_and_never_mutates() {
        let ok = json!({"type": "x", "_call_stack": ["a"]});
        assert_eq!(check_call_stack("me", &ok), Ok(vec![json!("a")]));
        assert_eq!(
            ok["_call_stack"],
            json!(["a"]),
            "the predicate is read-only"
        );

        let cyc = json!({"type": "x", "_call_stack": ["a", "me", "b"]});
        assert_eq!(
            check_call_stack("me", &cyc),
            Err(CallStackRefusal::Cycle(vec![
                json!("a"),
                json!("me"),
                json!("b"),
                json!("me"),
            ]))
        );

        let deep: Vec<String> = (0..MAX_CALL_DEPTH).map(|i| format!("a{i}")).collect();
        let too_deep = json!({"type": "x", "_call_stack": deep});
        assert_eq!(
            check_call_stack("me", &too_deep),
            Err(CallStackRefusal::TooDeep)
        );
    }
}
