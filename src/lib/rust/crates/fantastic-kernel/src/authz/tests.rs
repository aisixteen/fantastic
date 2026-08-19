//! Pure unit tests for the call-security grant engine — mirror of the python
//! `tests/test_grants.py`. Deny-all is the law; a grant on the sender opens it.

use super::*;
use serde_json::json;
use std::collections::HashSet;

fn kids(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

// sender "worker", parent "boss", one child "kid"
fn auth(can_call: &Value, verb: &str, target: &str) -> Decision {
    authorize_call(
        can_call,
        verb,
        target,
        "worker",
        Some("boss"),
        &kids(&["kid"]),
        Some("root"),
    )
}

#[test]
fn absent_grant_denies_everything() {
    let d = auth(&Value::Null, "send", "peer");
    assert!(!d.allowed);
    assert!(!d.reason.is_empty() && !d.hint.is_empty());
}

#[test]
fn empty_grant_denies() {
    assert!(!auth(&json!({}), "send", "peer").allowed);
}

#[test]
fn self_token_resolves_to_sender_id() {
    let g = json!({"$self": "*"});
    assert!(auth(&g, "ping", "worker").allowed);
    assert!(!auth(&g, "ping", "boss").allowed);
}

#[test]
fn parent_token_resolves_to_parent_id() {
    let g = json!({"$parent": "*"});
    assert!(auth(&g, "ping", "boss").allowed);
    assert!(!auth(&g, "ping", "worker").allowed);
}

#[test]
fn children_token_matches_only_direct_children() {
    let g = json!({"$children": "*"});
    assert!(auth(&g, "ping", "kid").allowed);
    // a non-child target is denied
    assert!(!auth(&g, "ping", "stranger").allowed);
}

#[test]
fn loader_token_resolves_to_passed_loader_id() {
    let g = json!({"$loader": "*"});
    assert!(auth(&g, "load", "root").allowed);
    assert!(!auth(&g, "load", "boss").allowed);
}

#[test]
fn unix_default_reaches_self_parent_and_children() {
    let g = Value::Object(default_can_call());
    assert!(auth(&g, "send", "worker").allowed); // $self
    assert!(auth(&g, "send", "boss").allowed); // $parent
    assert!(auth(&g, "send", "kid").allowed); // $children
    assert!(!auth(&g, "send", "stranger").allowed);
}

#[test]
fn literal_target_wildcard_verb() {
    let g = json!({"memory": "*"});
    assert!(auth(&g, "set", "memory").allowed);
    assert!(auth(&g, "read", "memory").allowed);
    assert!(!auth(&g, "set", "other").allowed);
}

#[test]
fn per_verb_list_grants_exactly_those() {
    let g = json!({"memory": ["read"]});
    assert!(auth(&g, "read", "memory").allowed);
    assert!(!auth(&g, "set", "memory").allowed);
}

#[test]
fn verb_wildcard_inside_list() {
    let g = json!({"memory": ["*"]});
    assert!(auth(&g, "anything", "memory").allowed);
}

#[test]
fn create_agent_denied_under_default_grant() {
    // create_agent on the parent (not self) is privileged → wildcard doesn't cover it.
    let g = Value::Object(default_can_call());
    assert!(!auth(&g, "create_agent", "boss").allowed);
}

#[test]
fn fork_into_own_subtree_is_not_privileged() {
    // create_agent on $self (adding a child to your OWN subtree) is baseline.
    let g = json!({"$self": "*"});
    assert!(auth(&g, "create_agent", "worker").allowed);
    assert!(!auth(&g, "create_agent", "boss").allowed);
}

#[test]
fn explicit_kernel_grant_opens_mutation() {
    let g = json!({"root": ["create_agent"]});
    assert!(auth(&g, "create_agent", "root").allowed);
    assert!(!auth(&g, "delete_agent", "root").allowed);
}
