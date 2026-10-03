//! `update_agent` / `delete_agent` are gated on their OPERAND (`payload.id`), the
//! way `create_agent` is gated on its placement (audit 2026-10-03, AV-1) — the
//! twin of python `tests/kernel/test_operand_gate.py`.
//!
//! Measured before the fix: an agent holding only `<root>:[update_agent,
//! delete_agent]` — the ordinary builder wiring — rewrote an unrelated
//! credential_store's refs, opened an unrelated exec_bridge's allowlist, cleared
//! another agent's `delete_lock` and deleted it. The gate asked about the ROOT
//! (which merely ANSWERS the verb) and never about the agent the verb ACTS UPON.
//! And the same gate refused an agent editing its OWN child without that root
//! grant.
//!
//! The rule: editing or deleting inside your OWN subtree (yourself included) is
//! baseline. Anywhere else needs egress NAMING the operand with the verb
//! (`<id>: ["update_agent"]` — never a wildcard, not even `<id>: "*"`) AND the
//! operand's `callable_by` accepting the sender. The operator is ungated.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

const ROOT: &str = "kernel_state";

struct Noop;

#[async_trait]
impl Bundle for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(json!({"ok": true})))
    }
}

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", Noop);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from(ROOT),
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

/// The structural baseline, restated in a grant map that names more.
fn base(extra: Value) -> Value {
    let mut m = json!({"$self": "*", "$parent": "*", "$children": "*"});
    for (k, v) in extra.as_object().expect("an object") {
        m[k] = v.clone();
    }
    m
}

/// Create a plain agent as the OPERATOR — the fixture builder.
async fn plain(kernel: &Arc<Kernel>, id: &str, parent: Option<&str>, meta: Value) -> String {
    let mut p = json!({"type": "create_agent", "parent_id": parent.unwrap_or(ROOT), "id": id,
                       "handler_module": "noop.tools"});
    for (k, v) in meta.as_object().expect("an object") {
        p[k] = v.clone();
    }
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel"), p)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], id, "{r}");
    id.to_string()
}

/// Send `payload` to the root as `sender` (`None` = the operator).
async fn as_(kernel: &Arc<Kernel>, sender: Option<&str>, payload: Value) -> Value {
    let item = Item::new(AgentId::from("kernel"), payload);
    match sender {
        None => kernel.send(vec![item]).await.remove(0).result,
        Some(s) => {
            fantastic_kernel::send::with_sender(AgentId::from(s), async {
                kernel.send(vec![item]).await.remove(0).result
            })
            .await
        }
    }
}

fn upd(id: &str, meta: Value) -> Value {
    let mut p = json!({"type": "update_agent", "id": id});
    for (k, v) in meta.as_object().expect("an object") {
        p[k] = v.clone();
    }
    p
}

fn del(id: &str) -> Value {
    json!({"type": "delete_agent", "id": id})
}

fn meta_of(kernel: &Arc<Kernel>, id: &str, field: &str) -> Value {
    kernel
        .agents
        .get(&AgentId::from(id))
        .map(|a| {
            a.meta
                .read()
                .unwrap()
                .get(field)
                .cloned()
                .unwrap_or(Value::Null)
        })
        .unwrap_or(Value::Null)
}

fn exists(kernel: &Arc<Kernel>, id: &str) -> bool {
    kernel.agents.contains_key(&AgentId::from(id))
}

fn refused(r: &Value, side: &str) -> bool {
    r["reason"] == "unauthorized" && r["side"] == side
}

