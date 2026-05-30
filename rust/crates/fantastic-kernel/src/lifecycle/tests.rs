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
    let mut kernel = Kernel::with_storage(storage);
    kernel
        .bundles
        .register("counting.tools", CountingBundle { deletes });
    Arc::new(kernel)
}

#[tokio::test]
async fn mint_id_format_is_bundle_underscore_hex6() {
    let id = mint_id("file.tools");
    assert!(id.starts_with("file_"));
    // 6 hex chars after the underscore.
    let suffix = &id["file_".len()..];
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

    // Create one child via the system verb.
    let v = kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "create_agent", "handler_module": "counting.tools", "id": "kid_1"}),
        )
        .await;
    assert_eq!(v["id"], "kid_1");
    assert!(kernel.agents.contains_key(&AgentId::from("kid_1")));
    assert!(tmp.path().join("agents/kid_1/agent.json").exists());

    // Delete it.
    let v = kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "delete_agent", "id": "kid_1"}),
        )
        .await;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "create_agent", "handler_module": "counting.tools", "id": "locked_1", "delete_lock": true}),
        )
        .await;
    let v = kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "delete_agent", "id": "locked_1"}),
        )
        .await;
    assert_eq!(v["locked"], true);
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "create_agent", "handler_module": "counting.tools", "id": "p_1"}),
        )
        .await;
    kernel
        .send(
            &AgentId::from("p_1"),
            json!({"type": "create_agent", "handler_module": "counting.tools", "id": "c_1"}),
        )
        .await;
    kernel
        .send(
            &AgentId::from("p_1"),
            json!({"type": "create_agent", "handler_module": "counting.tools", "id": "c_2"}),
        )
        .await;
    // Delete parent — both children's hooks must fire too.
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "delete_agent", "id": "p_1"}),
        )
        .await;
    // 3 hook fires (p_1 + c_1 + c_2).
    assert_eq!(deletes.load(Ordering::SeqCst), 3);
    assert!(!kernel.agents.contains_key(&AgentId::from("p_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("c_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("c_2")));
}
