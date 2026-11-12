//! The `_call_stack` recursion rule an AI backend's `send` applies (python
//! `ai_core`): REFUSE a hop on a cycle (own id already present) or depth >
//! [`MAX_CALL_DEPTH`]. [`check_call_stack`] READS an inbound call's stack and says
//! whether the next hop may happen — it never tags or propagates the chain.
//! Nothing else tags a payload: the kernel's `send` and the program relays carry
//! a payload untouched, as python's do.

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
    Cycle(Vec<String>),
    /// The chain is already [`MAX_CALL_DEPTH`] hops deep.
    TooDeep,
}

/// May this `_call_stack` take one more hop into `self_id`? `Ok` carries nothing: the
/// chain may go on; `Err` names which rule refused.
///
/// Pure — no kernel access, no mutation.
pub fn check_call_stack(self_id: &str, stack: &[String]) -> Result<(), CallStackRefusal> {
    if stack.iter().any(|v| v == self_id) {
        let mut cycle = stack.to_vec();
        cycle.push(self_id.to_string());
        return Err(CallStackRefusal::Cycle(cycle));
    }
    if stack.len() >= MAX_CALL_DEPTH {
        return Err(CallStackRefusal::TooDeep);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The predicate, read-only: a cycle carries the chain as walked.
    #[test]
    fn check_reports_the_chain_and_never_mutates() {
        let ok = vec!["a".to_string()];
        assert_eq!(check_call_stack("me", &ok), Ok(()));
        assert_eq!(ok, ["a"], "the predicate is read-only");

        let cyc: Vec<String> = ["a", "me", "b"].map(String::from).to_vec();
        assert_eq!(
            check_call_stack("me", &cyc),
            Err(CallStackRefusal::Cycle(
                ["a", "me", "b", "me"].map(String::from).to_vec()
            ))
        );

        let deep: Vec<String> = (0..MAX_CALL_DEPTH).map(|i| format!("a{i}")).collect();
        assert_eq!(
            check_call_stack("me", &deep),
            Err(CallStackRefusal::TooDeep)
        );
    }
}
