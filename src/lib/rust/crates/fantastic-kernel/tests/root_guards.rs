//! Guards on the ROOT and on the record fields that name substrate wiring.
//!
//! Measured 2026-09-02 (python-canonical, ported):
//!
//! - `delete_agent id=<root>` cascaded the whole tree and answered
//!   `{"deleted": true}`, leaving a process with no control surface.
//! - `update_agent` silently DROPPED `handler_module` and the reserved keys and
//!   still answered success, so a caller believed a change had landed that never
//!   did.
//! - A verb an agent cannot answer came back as a reason-less `{error: …}` which
//!   `apply_reflect_flags` then decorated with `kernel_verbs`/`tree`/`more` — a
//!   refusal wearing a reflect's clothes.

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

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_agent_refuses_the_root() {
    let (_t, kernel) = mk();
    call(
        &kernel,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "child",
               "handler_module": "noop.tools"}),
    )
    .await;
    let r = call(
        &kernel,
        "kernel",
        json!({"type": "delete_agent", "id": "kernel_state"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "id", "{r}");
    assert_eq!(r["deleted"], false, "{r}");
    // The tree is intact.
    assert!(kernel.agents.contains_key(&AgentId::from("kernel_state")));
    assert!(kernel.agents.contains_key(&AgentId::from("child")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_agent_refuses_handler_module_and_reserved_keys() {
    let (_t, kernel) = mk();
    call(
        &kernel,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "upd",
               "handler_module": "noop.tools"}),
    )
    .await;
    for (key, value) in [
        ("handler_module", json!("other.tools")),
        ("root_path", json!("/tmp/elsewhere")),
        ("ctx", json!("x")),
        ("parent", json!("y")),
    ] {
        let mut p = json!({"type": "update_agent", "id": "upd"});
        p[key] = value;
        let r = call(&kernel, "kernel", p).await;
        assert_eq!(r["reason"], "bad_arg", "{key}: {r}");
        assert_eq!(r["arg"], key, "{key}: {r}");
    }
    // handler_module unchanged.
    let rec = kernel.agents.get(&AgentId::from("upd")).unwrap();
    assert_eq!(rec.handler_module.as_deref(), Some("noop.tools"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_agent_refuses_reserved_keys() {
    let (_t, kernel) = mk();
    let r = call(
        &kernel,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "reserved",
               "handler_module": "noop.tools", "root_path": "/tmp/elsewhere"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "root_path", "{r}");
    assert!(!kernel.agents.contains_key(&AgentId::from("reserved")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_a_bare_agent_cannot_answer_is_coded() {
    let (_t, kernel) = mk();
    let r = call(&kernel, "kernel", json!({"type": "no_such_verb_at_all"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "no_such_verb_at_all", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_is_not_decorated_into_a_reflect() {
    // A record may name a bundle THIS runtime does not have (the browser's own
    // agents persist here). The call is refused with a code — and is not dressed
    // up with kernel_verbs/tree/more, which made it read like a healthy reflect.
    let (_t, kernel) = mk();
    let rec = call(
        &kernel,
        "kernel",
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "foreign",
               "handler_module": "frontend_view.tools"}),
    )
    .await;
    assert_eq!(rec["id"], "foreign", "{rec}");
    let r = call(&kernel, "foreign", json!({"type": "reflect"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "reflect", "{r}");
    for decoration in ["kernel_verbs", "tree", "more"] {
        assert!(r.get(decoration).is_none(), "{decoration} in {r}");
    }
}

/// C13: `persist_blob`/`load_blob` LEFT the kernel-verb set, so they no longer
/// bypass root ingress. A grandchild granted exactly `kernel_state:[persist_blob]`
/// is refused by the ROOT's `callable_by` — python's `namespacing` contract
/// step, which expects `{allow: false, side: "ingress"}`.
#[tokio::test]
async fn blob_verbs_pass_root_ingress() {
    let (_tmp, kernel) = mk();
    // A direct child, then a grandchild under it — `$children` on the root does
    // not reach a grandchild, so the only edge it could have is the explicit one.
    for (id, parent) in [("mid", "kernel"), ("leaf", "mid")] {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({
                    "type": "create_agent", "parent_id": parent,
                    "handler_module": "noop.tools", "id": id,
                    "can_call": {"kernel_state": ["persist_blob"]},
                }),
            )])
            .await;
    }
    let denial = fantastic_kernel::test_support::as_agent(AgentId::from("leaf"), async {
        kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "persist_blob", "agent_id": "leaf", "name": "s.yaml", "content": "x"}),
            )])
            .await
            .remove(0)
            .result
    })
    .await;
    assert_eq!(denial["reason"], json!("unauthorized"), "{denial}");
    assert_eq!(denial["side"], json!("ingress"), "{denial}");
}
