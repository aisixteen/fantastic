//! The SECRET-HOLDER MARK's two kernel rules, pinned WHOLE (owner 2026-10-02) —
//! the twin of python `tests/kernel/test_holder_mark.py`.
//!
//! `holds_secrets: true` makes an agent unreachable from a model turn (ai_core).
//! The kernel owns the field itself:
//!   - SHAPE, at the write, on every agent: exactly `true`, or `null` (the clear) —
//!     anything else is `invalid_meta`, at create_agent and update_agent alike;
//!   - WHO LOWERS IT, in the gate: on a holder, only the operator writes anything
//!     but `true` — an agent granted `update_agent` cannot unmark a holder and then
//!     call it.
//!
//! Order is fixed: the gate (who) → the kernel's shape checks → the bundle's own
//! `validate_meta`.

use async_trait::async_trait;
use fantastic_kernel::{
    Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Outcome, Reply, Status,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

const MOD: &str = "holder_mark_probe.tools";

/// python's probe module: answers `{ok: <id>, verb}`, reflects as a plain agent.
struct Probe;

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "holder_mark_probe"
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
            "reflect" => Some(json!({"id": id.as_str(), "sentence": "holder probe", "verbs": {}})),
            _ => Some(json!({"ok": id.as_str(), "verb": verb})),
        })
    }
}

/// AN AGENT EDITS A RECORD IT NAMES (AV-1): `update_agent` is gated on its
/// operand — egress naming it with the verb, and the operand's ingress accepting
/// the editor.
fn updater() -> Value {
    json!({"vault": ["update_agent"], "plain": ["update_agent"]})
}

fn accepts_a() -> Value {
    json!({"a": ["update_agent"]})
}

fn unmark(sender: &str, target: &str) -> Value {
    json!({
        "error": format!(
            "{sender} may not clear holds_secrets on {target}: it holds secrets, \
             and only the operator lowers that mark"
        ),
        "reason": "unauthorized",
        "side": "secret-holder",
        "hint": "only the operator clears it (update_agent holds_secrets=null); an agent \
                 may mark a holder, never unmark one",
    })
}

fn shape(got: &str) -> String {
    format!(
        "holds_secrets must be exactly true (this agent holds secrets) or null \
         (the operator clears the mark) — got {got}"
    )
}

/// Every non-true, non-null value, with the type name the refusal reports.
fn bad() -> Vec<(Value, &'static str)> {
    vec![
        (json!(false), "bool"),
        (json!("yes"), "str"),
        (json!("true"), "str"),
        (json!(1), "int"),
        (json!(0), "int"),
        (json!(1.5), "float"),
        (json!([true]), "list"),
        (json!({"v": true}), "dict"),
    ]
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// python's `seeded_kernel`: a root `kernel_state` and the probe bundle.
fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register(MOD, Probe);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (tmp, kernel)
}

/// python `_op`: the OPERATOR's call to `kernel`, its result.
async fn op(kernel: &Arc<Kernel>, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result
}

/// python `_as`: `sender`'s call to `kernel`, the whole outcome.
async fn as_agent(kernel: &Arc<Kernel>, sender: &str, payload: Value) -> Outcome {
    let k = Arc::clone(kernel);
    fantastic_kernel::send::with_sender(AgentId::from(sender), async move {
        k.send(vec![Item::new(AgentId::from("kernel"), payload)])
            .await
            .remove(0)
    })
    .await
}

/// python `k.create(MOD, id=…, **meta)` — the operator's create under the root.
async fn create(kernel: &Arc<Kernel>, id: &str, meta: Value) {
    let mut p = json!({"type": "create_agent", "parent_id": "kernel_state",
                       "handler_module": MOD, "id": id});
    for (k, v) in meta.as_object().cloned().unwrap_or_default() {
        p[k] = v;
    }
    let r = op(kernel, p).await;
    assert_eq!(r["id"], json!(id), "{r}");
}

/// python `k.get(id)` — the agent's record meta.
fn meta(kernel: &Kernel, id: &str) -> Map<String, Value> {
    kernel
        .agents
        .get(&AgentId::from(id))
        .expect("a live agent")
        .record()
        .meta
}

