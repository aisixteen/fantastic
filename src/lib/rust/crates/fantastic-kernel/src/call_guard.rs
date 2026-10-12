//! The `_call_stack` recursion rule an AI backend's `send` applies (python
//! `ai_core`): REFUSE a hop on a cycle (own id already present) or depth >
//! [`MAX_CALL_DEPTH`]. [`check_call_stack`] READS an inbound payload and says
//! whether the next hop may happen — it never tags or propagates the chain.
//! Nothing else tags a payload: the kernel's `send` and the program relays carry
//! a payload untouched, as python's do.

use serde_json::{json, Value};

/// Max `_call_stack` depth before an AI backend refuses a `send`.
pub const MAX_CALL_DEPTH: usize = 8;

/// Why a `_call_stack` refuses the next hop.
///
/// Carries the FACT, not a sentence: the AI backend words the refusal for a
/// model and hands it the chain it walked into.
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
/// Pure — no kernel access, no mutation.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The predicate, read-only: a cycle carries the chain as walked.
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
