//! `create_agent` is gated on the PLACEMENT TARGET, and `$loader` is real reach.
//!
//! Both measured 2026-09-02 against the live python kernel and ported here:
//!
//! - An agent holding the ordinary "may author tools" wiring
//!   (`<root>:[create_agent]`) could plant a child under ANY agent and the child's
//!   stamped `$parent:*` baseline handed it unrestricted reach into that agent. The
//!   gate asked about the ROOT (which merely ANSWERS the verb) instead of the
//!   parent (which is ACTED UPON).
//! - `check_conferral` skipped every `$`-prefixed key as "structural", but
//!   `$loader` resolves to the ROOT for whoever holds it, so conferring it handed
//!   over the kernel's control surface.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

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

/// Create as the OPERATOR (ungated) — the fixture builder.
async fn seed(kernel: &Arc<Kernel>, id: &str, parent: &str, extra: Value) -> Value {
    let mut p = json!({"type": "create_agent", "parent_id": parent, "id": id,
                       "handler_module": "noop.tools"});
    if let Some(m) = extra.as_object() {
        for (k, v) in m {
            p[k] = v.clone();
        }
    }
    kernel
        .send(vec![Item::new(AgentId::from("kernel"), p)])
        .await
        .remove(0)
        .result
}

/// Create AS `sender` — the gated path under test.
async fn create_as(kernel: &Arc<Kernel>, sender: &str, payload: Value) -> Value {
    let mut p = json!({"type": "create_agent", "handler_module": "noop.tools"});
    if let Some(m) = payload.as_object() {
        for (k, v) in m {
            p[k] = v.clone();
        }
    }
    fantastic_kernel::send::with_sender(AgentId::from(sender), async {
        kernel
            .send(vec![Item::new(AgentId::from("kernel"), p)])
            .await
            .remove(0)
            .result
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_creates_under_itself_with_no_grant() {
    let (_t, kernel) = mk();
    seed(&kernel, "mid", "kernel_state", json!({})).await;
    seed(&kernel, "deep", "mid", json!({})).await; // a GRANDCHILD
    let r = create_as(&kernel, "deep", json!({"parent_id": "deep", "id": "kid"})).await;
    assert_eq!(r["id"], "kid", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creating_under_own_descendant_is_baseline() {
    let (_t, kernel) = mk();
    seed(&kernel, "mid", "kernel_state", json!({})).await;
    seed(&kernel, "kid", "mid", json!({})).await;
    let r = create_as(
        &kernel,
        "mid",
        json!({"parent_id": "kid", "id": "grandkid"}),
    )
    .await;
    assert_eq!(r["id"], "grandkid", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_create_grant_does_not_reach_a_sibling() {
    // THE ESCALATION. `<root>:[create_agent]` is the ordinary authoring grant; it
    // must not be a key to every other agent's subtree.
    let (_t, kernel) = mk();
    seed(&kernel, "victim", "kernel_state", json!({})).await;
    seed(
        &kernel,
        "author",
        "kernel_state",
        json!({"can_call": {"$self": "*", "$parent": "*", "$children": "*",
                            "kernel_state": ["*", "create_agent", "update_agent"]}}),
    )
    .await;
    let r = create_as(
        &kernel,
        "author",
        json!({"parent_id": "victim", "id": "mole"}),
    )
    .await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert_eq!(r["side"], "egress", "{r}");
    assert!(r["error"].as_str().unwrap().contains("create under"), "{r}");
    assert!(!kernel.agents.contains_key(&AgentId::from("mole")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creating_under_an_unrelated_agent_needs_both_halves() {
    let (_t, kernel) = mk();
    seed(&kernel, "victim", "kernel_state", json!({})).await;
    seed(
        &kernel,
        "author",
        "kernel_state",
        json!({"can_call": {"$self": "*", "$parent": "*", "$children": "*",
                            "victim": ["create_agent"]}}),
    )
    .await;
    // Egress granted, ingress not yet: the parent must accept a child.
    let r = create_as(&kernel, "author", json!({"parent_id": "victim", "id": "a"})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert_eq!(r["side"], "ingress", "{r}");
    // Open the parent's ingress → the edge exists.
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "victim",
                   "callable_by": {"$self": "*", "$parent": "*", "$children": "*",
                                   "author": ["create_agent"]}}),
        )])
        .await;
    let ok = create_as(&kernel, "author", json!({"parent_id": "victim", "id": "b"})).await;
    assert_eq!(ok["id"], "b", "{ok}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loader_token_in_a_conferred_grant_is_bounded() {
    let (_t, kernel) = mk();
    seed(&kernel, "mid", "kernel_state", json!({})).await;
    seed(
        &kernel,
        "deep",
        "mid",
        json!({"can_call": {"$self": "*", "$parent": "*", "$children": "*",
                            "kernel_state": ["create_agent"]}}),
    )
    .await;
    // The granter itself cannot reach the root's task table…
    let denied = fantastic_kernel::send::with_sender(AgentId::from("deep"), async {
        kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "ps"}),
            )])
            .await
            .remove(0)
            .result
    })
    .await;
    assert_eq!(denied["reason"], "unauthorized", "{denied}");
    // …so it cannot hand that reach to a child either.
    let r = create_as(
        &kernel,
        "deep",
        json!({"parent_id": "deep", "id": "kid", "can_call": {"$loader": "*"}}),
    )
    .await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert_eq!(r["side"], "delegation", "{r}");
    assert!(r["error"].as_str().unwrap().contains("$loader"), "{r}");
    assert!(!kernel.agents.contains_key(&AgentId::from("kid")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_granter_that_holds_the_root_may_confer_loader() {
    let (_t, kernel) = mk();
    seed(
        &kernel,
        "holder",
        "kernel_state",
        json!({"can_call": {"$self": "*", "$parent": "*", "$children": "*", "kernel_state": "*"}}),
    )
    .await;
    let r = create_as(
        &kernel,
        "holder",
        json!({"parent_id": "holder", "id": "kid", "can_call": {"$loader": "*"}}),
    )
    .await;
    assert_eq!(r["id"], "kid", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_structural_token_is_refused() {
    let (_t, kernel) = mk();
    seed(&kernel, "a", "kernel_state", json!({})).await;
    let r = create_as(
        &kernel,
        "a",
        json!({"parent_id": "a", "id": "kid", "can_call": {"$everything": "*"}}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["side"], "delegation", "{r}");
    assert!(r["error"].as_str().unwrap().contains("$everything"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn placement_for_a_relayed_caller_is_judged_as_anywhere_else() {
    // A foreign caller a bridge leg relays owns no subtree here (audit
    // 2026-10-03, G-1): placing under the leg or its child takes the path any
    // other placement takes — the leg's egress, then the parent's ingress
    // against `<guid>|<agent>`.
    let (_t, kernel) = mk();
    seed(&kernel, "leg", "kernel_state", json!({})).await;
    seed(&kernel, "kid", "leg", json!({})).await;
    let relayed = |parent: &'static str, id: &'static str| {
        let kernel = Arc::clone(&kernel);
        async move {
            let p = json!({"type": "create_agent", "parent_id": parent, "id": id,
                           "handler_module": "noop.tools"});
            fantastic_kernel::send::with_sender(
                AgentId::from("leg"),
                fantastic_kernel::send::with_remote_caller(
                    AgentId::from("leg"),
                    "g9|scout".to_string(),
                    async move {
                        kernel
                            .send(vec![Item::new(AgentId::from("kernel"), p)])
                            .await
                            .remove(0)
                            .result
                    },
                ),
            )
            .await
        }
    };
    let r = relayed("kid", "mole").await;
    assert_eq!(
        r,
        json!({"error": "leg may not create under kid", "reason": "unauthorized",
               "hint": fantastic_kernel::authz::FROM_ABOVE, "side": "egress"}),
        "{r}"
    );
    let r = relayed("leg", "mole").await;
    assert_eq!(
        r,
        json!({"error": "leg does not accept a child from g9|scout", "reason": "unauthorized",
               "hint": fantastic_kernel::authz::FROM_ABOVE, "side": "ingress"}),
        "{r}"
    );
    assert!(!kernel.agents.contains_key(&AgentId::from("mole")));
    // the parent naming the peer opens it
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "leg",
                   "callable_by": {"$self": "*", "$parent": "*", "$children": "*",
                                   "g9|scout": ["create_agent"]}}),
        )])
        .await;
    let ok = relayed("leg", "planted").await;
    assert_eq!(ok["id"], "planted", "{ok}");
}
