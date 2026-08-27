//! Unit tests for [`crate::lifecycle`].

use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingBundle {
    deletes: Arc<AtomicUsize>,
}
#[async_trait]
impl crate::Bundle for CountingBundle {
    fn name(&self) -> &str {
        "counting"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _k: &Arc<Kernel>,
    ) -> Result<crate::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
    async fn on_delete(
        &self,
        _id: &AgentId,
        _k: &Arc<Kernel>,
    ) -> Result<(), crate::bundle::BundleError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn mk_kernel(deletes: Arc<AtomicUsize>) -> Arc<Kernel> {
    mk_kernel_with_storage(deletes, crate::storage::StorageMode::InMemory)
}

fn mk_kernel_with_storage(
    deletes: Arc<AtomicUsize>,
    storage: crate::storage::StorageMode,
) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(storage.clone());
    kernel
        .bundles
        .register("counting.tools", CountingBundle { deletes });
    // Disk mode persists THROUGH a provider — register the fake `file_bridge`
    // store rooted at the workdir so a test can wire one (see `wire_fake_store`).
    if let Some(wd) = storage.workdir() {
        crate::test_support::register_fake_store(&mut kernel.bundles, wd);
    }
    Arc::new(kernel)
}

#[tokio::test]
async fn mint_id_format_is_bundle_underscore_hex6() {
    let id = mint_id("file_bridge.tools");
    assert!(id.starts_with("file_bridge_"));
    // 6 hex chars after the final underscore (the bundle name itself
    // contains an underscore, so split off the trailing token).
    let suffix = &id["file_bridge_".len()..];
    assert_eq!(suffix.len(), 6);
    assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn create_then_delete_unregisters_and_calls_hook() {
    let tmp = tempfile::TempDir::new().unwrap();
    let deletes = Arc::new(AtomicUsize::new(0));
    // Disk-backed kernel so persistence::persist actually writes per-agent
    // agent.json files (the assertions below rely on that).
    let kernel = mk_kernel_with_storage(
        Arc::clone(&deletes),
        crate::storage::StorageMode::Disk(tmp.path().to_path_buf()),
    );
    // Stand up a root pointing at the tempdir.
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // Wire the persistence provider (rooted at the workdir) so records land on
    // disk — persistence is provider-routed now, RAM without one.
    crate::test_support::wire_fake_store(&kernel, tmp.path()).await;

    // Create one child via the system verb.
    let v = kernel
        .send(vec![Item::new(AgentId::from("core"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "kid_1"}))])
        .await.remove(0).result;
    assert_eq!(v["id"], "kid_1");
    assert!(kernel.agents.contains_key(&AgentId::from("kid_1")));
    assert!(tmp.path().join("agents/kid_1/agent.json").exists());

    // Delete it.
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "delete_agent", "id": "kid_1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["deleted"], true);
    assert!(!kernel.agents.contains_key(&AgentId::from("kid_1")));
    assert_eq!(deletes.load(Ordering::SeqCst), 1);
    assert!(!tmp.path().join("agents/kid_1").exists());
}

#[tokio::test]
async fn delete_refuses_locked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_kernel(Arc::new(AtomicUsize::new(0)));
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let _ = kernel
        .send(vec![Item::new(AgentId::from("core"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "locked_1", "delete_lock": true}))])
        .await.remove(0).result;
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "delete_agent", "id": "locked_1"}),
        )])
        .await
        .remove(0)
        .result;
    // #27: the machine field, not the ad-hoc flag it replaced.
    assert_eq!(v["reason"], "locked");
    assert!(kernel.agents.contains_key(&AgentId::from("locked_1")));
}

#[tokio::test]
async fn cascade_delete_fires_hooks_depth_first() {
    let tmp = tempfile::TempDir::new().unwrap();
    let deletes = Arc::new(AtomicUsize::new(0));
    let kernel = mk_kernel(Arc::clone(&deletes));
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let _ = kernel
        .send(vec![Item::new(AgentId::from("core"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "counting.tools", "id": "p_1"}))])
        .await.remove(0).result;
    // PLACEMENT IS `parent_id`, not the target. The verb is kernel-owned, so
    // these go to the root; the id names the OWNER of the new agent. Addressing
    // `p_1` used to make it the parent implicitly — that is the ambiguity #23
    // removed.
    for kid in ["c_1", "c_2"] {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "create_agent", "parent_id": "p_1",
                       "handler_module": "counting.tools", "id": kid}),
            )])
            .await
            .remove(0)
            .result;
    }
    // Delete parent — both children's hooks must fire too.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "delete_agent", "id": "p_1"}),
        )])
        .await
        .remove(0)
        .result;
    // 3 hook fires (p_1 + c_1 + c_2).
    assert_eq!(deletes.load(Ordering::SeqCst), 3);
    assert!(!kernel.agents.contains_key(&AgentId::from("p_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("c_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("c_2")));
}

/// Kernel + registered root, the shape every lifecycle test needs.
fn mk_rooted(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    let kernel = mk_kernel(Arc::new(AtomicUsize::new(0)));
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

// ─── #22-#25 — kernel verbs are ROOT-owned, placement is `parent_id` ───

#[tokio::test]
async fn a_kernel_verb_at_a_non_root_agent_says_where_to_send_it() {
    // The verb acts on the TREE, so the agent named is the OPERAND, not the
    // actor. Answered everywhere, "send create_agent to X" read three ways and
    // a model spent 16 turns unable to resolve which — finishing with nothing
    // created. Refused in the HANDLER, so gate-denial probes stay valid.
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "counting.tools", "id": "victim"}),
        )])
        .await
        .remove(0)
        .result;

    let r = kernel
        .send(vec![Item::new(
            AgentId::from("victim"),
            json!({"type": "list_agents"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_here");
    assert!(r["hint"].as_str().unwrap().contains("kernel"));
    assert!(r["hint"].as_str().unwrap().contains("OPERAND"));
}

#[tokio::test]
async fn create_agent_demands_a_parent_and_resolves_it_like_a_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = mk_rooted(&tmp);

    // Absent ⇒ missing_arg, and the refusal says what to name.
    let missing = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "handler_module": "counting.tools", "id": "x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(missing["reason"], "missing_arg");
    assert!(missing["hint"].as_str().unwrap().contains("subtree"));

    // Unknown ⇒ unknown_agent, carrying the live id index.
    let unknown = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "nope",
                   "handler_module": "counting.tools", "id": "x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(unknown["reason"], "unknown_agent");
    assert!(unknown["agents"].is_array());

    // Named ⇒ the new agent lands THERE, not under whoever was addressed.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": "counting.tools", "id": "mid"}),
        )])
        .await
        .remove(0)
        .result;
    let rec = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "mid",
                   "handler_module": "counting.tools", "id": "leaf"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        rec["parent_id"], "mid",
        "the field that expresses placement must actually place it"
    );
}
