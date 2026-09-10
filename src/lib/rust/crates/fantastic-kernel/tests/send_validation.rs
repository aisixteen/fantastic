//! `send` validates the ITEM before anything is resolved, gated or minted.
//!
//! Measured 2026-09-02 (python, ported): a payload without `type` reached the
//! gate and the target instead of being a coded refusal; a non-string `type`
//! raised out of `send`; rust/swift minted an id when the caller supplied `""`.

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
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (tmp, kernel)
}

async fn create(kernel: &Arc<Kernel>, id: &str, extra: Value) -> Value {
    let mut p = json!({"type": "create_agent", "parent_id": "core", "id": id,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_type_is_missing_arg_before_gate() {
    let (_t, kernel) = mk();
    create(&kernel, "a", json!({})).await;
    create(&kernel, "b", json!({})).await; // no edge a→b: the gate WOULD refuse
    let out = fantastic_kernel::send::with_sender(AgentId::from("a"), async {
        kernel
            .send(vec![Item::new(AgentId::from("b"), json!({}))])
            .await
            .remove(0)
    })
    .await;
    assert!(out.pid.is_none(), "{out:?}");
    assert_eq!(out.result["reason"], "missing_arg", "{}", out.result);
    assert_eq!(out.result["arg"], "payload.type");
    assert!(
        out.result.get("side").is_none(),
        "validated BEFORE the gate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_string_type_is_bad_arg() {
    let (_t, kernel) = mk();
    create(&kernel, "a", json!({})).await;
    for bad in [json!(["x"]), json!(7), json!(""), json!(null)] {
        let out = kernel
            .send(vec![Item::new(AgentId::from("a"), json!({"type": bad}))])
            .await
            .remove(0);
        assert_eq!(out.result["reason"], "bad_arg", "{bad}: {}", out.result);
        assert_eq!(out.result["arg"], "payload.type");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_id_is_refused_bad_arg() {
    let (_t, kernel) = mk();
    create(&kernel, "a", json!({})).await;
    let out = kernel
        .send(vec![
            Item::new(AgentId::from("a"), json!({"type": "poke"})).with_id("")
        ])
        .await
        .remove(0);
    assert!(out.pid.is_none(), "{out:?}");
    assert_eq!(out.result["reason"], "bad_arg", "{}", out.result);
    assert_eq!(out.result["arg"], "id");
    // AND NOTHING ELSE IS REFUSED. There is no minted namespace to trespass in
    // any more: a caller supplies a NAME, the kernel supplies every id, so a
    // name that happens to look like an old pid runs like any other.
    let out = kernel
        .send(vec![
            Item::new(AgentId::from("a"), json!({"type": "poke"})).with_id("a:deadbeef")
        ])
        .await
        .remove(0);
    assert!(out.pid.is_some(), "{}", out.result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_agent_with_empty_id_is_refused() {
    let (_t, kernel) = mk();
    let before = kernel.agents.len();
    let r = create(&kernel, "", json!({})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "id");
    assert_eq!(kernel.agents.len(), before, "no agent minted for \"\"");
}