// ─── the escalation ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_grant_does_not_reach_an_unrelated_agent() {
    // THE ESCALATION. `<root>:[update_agent, delete_agent]` is the builder
    // wiring; it must not be a key to every other agent's record.
    let (_t, k) = mk();
    let victim = plain(&k, "og1_victim", None, json!({"description": "before"})).await;
    let builder = plain(
        &k,
        "og1_builder",
        None,
        json!({"can_call": base(json!({ROOT: ["update_agent", "delete_agent"]}))}),
    )
    .await;
    let r = as_(
        &k,
        Some(&builder),
        upd(&victim, json!({"description": "pwned"})),
    )
    .await;
    assert!(refused(&r, "egress"), "{r}");
    assert_eq!(
        r["error"],
        format!("{builder} may not update_agent {victim}"),
        "{r}"
    );
    assert_eq!(
        r["hint"],
        format!(
            "{victim} is outside your subtree: your can_call must name it with the \
             verb — {{'{victim}': ['update_agent']}}, never a wildcard — and its \
             callable_by must accept you"
        ),
        "{r}"
    );
    assert_eq!(meta_of(&k, &victim, "description"), "before");
    let r = as_(&k, Some(&builder), del(&victim)).await;
    assert!(refused(&r, "egress"), "{r}");
    assert_eq!(
        r["error"],
        format!("{builder} may not delete_agent {victim}"),
        "{r}"
    );
    assert!(exists(&k, &victim));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_lock_on_an_unrelated_agent_cannot_be_cleared() {
    let (_t, k) = mk();
    let victim = plain(&k, "og2_victim", None, json!({"delete_lock": true})).await;
    let builder = plain(
        &k,
        "og2_builder",
        None,
        json!({"can_call": base(json!({ROOT: ["update_agent", "delete_agent"]}))}),
    )
    .await;
    let r = as_(
        &k,
        Some(&builder),
        upd(&victim, json!({"delete_lock": null})),
    )
    .await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert_eq!(meta_of(&k, &victim, "delete_lock"), true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wildcard_never_covers_the_operand() {
    // `<id>: "*"` is reach, not authority over the record: the verb is named.
    let (_t, k) = mk();
    let victim = plain(
        &k,
        "og3_victim",
        None,
        json!({"callable_by": base(json!({"og3_holder": "*"}))}),
    )
    .await;
    let holder = plain(
        &k,
        "og3_holder",
        None,
        json!({"can_call": base(json!({"og3_victim": "*"}))}),
    )
    .await;
    for payload in [upd(&victim, json!({"description": "x"})), del(&victim)] {
        let r = as_(&k, Some(&holder), payload).await;
        assert!(refused(&r, "egress"), "{r}");
    }
    let holder2 = plain(
        &k,
        "og3_holder2",
        None,
        json!({"can_call": base(json!({"og3_victim": ["*"]}))}),
    )
    .await;
    let r = as_(
        &k,
        Some(&holder2),
        upd(&victim, json!({"description": "x"})),
    )
    .await;
    assert!(refused(&r, "egress"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_structural_token_never_names_the_operand() {
    // `$parent: ["update_agent"]` names a verb, not the operand.
    let (_t, k) = mk();
    let parent = plain(&k, "og4_parent", None, json!({})).await;
    let kid = plain(
        &k,
        "og4_kid",
        Some(&parent),
        json!({"can_call": base(json!({"$parent": ["update_agent"]}))}),
    )
    .await;
    let r = as_(&k, Some(&kid), upd(&parent, json!({"description": "x"}))).await;
    assert!(refused(&r, "egress"), "{r}");
}

// ─── baseline: your own subtree ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_edits_and_deletes_its_own_subtree_with_no_grant() {
    // Before the fix this was refused (`unauthorized egress`) without a ROOT
    // grant — the only grant that existed was tree-wide.
    let (_t, k) = mk();
    let owner = plain(&k, "og5_owner", None, json!({})).await;
    let kid = plain(&k, "og5_kid", Some(&owner), json!({})).await;
    let grandkid = plain(&k, "og5_grandkid", Some(&kid), json!({})).await;
    let r = as_(
        &k,
        Some(&owner),
        upd(&grandkid, json!({"description": "mine"})),
    )
    .await;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(meta_of(&k, &grandkid, "description"), "mine");
    let r = as_(&k, Some(&owner), upd(&owner, json!({"description": "me"}))).await;
    assert_eq!(r["updated"], true, "{r}");
    let r = as_(&k, Some(&owner), del(&kid)).await;
    assert_eq!(r["deleted"], true, "{r}");
    assert!(!exists(&k, &grandkid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_subtree_keeps_the_self_grant_rule() {
    let (_t, k) = mk();
    let owner = plain(&k, "og6_owner", None, json!({})).await;
    let r = as_(
        &k,
        Some(&owner),
        upd(&owner, json!({"can_call": base(json!({ROOT: "*"}))})),
    )
    .await;
    assert!(refused(&r, "self-grant"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_subtree_keeps_the_holder_mark_rule() {
    let (_t, k) = mk();
    let owner = plain(&k, "og7_owner", None, json!({})).await;
    let vault = plain(
        &k,
        "og7_vault",
        Some(&owner),
        json!({"holds_secrets": true}),
    )
    .await;
    let r = as_(
        &k,
        Some(&owner),
        upd(&vault, json!({"holds_secrets": null})),
    )
    .await;
    assert!(refused(&r, "secret-holder"), "{r}");
    assert_eq!(meta_of(&k, &vault, "holds_secrets"), true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_subtree_keeps_the_delegation_rule() {
    let (_t, k) = mk();
    let other = plain(&k, "og8_other", None, json!({})).await;
    let owner = plain(&k, "og8_owner", None, json!({})).await;
    let kid = plain(&k, "og8_kid", Some(&owner), json!({})).await;
    let r = as_(
        &k,
        Some(&owner),
        upd(&kid, json!({"can_call": base(json!({other: ["reflect"]}))})),
    )
    .await;
    assert!(refused(&r, "delegation"), "{r}");
}

// ─── anywhere else: both halves, naming the operand ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unrelated_operand_needs_egress_naming_it_and_its_ingress() {
    let (_t, k) = mk();
    let victim = plain(&k, "og9_victim", None, json!({"description": "before"})).await;
    let editor = plain(
        &k,
        "og9_editor",
        None,
        json!({"can_call": base(json!({"og9_victim": ["update_agent", "delete_agent"]}))}),
    )
    .await;
    // egress names it; the operand has not accepted the editor yet
    let r = as_(&k, Some(&editor), upd(&victim, json!({"description": "x"}))).await;
    assert!(refused(&r, "ingress"), "{r}");
    assert_eq!(
        r["error"],
        format!("{victim} does not accept 'update_agent' from {editor}"),
        "{r}"
    );
    let r = as_(&k, Some(&editor), del(&victim)).await;
    assert!(refused(&r, "ingress"), "{r}");
    // the operator opens the operand's ingress → both halves granted
    as_(
        &k,
        None,
        upd(
            &victim,
            json!({"callable_by": base(json!({"og9_editor": ["update_agent", "delete_agent"]}))}),
        ),
    )
    .await;
    let r = as_(
        &k,
        Some(&editor),
        upd(&victim, json!({"description": "after"})),
    )
    .await;
    assert_eq!(r["updated"], true, "{r}");
    let r = as_(&k, Some(&editor), del(&victim)).await;
    assert_eq!(r["deleted"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_record_is_an_operand_like_any_other() {
    // `<root>: ["update_agent"]` now means what it says: edit the ROOT.
    let (_t, k) = mk();
    let editor = plain(
        &k,
        "og10_editor",
        None,
        json!({"can_call": base(json!({ROOT: ["update_agent"]}))}),
    )
    .await;
    // the root's baseline callable_by accepts its direct children
    let r = as_(
        &k,
        Some(&editor),
        upd(ROOT, json!({"description": "root edited"})),
    )
    .await;
    assert_eq!(r["updated"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heritable_entry_naming_the_operand_reaches_it() {
    let (_t, k) = mk();
    let victim = plain(
        &k,
        "og11_victim",
        None,
        json!({"callable_by": base(json!({"og11_kid": ["*"]}))}),
    )
    .await;
    let hub = plain(
        &k,
        "og11_hub",
        None,
        json!({"can_call": base(json!({
            "og11_victim": {"verbs": ["update_agent"], "for": "descendants"}
        }))}),
    )
    .await;
    let kid = plain(&k, "og11_kid", Some(&hub), json!({})).await;
    let r = as_(
        &k,
        Some(&kid),
        upd(&victim, json!({"description": "via hub"})),
    )
    .await;
    assert_eq!(r["updated"], true, "{r}");
    // the holder itself is never granted by its heritable entry
    let r = as_(&k, Some(&hub), upd(&victim, json!({"description": "x"}))).await;
    assert!(refused(&r, "egress"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_is_ungated() {
    let (_t, k) = mk();
    let victim = plain(&k, "og12_victim", None, json!({})).await;
    let r = as_(&k, None, upd(&victim, json!({"description": "op"}))).await;
    assert_eq!(r["updated"], true, "{r}");
    let r = as_(&k, None, del(&victim)).await;
    assert_eq!(r["deleted"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_operand_is_answered_by_the_verb() {
    // No operand to gate: the root grant decides, and the verb says what is
    // wrong (as an unknown `parent_id` does for create_agent).
    let (_t, k) = mk();
    let builder = plain(
        &k,
        "og13_builder",
        None,
        json!({"can_call": base(json!({ROOT: ["update_agent"]}))}),
    )
    .await;
    let r = as_(
        &k,
        Some(&builder),
        upd("og13_nobody", json!({"description": "x"})),
    )
    .await;
    assert_eq!(r["reason"], "unknown_agent", "{r}");
}

// ─── a relayed foreign caller owns no subtree here (audit G-1) ───────────────

/// Send `payload` to the root as bridge leg `leg` relaying the foreign `caller`.
async fn relayed(kernel: &Arc<Kernel>, leg: &str, caller: &str, payload: Value) -> Value {
    let item = Item::new(AgentId::from("kernel"), payload);
    fantastic_kernel::send::with_sender(
        AgentId::from(leg),
        fantastic_kernel::send::with_remote_caller(AgentId::from(leg), caller.to_string(), async {
            kernel.send(vec![item]).await.remove(0).result
        }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relayed_caller_does_not_inherit_the_legs_subtree() {
    // Measured before the fix: a foreign caller a leg relays — refused `reflect`
    // on the leg's child — edited and deleted that child and re-pinned the leg's
    // own record: the gate took the LEG's own-subtree baseline on its behalf.
    let (_t, k) = mk();
    let leg = plain(&k, "og14_leg", None, json!({"peer_guid": "g9"})).await;
    let kid = plain(&k, "og14_kid", Some(&leg), json!({"description": "before"})).await;
    for payload in [
        upd(&kid, json!({"description": "pwned"})),
        del(&kid),
        upd(&leg, json!({"peer_guid": "g0"})),
    ] {
        let r = relayed(&k, &leg, "g9|mallory", payload.clone()).await;
        assert!(refused(&r, "egress"), "{r}");
        assert_eq!(
            r["error"],
            format!(
                "{leg} may not {} {}",
                payload["type"].as_str().unwrap(),
                payload["id"].as_str().unwrap()
            ),
            "{r}"
        );
    }
    assert_eq!(meta_of(&k, &kid, "description"), "before");
    assert_eq!(meta_of(&k, &leg, "peer_guid"), "g9");
    // the leg on its OWN behalf keeps its baseline
    let r = as_(&k, Some(&leg), upd(&kid, json!({"description": "mine"}))).await;
    assert_eq!(r["updated"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relayed_caller_is_judged_on_the_operand_as_its_qualified_id() {
    // Egress is the leg's `can_call` naming the operand; ingress is the
    // operand's `callable_by` against `<guid>|<agent>` — accepting the leg (the
    // child's `$parent`) is not accepting its peer.
    let (_t, k) = mk();
    let leg = plain(
        &k,
        "og15_leg",
        None,
        json!({"can_call": base(json!({"og15_kid": ["update_agent"]}))}),
    )
    .await;
    let kid = plain(&k, "og15_kid", Some(&leg), json!({"description": "before"})).await;
    let r = relayed(&k, &leg, "g9|scout", upd(&kid, json!({"description": "x"}))).await;
    assert!(refused(&r, "ingress"), "{r}");
    assert_eq!(
        r["error"],
        format!("{kid} does not accept 'update_agent' from g9|scout"),
        "{r}"
    );
    as_(
        &k,
        None,
        upd(
            &kid,
            json!({"callable_by": base(json!({"g9|scout": ["update_agent"]}))}),
        ),
    )
    .await;
    let r = relayed(
        &k,
        &leg,
        "g9|scout",
        upd(&kid, json!({"description": "after"})),
    )
    .await;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(meta_of(&k, &kid, "description"), "after");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relayed_caller_needs_the_roots_callable_by_on_kernel_verbs() {
    // Measured before the fix (audit 2026-10-03, AV-3): kernel verbs at the root
    // skip ingress, so a foreign caller relayed by a direct child of the root got
    // `list_agents` on the leg's `$parent:*` alone while `reflect` on the same
    // root was refused. The root's `callable_by` must accept `<guid>|<agent>` or
    // `<guid>|*`; a local caller is unchanged.
    let (_t, k) = mk();
    let leg = plain(
        &k,
        "og16_leg",
        None,
        json!({"can_call": base(json!({ROOT: ["ps"]}))}),
    )
    .await;
    for verb in ["list_agents", "ps"] {
        let r = relayed(&k, &leg, "g9|mallory", json!({"type": verb})).await;
        assert!(refused(&r, "ingress"), "{r}");
        assert_eq!(
            r["error"],
            format!("{ROOT} does not accept '{verb}' from g9|mallory"),
            "{r}"
        );
        assert_eq!(
            r["hint"],
            format!(
                "a foreign caller is accepted only by name: callable_by \
                 {{'g9|mallory': [...]}} or {{'g9|*': [...]}} on {ROOT}"
            ),
            "{r}"
        );
        // the leg on its OWN behalf: egress is the whole check, as before
        let r = as_(&k, Some(&leg), json!({"type": verb})).await;
        assert!(r.get("error").is_none(), "{r}");
    }
    let mut root_cb = meta_of(&k, ROOT, "callable_by");
    if root_cb.is_null() {
        root_cb = json!({});
    }
    root_cb["g9|*"] = json!(["list_agents"]);
    root_cb["g9|mallory"] = json!(["ps"]);
    as_(&k, None, upd(ROOT, json!({"callable_by": root_cb}))).await;
    let r = relayed(&k, &leg, "g9|mallory", json!({"type": "list_agents"})).await;
    assert!(r.get("error").is_none() && r.get("agents").is_some(), "{r}");
    let r = relayed(&k, &leg, "g9|mallory", json!({"type": "ps"})).await;
    assert!(r.get("error").is_none(), "{r}");
    // a guid the root does not name is still refused
    let r = relayed(&k, &leg, "g7|eve", json!({"type": "list_agents"})).await;
    assert!(refused(&r, "ingress"), "{r}");
}
