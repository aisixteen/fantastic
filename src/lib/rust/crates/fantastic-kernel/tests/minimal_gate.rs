//! THE MINIMAL ACCESS MODEL (owner decision 2026-10-03), pinned.
//!
//! - Two-sided: an agent send passes iff the sender's `can_call` grants
//!   (target, verb) AND the target's `callable_by` grants (sender, verb).
//! - `"*"` covers EVERY verb — `create_agent` included.
//! - The baseline is `{"$self": "*"}` on both fields, nothing else.
//! - Kernel verbs answered by the root take the same check against the root.
//! - The MUTATORS act on the sender's own direct children only; a missing
//!   operand gets the verb's own refusal first.
//! - A grant key outside the legal set is refused `invalid_meta` at the write.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Outcome, Reply};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

const MOD: &str = "minimal_gate_probe.tools";

/// Answers `{ok: <id>, verb}` for any verb.
struct Probe;

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "minimal_gate_probe"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(match verb {
            "boot" | "shutdown" => None,
            _ => Some(json!({"ok": id.as_str(), "verb": verb})),
        })
    }
}

fn mk(root_callable_by: Value) -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register(MOD, Probe);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let mut meta = Map::new();
    meta.insert("callable_by".to_string(), root_callable_by);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        meta,
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (tmp, kernel)
}

async fn op(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn as_agent(kernel: &Arc<Kernel>, sender: &str, target: &str, payload: Value) -> Outcome {
    let k = Arc::clone(kernel);
    let t = target.to_string();
    fantastic_kernel::test_support::as_agent(AgentId::from(sender), async move {
        k.send(vec![Item::new(AgentId::from(t.as_str()), payload)])
            .await
            .remove(0)
    })
    .await
}

async fn create(kernel: &Arc<Kernel>, parent: &str, id: &str, meta: Value) {
    let mut p =
        json!({"type": "create_agent", "parent_id": parent, "handler_module": MOD, "id": id});
    for (k, v) in meta.as_object().cloned().unwrap_or_default() {
        p[k] = v;
    }
    let r = op(kernel, "kernel", p).await;
    assert_eq!(r["id"], json!(id), "{r}");
}

#[tokio::test]
async fn the_baseline_is_self_only_on_both_fields() {
    let (_t, k) = mk(json!({}));
    create(&k, "kernel_state", "a", json!({})).await;
    let rec = k.agents.get(&AgentId::from("a")).unwrap().record().meta;
    assert_eq!(rec["can_call"], json!({"$self": "*"}));
    assert_eq!(rec["callable_by"], json!({"$self": "*"}));
    // a call to itself passes on the baseline alone
    let out = as_agent(&k, "a", "a", json!({"type": "ping"})).await;
    assert_eq!(out.result, json!({"ok": "a", "verb": "ping"}));
}

#[tokio::test]
async fn both_ends_must_grant_the_edge() {
    let (_t, k) = mk(json!({}));
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can_call": {"b": ["ping"]}}),
    )
    .await;
    create(&k, "kernel_state", "b", json!({})).await;
    // egress granted, ingress not
    let out = as_agent(&k, "a", "b", json!({"type": "ping"})).await;
    assert_eq!(out.result["reason"], "unauthorized", "{}", out.result);
    assert_eq!(out.result["side"], "ingress", "{}", out.result);
    assert_eq!(out.result["error"], "b does not accept 'ping' from a");
    // both granted
    let _ = op(
        &k,
        "kernel",
        json!({"type": "update_agent", "id": "b", "callable_by": {"$self": "*", "a": ["ping"]}}),
    )
    .await;
    let out = as_agent(&k, "a", "b", json!({"type": "ping"})).await;
    assert_eq!(out.result, json!({"ok": "b", "verb": "ping"}));
    // a verb neither end names
    let out = as_agent(&k, "a", "b", json!({"type": "other"})).await;
    assert_eq!(out.result["side"], "egress", "{}", out.result);
    assert_eq!(out.result["error"], "a may not call 'other' on b");
    // ingress granted, egress not: b → a
    let out = as_agent(&k, "b", "a", json!({"type": "ping"})).await;
    assert_eq!(out.result["side"], "egress", "{}", out.result);
}

#[tokio::test]
async fn kernel_verbs_at_the_root_check_the_roots_callable_by() {
    let (_t, k) = mk(json!({}));
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can_call": {"kernel_state": ["ps"]}}),
    )
    .await;
    let out = as_agent(&k, "a", "kernel", json!({"type": "ps"})).await;
    assert_eq!(out.result["side"], "ingress", "{}", out.result);
    assert_eq!(
        out.result["error"],
        "kernel_state does not accept 'ps' from a"
    );
}

#[tokio::test]
async fn star_covers_create_agent() {
    let (_t, k) = mk(json!({"a": "*"}));
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can_call": {"kernel_state": "*"}}),
    )
    .await;
    let out = as_agent(
        &k,
        "a",
        "kernel",
        json!({"type": "create_agent", "parent_id": "a", "handler_module": MOD, "id": "kid"}),
    )
    .await;
    assert_eq!(out.result["id"], "kid", "{}", out.result);
    let kid = k.agents.get(&AgentId::from("kid")).unwrap();
    assert_eq!(kid.parent_id, Some(AgentId::from("a")));
}

