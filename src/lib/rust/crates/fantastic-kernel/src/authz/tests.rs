//! Pure unit tests for the call-security grant engine — mirror of the python
//! `tests/test_grants.py`. Deny-all is the law; a grant on the sender opens it.

use super::*;
use serde_json::json;
use std::collections::HashSet;

fn kids(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

// holder "worker", parent "boss", one child "kid". Egress posture (enforce_privileged).
fn auth(grant: &Value, verb: &str, other: &str) -> Decision {
    authorize_call(
        grant,
        verb,
        other,
        "worker",
        Some("boss"),
        &kids(&["kid"]),
        Some("root"),
        true,
        None,
    )
}

// Ingress posture — privileged-no-wildcard is NOT enforced.
fn auth_ingress(grant: &Value, verb: &str, other: &str) -> Decision {
    authorize_call(
        grant,
        verb,
        other,
        "worker",
        Some("boss"),
        &kids(&["kid"]),
        Some("root"),
        false,
        None,
    )
}

#[test]
fn absent_grant_denies_everything() {
    let d = auth(&Value::Null, "send", "peer");
    assert!(!d.allowed);
    assert!(!d.hint.is_empty()); // a denial always teaches (from above)
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
fn baseline_reaches_self_parent_and_children() {
    for base in [default_can_call(), default_callable_by()] {
        let g = Value::Object(base);
        assert!(auth(&g, "send", "worker").allowed); // $self
        assert!(auth(&g, "send", "boss").allowed); // $parent
        assert!(auth(&g, "send", "kid").allowed); // $children
        assert!(!auth(&g, "send", "stranger").allowed);
    }
}

#[test]
fn ingress_wildcard_accepts_privileged_verbs() {
    // enforce_privileged=false (ingress): a callee's `$children:*` legitimately
    // accepts a privileged verb — escalation is blocked at egress.
    let g = json!({"$children": "*"});
    assert!(!auth(&g, "create_agent", "kid").allowed); // egress default
    assert!(auth_ingress(&g, "create_agent", "kid").allowed); // ingress
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

/// Measured 2026-09-28: `$parent: "*"` let every direct child of the root stop the
/// kernel, kill ANY task and read the whole ledger. A structural token's wildcard
/// covers none of them; a grant that names the verb does.
#[test]
fn the_baseline_never_reaches_the_kernel_controls() {
    let baseline = Value::Object(default_can_call());
    for verb in [
        "kernel_stop",
        "kernel_start",
        "shutdown_agents",
        "ps",
        "cancel",
        "ledger",
        "ack_deadletter",
        "persist_record",
        "forget_record",
        "load_tree",
    ] {
        assert!(!auth(&baseline, verb, "boss").allowed, "{verb}");
        assert!(
            auth(&json!({"boss": [verb]}), verb, "boss").allowed,
            "{verb}"
        );
    }
}

/// Measured 2026-10-03 (audit BV-1): a web_ws/web_rest leg's `$parent:*` let any
/// client it admitted `stop` its web host for every client, or `unmount` a sealed
/// sibling's routes. By verb name, whatever the bundle: a structural token's
/// wildcard covers none of them; a NAMED verb or a NAMED id's `*` does.
#[test]
fn the_baseline_never_reaches_an_agents_lifecycle_or_routes() {
    let baseline = Value::Object(default_can_call());
    for verb in ["stop", "mount", "unmount", "boot"] {
        assert!(PRIVILEGED_VERBS.contains(&verb), "{verb}");
        assert!(!auth(&baseline, verb, "boss").allowed, "{verb}");
        assert!(!auth(&baseline, verb, "kid").allowed, "{verb}");
        assert!(
            auth(&json!({"$parent": [verb]}), verb, "boss").allowed,
            "{verb}"
        );
        assert!(auth(&json!({"boss": "*"}), verb, "boss").allowed, "{verb}");
        // ingress: a callee's `$children:*` still accepts it
        assert!(
            auth_ingress(&json!({"$children": "*"}), verb, "kid").allowed,
            "{verb}"
        );
    }
    assert_eq!(PRIVILEGED_VERBS.len(), 20);
}

/// Audit 2026-10-03 (KV-2): `watch` whose src is the ROOT (`loader_id`) needs a
/// NAMED grant; every other `watch` is an ordinary verb.
#[test]
fn watching_the_root_loses_a_structural_wildcard() {
    let baseline = Value::Object(default_can_call());
    // holder "worker", child of the root "boss"
    let on_root = |grant: &Value, other: &str| {
        authorize_call(
            grant,
            "watch",
            other,
            "worker",
            Some("boss"),
            &kids(&["kid"]),
            Some("boss"),
            true,
            None,
        )
        .allowed
    };
    assert!(!on_root(&baseline, "boss"));
    assert!(on_root(&json!({"boss": ["watch"]}), "boss"));
    assert!(on_root(&json!({"boss": "*"}), "boss"));
    assert!(on_root(&json!({"$parent": ["watch"]}), "boss"));
    assert!(on_root(&baseline, "kid"));
    assert!(auth(&baseline, "watch", "boss").allowed); // "boss" is not the root here
}

/// `*` under a NAMED id is the operator's own statement — it covers everything,
/// privileged verbs included. Whoever writes it owns it.
#[test]
fn a_named_wildcard_covers_privileged_verbs() {
    let g = json!({"root": "*"});
    for verb in [
        "create_agent",
        "delete_agent",
        "kernel_stop",
        "cancel",
        "allow",
    ] {
        assert!(auth(&g, verb, "root").allowed, "{verb}");
    }
    assert!(auth(&json!({"root": ["*"]}), "update_agent", "root").allowed);
}

/// A foreign caller is named `<guid>|<agent>`, or every agent of that one kernel
/// with `<guid>|*`; the structural tokens never match it.
#[test]
fn a_foreign_caller_matches_only_by_name() {
    assert!(auth_ingress(&json!({"g1|ai": "*"}), "read", "g1|ai").allowed);
    assert!(!auth_ingress(&json!({"g1|ai": "*"}), "read", "g1|other").allowed);
    assert!(auth_ingress(&json!({"g1|*": "*"}), "read", "g1|anyone").allowed);
    assert!(!auth_ingress(&json!({"g1|*": "*"}), "read", "g2|anyone").allowed);
    let baseline = Value::Object(default_callable_by());
    assert!(!auth_ingress(&baseline, "read", "boss|x").allowed);
}

// ── $subtree:<X> + heritable entries ─────────────────────────────────────────

/// A live tree for the resolver: parent links, nearest first up to the root.
fn tree_parent(id: &str) -> Option<&'static str> {
    match id {
        "hub" => Some("root"),
        "kid" => Some("hub"),
        "grandkid" => Some("kid"),
        "outsider" => Some("root"),
        _ => None,
    }
}

fn in_tree(other: &str, x: &str) -> bool {
    if other != "root" && tree_parent(other).is_none() {
        return false; // not a live agent
    }
    let mut cur = Some(other);
    while let Some(id) = cur {
        if id == x {
            return true;
        }
        cur = tree_parent(id);
    }
    false
}

fn auth_sub(grant: &Value, verb: &str, other: &str, egress: bool) -> Decision {
    authorize_call(
        grant,
        verb,
        other,
        "served",
        Some("root"),
        &kids(&[]),
        Some("root"),
        egress,
        Some(&in_tree),
    )
}

#[test]
fn subtree_matches_x_and_every_live_descendant() {
    let g = json!({"$subtree:hub": ["list"]});
    for other in ["hub", "kid", "grandkid"] {
        assert!(auth_sub(&g, "list", other, false).allowed, "{other}");
    }
    assert!(!auth_sub(&g, "list", "outsider", false).allowed);
    assert!(!auth_sub(&g, "list", "root", false).allowed);
    assert!(!auth_sub(&g, "mkdir", "kid", false).allowed);
}

#[test]
fn subtree_never_matches_a_foreign_id() {
    let g = json!({"$subtree:hub": "*"});
    assert!(!auth_sub(&g, "list", "g1|hub", false).allowed);
    assert!(!auth_sub(&g, "list", "g1|kid", false).allowed);
}

#[test]
fn subtree_of_an_absent_id_matches_nothing() {
    let g = json!({"$subtree:gone": "*"});
    assert!(!auth_sub(&g, "list", "gone", false).allowed);
    assert!(!auth_sub(&g, "list", "kid", false).allowed);
    // an absent OTHER is under nobody either
    assert!(!auth_sub(&json!({"$subtree:hub": "*"}), "list", "ghost", false).allowed);
}

#[test]
fn subtree_without_a_resolver_matches_nothing() {
    let g = json!({"$subtree:hub": "*"});
    assert!(!auth_ingress(&g, "list", "hub").allowed);
}

#[test]
fn subtree_is_structural_for_privileged_verbs() {
    // enforce_privileged: a NAMED verb covers, `*` never does
    assert!(auth_sub(&json!({"$subtree:hub": ["allow"]}), "allow", "kid", true).allowed);
    assert!(!auth_sub(&json!({"$subtree:hub": "*"}), "allow", "kid", true).allowed);
    assert!(!auth_sub(&json!({"$subtree:hub": ["*"]}), "allow", "kid", true).allowed);
    // ingress posture accepts it
    assert!(auth_sub(&json!({"$subtree:hub": "*"}), "allow", "kid", false).allowed);
}

#[test]
fn a_heritable_entry_never_grants_its_holder() {
    let g = json!({"served": {"verbs": ["list"], "for": "descendants"}});
    assert!(!auth(&g, "list", "served").allowed);
    assert!(!auth_ingress(&g, "list", "served").allowed);
}

#[test]
fn heritable_shape_is_exact() {
    assert!(is_heritable(
        &json!({"verbs": ["list"], "for": "descendants"})
    ));
    assert!(is_heritable(&json!({"verbs": ["*"], "for": "descendants"})));
    assert!(!is_heritable(
        &json!({"verbs": ["list"], "for": "siblings"})
    ));
    assert!(!is_heritable(&json!({"verbs": [], "for": "descendants"})));
    assert!(!is_heritable(
        &json!({"verbs": "list", "for": "descendants"})
    ));
    assert!(!is_heritable(&json!({"verbs": [1], "for": "descendants"})));
    assert!(!is_heritable(&json!({"verbs": ["list"]})));
    assert!(!is_heritable(
        &json!({"verbs": ["list"], "for": "descendants", "x": 1})
    ));
    assert!(!is_heritable(&json!(["list"])));
}

#[test]
fn heritable_egress_walks_ancestors() {
    let owner = json!({"served": {"verbs": ["list", "allow"], "for": "descendants"}});
    let plain = json!({"served": ["mkdir"]}); // a plain grant is not heritable
    assert!(authorize_heritable([&plain, &owner], "list", "served", None).allowed);
    assert!(authorize_heritable([&owner], "allow", "served", None).allowed); // named privileged
    assert!(!authorize_heritable([&plain, &owner], "mkdir", "served", None).allowed);
    assert!(!authorize_heritable([&owner], "list", "other", None).allowed);
    assert!(!authorize_heritable(std::iter::empty(), "list", "served", None).allowed);
}

#[test]
fn heritable_wildcard_never_covers_a_privileged_verb() {
    let owner = json!({"served": {"verbs": ["*"], "for": "descendants"}});
    assert!(authorize_heritable([&owner], "list", "served", None).allowed);
    for verb in PRIVILEGED_VERBS {
        assert!(
            !authorize_heritable([&owner], verb, "served", None).allowed,
            "{verb}"
        );
    }
}

/// Audit KV-2: `watch` on the ROOT needs a NAMED grant — a heritable `*` on the
/// root does not cover it; `watch` on any other agent stays ordinary.
#[test]
fn heritable_wildcard_does_not_cover_watch_on_the_root() {
    let wild = json!({"kernel_state": {"verbs": ["*"], "for": "descendants"},
                      "served": {"verbs": ["*"], "for": "descendants"}});
    let root = Some("kernel_state");
    assert!(authorize_heritable([&wild], "list", "kernel_state", root).allowed);
    assert!(!authorize_heritable([&wild], "watch", "kernel_state", root).allowed);
    assert!(authorize_heritable([&wild], "watch", "served", root).allowed);
    let named = json!({"kernel_state": {"verbs": ["watch"], "for": "descendants"}});
    assert!(authorize_heritable([&named], "watch", "kernel_state", root).allowed);
}

#[test]
fn heritable_conferral_is_a_subset() {
    let held = json!({"verbs": ["list", "allow"], "for": "descendants"});
    let wild = json!({"verbs": ["*"], "for": "descendants"});
    let w = |v: Value| v.as_array().cloned().unwrap();
    assert_eq!(heritable_uncovered(Some(&held), &w(json!(["list"]))), None);
    assert_eq!(
        heritable_uncovered(Some(&held), &w(json!(["list", "allow"]))),
        None
    );
    assert_eq!(
        heritable_uncovered(Some(&held), &w(json!(["list", "disallow"]))).as_deref(),
        Some("disallow")
    );
    assert_eq!(
        heritable_uncovered(Some(&held), &w(json!(["*"]))).as_deref(),
        Some("*")
    );
    // `*` requires `*`; a held `*` covers an ordinary verb, not a privileged one
    assert_eq!(heritable_uncovered(Some(&wild), &w(json!(["*"]))), None);
    assert_eq!(heritable_uncovered(Some(&wild), &w(json!(["list"]))), None);
    assert_eq!(
        heritable_uncovered(Some(&wild), &w(json!(["allow"]))).as_deref(),
        Some("allow")
    );
    // a plain grant, or none, holds nothing heritable
    assert_eq!(
        heritable_uncovered(Some(&json!("*")), &w(json!(["list"]))).as_deref(),
        Some("list")
    );
    assert_eq!(
        heritable_uncovered(None, &w(json!(["list"]))).as_deref(),
        Some("list")
    );
}

#[test]
fn a_heritable_grant_round_trips_through_the_record_yaml() {
    let rec = json!({
        "handler_module": "file_bridge.tools",
        "can_call": {
            "$self": "*",
            "served": {"verbs": ["list", "allow"], "for": "descendants"},
        },
        "callable_by": {"$subtree:hub": ["list"], "$parent": "*"},
    });
    let text = crate::yaml_io::dump(&rec);
    let back = crate::yaml_io::load(&text).unwrap();
    assert_eq!(back, rec, "{text}");
    assert!(is_heritable(&back["can_call"]["served"]));
}