fn update(id: &str, field: &str, v: Value) -> Value {
    let mut p = json!({"type": "update_agent", "id": id});
    p[field] = v;
    p
}

// ─── K1: only the operator lowers the mark ───────────────────────────────────

#[tokio::test]
async fn an_agent_cannot_clear_the_mark_on_a_holder() {
    let (_t, k) = mk();
    create(
        &k,
        "vault",
        json!({"holds_secrets": true, "callable_by": accepts_a()}),
    )
    .await;
    create(&k, "a", json!({"can_call": updater()})).await;
    let mut values = vec![Value::Null];
    values.extend(bad().into_iter().map(|(v, _)| v));
    for v in values {
        let out = as_agent(&k, "a", update("vault", "holds_secrets", v.clone())).await;
        assert!(
            out.pid.is_none() && out.status == Status::Failed,
            "{v}: {:?} {}",
            out.status,
            out.result
        );
        assert_eq!(out.result, unmark("a", "vault"), "{v}");
        assert_eq!(keys(&out.result), ["error", "reason", "side", "hint"]);
    }
    assert_eq!(meta(&k, "vault")["holds_secrets"], json!(true));
}

/// The whole update is refused — a field beside the clear is not written.
#[tokio::test]
async fn the_refusal_wins_over_every_other_field_in_the_patch() {
    let (_t, k) = mk();
    create(
        &k,
        "vault",
        json!({"holds_secrets": true, "callable_by": accepts_a()}),
    )
    .await;
    create(&k, "a", json!({"can_call": updater()})).await;
    let r = as_agent(
        &k,
        "a",
        json!({"type": "update_agent", "id": "vault", "holds_secrets": null,
               "description": "unmarked"}),
    )
    .await
    .result;
    assert_eq!(r, unmark("a", "vault"));
    assert_eq!(meta(&k, "vault").get("description"), None);
}

#[tokio::test]
async fn a_holder_cannot_clear_its_own_mark() {
    let (_t, k) = mk();
    create(
        &k,
        "vault",
        json!({"holds_secrets": true, "can_call": updater()}),
    )
    .await;
    let r = as_agent(&k, "vault", update("vault", "holds_secrets", Value::Null))
        .await
        .result;
    assert_eq!(r, unmark("vault", "vault"));
    assert_eq!(meta(&k, "vault")["holds_secrets"], json!(true));
}

/// `true` passes from anywhere — on a plain agent it marks it, on a holder it is
/// the value already there. Other fields of a holder stay writable.
#[tokio::test]
async fn an_agent_may_mark_but_never_unmark() {
    let (_t, k) = mk();
    create(&k, "plain", json!({"callable_by": accepts_a()})).await;
    create(
        &k,
        "vault",
        json!({"holds_secrets": true, "callable_by": accepts_a()}),
    )
    .await;
    create(&k, "a", json!({"can_call": updater()})).await;
    for target in ["plain", "vault"] {
        let out = as_agent(&k, "a", update(target, "holds_secrets", json!(true))).await;
        assert_eq!(out.result["updated"], json!(true), "{}", out.result);
        assert_eq!(meta(&k, target)["holds_secrets"], json!(true));
    }
    let out = as_agent(&k, "a", update("vault", "description", json!("d"))).await;
    assert_eq!(out.result["updated"], json!(true), "{}", out.result);
    let r = as_agent(&k, "a", update("plain", "holds_secrets", Value::Null))
        .await
        .result;
    assert_eq!(
        r,
        unmark("a", "plain"),
        "once marked, the agent cannot take it back"
    );
}

/// The rule guards a HOLDER. `null` on an agent with no mark changes nothing it
/// protects, so it is not refused.
#[tokio::test]
async fn null_on_an_unmarked_agent_is_not_a_clear() {
    let (_t, k) = mk();
    create(&k, "plain", json!({"callable_by": accepts_a()})).await;
    create(&k, "a", json!({"can_call": updater()})).await;
    let out = as_agent(&k, "a", update("plain", "holds_secrets", Value::Null)).await;
    assert_eq!(out.result["updated"], json!(true), "{}", out.result);
}