#[tokio::test]
async fn the_mutators_act_on_the_senders_own_children_only() {
    let (_t, k) = mk(json!({"a": "*"}));
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can_call": {"kernel_state": "*"}}),
    )
    .await;
    create(&k, "kernel_state", "other", json!({})).await;
    create(&k, "a", "mine", json!({})).await;
    create(&k, "mine", "grandkid", json!({})).await;
    let refused = |verb: &str, id: &str| {
        json!({
            "error": format!("a may only {verb} its own children — {id} is not one"),
            "reason": "unauthorized",
            "side": "mutator",
        })
    };
    // create elsewhere
    let out = as_agent(
        &k,
        "a",
        "kernel",
        json!({"type": "create_agent", "parent_id": "other", "handler_module": MOD, "id": "x"}),
    )
    .await;
    assert_eq!(out.result, refused("create_agent", "other"));
    assert!(!k.agents.contains_key(&AgentId::from("x")));
    // update / delete a non-child (a sibling, a grandchild, itself)
    for id in ["other", "grandkid", "a"] {
        let out = as_agent(
            &k,
            "a",
            "kernel",
            json!({"type": "update_agent", "id": id, "description": "x"}),
        )
        .await;
        assert_eq!(out.result, refused("update_agent", id));
        let out = as_agent(&k, "a", "kernel", json!({"type": "delete_agent", "id": id})).await;
        assert_eq!(out.result, refused("delete_agent", id));
    }
    // its own child: allowed
    let out = as_agent(
        &k,
        "a",
        "kernel",
        json!({"type": "update_agent", "id": "mine", "description": "ok"}),
    )
    .await;
    assert_eq!(out.result["updated"], true, "{}", out.result);
    let out = as_agent(
        &k,
        "a",
        "kernel",
        json!({"type": "delete_agent", "id": "mine"}),
    )
    .await;
    assert_eq!(out.result["deleted"], true, "{}", out.result);
}

#[tokio::test]
async fn a_missing_operand_gets_the_verbs_own_refusal_and_an_unknown_id_the_mutator_text() {
    let (_t, k) = mk(json!({"a": "*"}));
    create(
        &k,
        "kernel_state",
        "a",
        json!({"can_call": {"kernel_state": "*"}}),
    )
    .await;
    // a missing or non-string operand is the VERB's missing/shape refusal, never
    // the mutator text — judged before the mutator rule
    for (payload, verb, arg) in [
        (
            json!({"type": "create_agent", "handler_module": MOD}),
            "create_agent",
            "parent_id",
        ),
        (
            json!({"type": "create_agent", "parent_id": 7, "handler_module": MOD}),
            "create_agent",
            "parent_id",
        ),
        (
            json!({"type": "update_agent", "description": "x"}),
            "update_agent",
            "id",
        ),
        (
            json!({"type": "update_agent", "id": 7}),
            "update_agent",
            "id",
        ),
        (json!({"type": "delete_agent"}), "delete_agent", "id"),
        (
            json!({"type": "delete_agent", "id": ""}),
            "delete_agent",
            "id",
        ),
    ] {
        let out = as_agent(&k, "a", "kernel", payload.clone()).await;
        assert_eq!(
            out.result["reason"], "missing_arg",
            "{payload}: {}",
            out.result
        );
        assert_eq!(out.result["arg"], arg, "{payload}: {}", out.result);
        assert!(
            out.result["error"]
                .as_str()
                .is_some_and(|e| e.starts_with(&format!("{verb}: "))),
            "{payload}: {}",
            out.result
        );
    }
    // an id naming no agent is not the sender's child either: the mutator text
    for verb in ["update_agent", "delete_agent"] {
        let out = as_agent(&k, "a", "kernel", json!({"type": verb, "id": "ghost"})).await;
        assert_eq!(
            out.result,
            json!({
                "error": format!("a may only {verb} its own children — ghost is not one"),
                "reason": "unauthorized",
                "side": "mutator",
            })
        );
    }
}

#[tokio::test]
async fn a_peer_address_in_callable_by_is_refused_at_the_write() {
    // A caller is always local — a call a bridge leg serves is the leg's — so a
    // `<guid>|…` key in `callable_by` could never match. In `can_call` it is an
    // outbound peer address.
    let (_t, k) = mk(json!({}));
    for key in ["g1|ai", "g1|*"] {
        let r = op(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": MOD,
                   "id": "z", "callable_by": {key: "*"}}),
        )
        .await;
        assert_eq!(r["reason"], "invalid_meta", "{key}: {r}");
        assert_eq!(
            r["error"],
            json!(format!(
                "callable_by[{}] is not a grant key — legal keys: an agent id, $self, $parent, \
                 $children",
                fantastic_bundle::py_repr(key)
            )),
            "{key}"
        );
    }
    create(
        &k,
        "kernel_state",
        "out",
        json!({"can_call": {"g1|ai": "*", "g2|*": ["ping"]}}),
    )
    .await;
}

#[tokio::test]
async fn an_unknown_grant_key_is_refused_at_the_write() {
    let (_t, k) = mk(json!({}));
    for key in ["$loader", "$subtree:a", "*", "a|b|c"] {
        let r = op(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": MOD,
                   "id": "z", "can_call": {key: "*"}}),
        )
        .await;
        assert_eq!(r["reason"], "invalid_meta", "{key}: {r}");
        assert_eq!(
            r["error"],
            json!(format!(
                "can_call[{}] is not a grant key — legal keys: an agent id, $self, $parent, \
                 $children, <guid>|<agent> or <guid>|*",
                fantastic_bundle::py_repr(key)
            )),
            "{key}"
        );
    }
    let r = op(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": MOD,
               "id": "z", "callable_by": {"a": {"verbs": ["x"]}}}),
    )
    .await;
    assert_eq!(
        r["error"],
        "callable_by['a'] must be \"*\" or a list of verb strings"
    );
}
