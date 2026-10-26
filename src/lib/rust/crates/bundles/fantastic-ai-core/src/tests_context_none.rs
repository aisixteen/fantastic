//! context_strategy `none` — registry-level checks. The turn-refusal test lives in
//! `tests.rs` as `an_over_budget_turn_is_refused_under_none` (it needs that module's helpers).

use crate::strategies::{is_known_strategy, STRATEGY_NAMES};

#[test]
fn none_is_a_known_strategy() {
    assert!(is_known_strategy("none"));
    assert!(STRATEGY_NAMES.contains(&"none"));
}

#[test]
fn an_unknown_strategy_is_still_refused() {
    assert!(!is_known_strategy("shrink"));
    assert!(!is_known_strategy(""));
}
