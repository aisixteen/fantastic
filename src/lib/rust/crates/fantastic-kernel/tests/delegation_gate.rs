//! The conferral gate names the TRUE reason.
//!
//! THE BATCH TRAP: `[create X, grant peer→X]` in ONE send. Items launch
//! concurrently, so X is not an agent yet when the grant's gate runs. The refusal
//! must say "no such agent" and point at the next batch — "you don't hold X" is
//! false and sent a builder into a 382-second deliberation (measured 2026-09-02,
//! E30 subai:python). The same grant in the NEXT send passes.

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

async fn create(kernel: &Arc<Kernel>, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegating_on_an_agent_that_does_not_exist_names_that() {
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

    // The peer's record is the OPERAND of the grant (AV-1): the granter names it
    // with `update_agent`, and the peer accepts the granter's edit.
    let peer = create(
        &kernel,
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "peer",
               "handler_module": "noop.tools",
               "callable_by": {"$self": "*", "$parent": "*", "$children": "*",
                               "granter": ["update_agent"]}}),
    )
    .await;
    assert_eq!(peer["id"], "peer", "{peer}");
    let granter = create(
        &kernel,
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "granter",
               "handler_module": "noop.tools",
               "can_call": {"$self": "*", "$parent": "*", "$children": "*",
                            "kernel_state": ["*", "create_agent", "update_agent"],
                            "peer": ["update_agent"]}}),
    )
    .await;
    assert_eq!(granter["id"], "granter", "{granter}");

    let (no, ok, held) = fantastic_kernel::send::with_sender(AgentId::from("granter"), async {
        let batch = kernel
            .send(vec![
                Item::new(
                    AgentId::from("kernel"),
                    json!({"type": "create_agent", "parent_id": "granter", "id": "kid",
                           "handler_module": "noop.tools"}),
                ),
                Item::new(
                    AgentId::from("kernel"),
                    json!({"type": "update_agent", "id": "peer", "can_call": {"kid": "*"}}),
                ),
            ])
            .await;
        let no = batch[1].result.clone();
        // Next batch: kid exists and is the granter's child → the grant passes.
        let ok = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "update_agent", "id": "peer", "can_call": {"kid": "*"}}),
            )])
            .await
            .remove(0)
            .result;
        // An EXISTING agent the granter does not hold: the refusal says whose child
        // it is — the diagnosis behind "create your organs UNDER YOURSELF".
        let held = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "update_agent", "id": "kid", "can_call": {"peer": "*"}}),
            )])
            .await
            .remove(0)
            .result;
        (no, ok, held)
    })
    .await;
    assert_eq!(held["side"], "delegation", "{held}");
    assert_eq!(held["parent"], "kernel_state", "{held}");
    assert!(
        held["error"].as_str().unwrap().contains("not your child"),
        "{held}"
    );

    assert_eq!(no["reason"], "unauthorized", "{no}");
    assert_eq!(no["side"], "delegation", "{no}");
    assert_eq!(no["missing"], "kid", "{no}");
    let msg = no["error"].as_str().unwrap();
    assert!(
        msg.contains("no such agent") && msg.contains("next"),
        "{no}"
    );
    // `update_agent` answers python's shape — {updated, id, agent} — so the
    // granted edge is read off `agent`, not off the reply's own top level.
    assert_eq!(ok["updated"], true, "{ok}");
    assert_eq!(ok["id"], "peer", "{ok}");
    assert_eq!(ok["agent"]["can_call"]["kid"], "*", "{ok}");
}