/// A non-operator's malformed value on a HOLDER gets the gate's refusal; on a
/// plain agent, the shape's.
#[tokio::test]
async fn the_gate_judges_before_the_shape() {
    let (_t, k) = mk();
    create(&k, "plain", json!({"callable_by": accepts_a()})).await;
    create(
        &k,
        "vault",
        json!({"holds_secrets": true, "callable_by": accepts_a()}),
    )
    .await;
    create(&k, "a", json!({"can_call": updater()})).await;
    let on_holder = as_agent(&k, "a", update("vault", "holds_secrets", json!("yes")))
        .await
        .result;
    assert_eq!(on_holder, unmark("a", "vault"));
    let on_plain = as_agent(&k, "a", update("plain", "holds_secrets", json!("yes")))
        .await
        .result;
    assert_eq!(
        on_plain,
        json!({"error": shape("str"), "reason": "invalid_meta", "id": "plain"})
    );
}

/// `holds_secrets: null` is the clear: update_agent REPLACES the named field, so
/// the record carries `null` — not a holder (only exactly `true` marks one).
#[tokio::test]
async fn the_operator_clears_the_mark_with_null() {
    let (_t, k) = mk();
    create(
        &k,
        "vault",
        json!({"holds_secrets": true, "callable_by": accepts_a()}),
    )
    .await;
    let out = op(&k, update("vault", "holds_secrets", Value::Null)).await;
    assert_eq!(out["updated"], json!(true), "{out}");
    assert_eq!(
        out["agent"].get("holds_secrets"),
        Some(&Value::Null),
        "{out}"
    );
    assert_eq!(meta(&k, "vault").get("holds_secrets"), Some(&Value::Null));
    // cleared means cleared: an agent's `null` is no longer refused
    create(&k, "a", json!({"can_call": updater()})).await;
    let again = as_agent(&k, "a", update("vault", "holds_secrets", Value::Null)).await;
    assert_eq!(again.result["updated"], json!(true), "{}", again.result);
}

// ─── K2: the shape, at the write, on every agent ─────────────────────────────

#[tokio::test]
async fn every_non_true_value_is_refused_at_update() {
    let (_t, k) = mk();
    create(&k, "plain", json!({"callable_by": accepts_a()})).await;
    for (v, got) in bad() {
        let r = op(&k, update("plain", "holds_secrets", v.clone())).await;
        assert_eq!(
            r,
            json!({"error": shape(got), "reason": "invalid_meta", "id": "plain"}),
            "{v}"
        );
        assert_eq!(keys(&r), ["error", "reason", "id"]);
    }
    assert!(!meta(&k, "plain").contains_key("holds_secrets"));
}

#[tokio::test]
async fn every_non_true_value_is_refused_at_create() {
    let (_t, k) = mk();
    for (i, (v, got)) in bad().into_iter().enumerate() {
        let aid = format!("made_{i}");
        let r = op(
            &k,
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": MOD, "id": aid, "holds_secrets": v}),
        )
        .await;
        assert_eq!(
            r,
            json!({"error": shape(got), "reason": "invalid_meta"}),
            "{v}"
        );
        assert!(!k.agents.contains_key(&AgentId::from(aid.as_str())));
    }
}

/// The shape check is the write's, not the operator's: an agent forking its own
/// subtree is refused the same way.
#[tokio::test]
async fn an_agent_create_is_shape_checked_too() {
    let (_t, k) = mk();
    create(&k, "a", json!({})).await;
    let r = as_agent(
        &k,
        "a",
        json!({"type": "create_agent", "parent_id": "a", "handler_module": MOD,
               "id": "kid", "holds_secrets": false}),
    )
    .await
    .result;
    assert_eq!(r, json!({"error": shape("bool"), "reason": "invalid_meta"}));
    assert!(!k.agents.contains_key(&AgentId::from("kid")));
}

#[tokio::test]
async fn true_and_null_are_legal_at_create() {
    let (_t, k) = mk();
    for (aid, v) in [("marked", json!(true)), ("unmarked", Value::Null)] {
        let r = op(
            &k,
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": MOD, "id": aid, "holds_secrets": v.clone()}),
        )
        .await;
        assert_eq!(r["id"], json!(aid), "{r}");
        assert_eq!(meta(&k, aid).get("holds_secrets"), Some(&v));
    }
}
