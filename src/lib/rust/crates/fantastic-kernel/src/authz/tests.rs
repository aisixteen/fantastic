//! Pure unit tests for the call-security grant engine. Deny-all is the law; an
//! explicit grant opens one side of an edge.

use super::*;
use serde_json::json;
use std::collections::HashSet;

fn kids(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

// holder "worker", parent "boss", one child "kid".
fn auth(grant: &Value, verb: &str, other: &str) -> Decision {
    authorize_call(grant, verb, other, "worker", Some("boss"), &kids(&["kid"]))
}

#[test]
fn absent_grant_denies_everything() {
    let d = auth(&Value::Null, "send", "peer");
    assert!(!d.allowed);
    assert!(!d.hint.is_empty()); // a denial always teaches
}

#[test]
fn empty_grant_denies() {
    assert!(!auth(&json!({}), "send", "peer").allowed);
}

#[test]
fn self_token_resolves_to_holder_id() {
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
    assert!(!auth(&g, "ping", "stranger").allowed);
}

#[test]
fn baseline_is_self_only() {
    let g = Value::Object(default_can_call());
    assert_eq!(g, json!({"$self": "*"}));
    assert_eq!(Value::Object(default_callable_by()), json!({"$self": "*"}));
    assert!(auth(&g, "anything", "worker").allowed);
    assert!(!auth(&g, "ping", "boss").allowed);
    assert!(!auth(&g, "ping", "kid").allowed);
}

#[test]
fn star_covers_every_verb_including_the_mutators() {
    let g = json!({"kernel_state": "*"});
    for verb in [
        "create_agent",
        "update_agent",
        "delete_agent",
        "state_subscribe",
        "kernel_stop",
        "watch",
        "stop",
    ] {
        assert!(auth(&g, verb, "kernel_state").allowed, "{verb}");
    }
    let g = json!({"$parent": "*"});
    assert!(auth(&g, "create_agent", "boss").allowed);
    let g = json!({"$parent": ["*"]});
    assert!(auth(&g, "kernel_stop", "boss").allowed);
}

#[test]
fn per_verb_list_grants_exactly_those() {
    let g = json!({"peer": ["read", "send"]});
    assert!(auth(&g, "read", "peer").allowed);
    assert!(auth(&g, "send", "peer").allowed);
    assert!(!auth(&g, "write", "peer").allowed);
    assert!(!auth(&g, "read", "other").allowed);
}

#[test]
fn a_peer_target_matches_only_by_name() {
    // egress: a `can_call` naming one agent of a connected kernel, or all of it
    let one = json!({"g1|alice": ["ping"]});
    assert!(auth(&one, "ping", "g1|alice").allowed);
    assert!(!auth(&one, "ping", "g1|bob").allowed);
    let all = json!({"g1|*": "*"});
    assert!(auth(&all, "ping", "g1|bob").allowed);
    assert!(!auth(&all, "ping", "g2|bob").allowed);
    // the tokens never match a peer address
    let base = json!({"$self": "*", "$parent": "*", "$children": "*"});
    assert!(!auth(&base, "ping", "g1|boss").allowed);
}

#[test]
fn removed_tokens_match_nothing() {
    for key in ["$loader", "$subtree:worker"] {
        let g = json!({ key: "*" });
        assert!(!auth(&g, "ping", "worker").allowed, "{key}");
        assert!(!auth(&g, "ping", "kid").allowed, "{key}");
    }
}

#[test]
fn grant_keys_are_exactly_the_legal_set() {
    for field in ["can_call", "callable_by"] {
        for ok in ["$self", "$parent", "$children", "ai"] {
            assert!(is_grant_key(field, ok), "{field} {ok}");
        }
        for bad in [
            "",
            "*",
            "$loader",
            "$subtree:x",
            "$anything",
            "|ai",
            "g1|",
            "a|b|c",
        ] {
            assert!(!is_grant_key(field, bad), "{field} {bad}");
        }
    }
    // a peer address is an OUTBOUND key: `can_call` only — a caller is always
    // local, so in `callable_by` it could never match
    for peer in ["g1|ai", "g1|*"] {
        assert!(is_grant_key("can_call", peer), "{peer}");
        assert!(!is_grant_key("callable_by", peer), "{peer}");
    }
    assert_eq!(
        legal_keys("callable_by"),
        "an agent id, $self, $parent, $children"
    );
    assert_eq!(
        legal_keys("can_call"),
        "an agent id, $self, $parent, $children, <guid>|<agent> or <guid>|*"
    );
}

#[test]
fn the_denial_hint_is_the_from_above_text() {
    assert_eq!(
        FROM_ABOVE,
        "grants come from above — the agent's parent (update_agent on its direct child) or \
         the operator writes can_call and callable_by; an agent never raises its own"
    );
    assert_eq!(auth(&json!({}), "ping", "peer").hint, FROM_ABOVE);
}
