//! Unit tests for [`crate::bootstrap`].

use super::*;
use crate::Bundle;
use async_trait::async_trait;
use serde_json::{json, Value};
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
        _k: &Arc<Kernel>,
    ) -> Result<crate::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
}

#[test]
fn bootstrap_creates_root_on_virgin_workdir() {
    let tmp = TempDir::new().unwrap();
    let booted =
        bootstrap(BundleRegistry::new(), BootstrapOptions::daemon(tmp.path())).expect("boot");
    assert!(booted.kernel.root().is_some());
    assert_eq!(booted.kernel.root().unwrap().id.0, "core");
    assert!(tmp.path().join(".fantastic/agent.json").exists());
    assert!(tmp.path().join(".fantastic/lock.json").exists());
    shutdown(tmp.path()).unwrap();
}

#[test]
fn bootstrap_hydrates_persisted_children() {
    let tmp = TempDir::new().unwrap();
    // Boot once + create a child.
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", Noop);
    {
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
        let kernel = Arc::clone(&booted.kernel);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            kernel
                .send(
                    &AgentId::from("core"),
                    json!({"type":"create_agent","handler_module":"noop.tools","id":"child_a"}),
                )
                .await;
        });
        shutdown(tmp.path()).unwrap();
    }
    // Boot again — child must rehydrate.
    let mut reg2 = BundleRegistry::new();
    reg2.register("noop.tools", Noop);
    let booted2 = bootstrap(reg2, BootstrapOptions::daemon(tmp.path())).expect("boot2");
    assert!(booted2
        .kernel
        .agents
        .contains_key(&AgentId::from("child_a")));
    assert_eq!(booted2.loaded, vec![AgentId::from("child_a")]);
    shutdown(tmp.path()).unwrap();
}

#[test]
fn bootstrap_one_shot_does_not_acquire_lock() {
    let tmp = TempDir::new().unwrap();
    // First boot acquires.
    let _b1 = bootstrap(BundleRegistry::new(), BootstrapOptions::daemon(tmp.path())).unwrap();
    // One-shot must NOT contend with the held lock.
    let opts = BootstrapOptions::one_shot(tmp.path());
    let booted = bootstrap(BundleRegistry::new(), opts).expect("one-shot ok");
    assert!(booted.kernel.root().is_some());
    shutdown(tmp.path()).unwrap();
}

#[test]
fn bootstrap_weak_loads_unknown_handler_modules() {
    let tmp = TempDir::new().unwrap();
    // Plant a ghost agent on disk before booting.
    let ghost = tmp.path().join(".fantastic/agents/ghost_1");
    std::fs::create_dir_all(&ghost).unwrap();
    std::fs::write(
        ghost.join("agent.json"),
        r#"{"id":"ghost_1","handler_module":"unknown.tools","parent_id":"core"}"#,
    )
    .unwrap();
    let booted =
        bootstrap(BundleRegistry::new(), BootstrapOptions::daemon(tmp.path())).expect("boot");
    // Skipped — ghost_1 not registered.
    assert!(!booted.kernel.agents.contains_key(&AgentId::from("ghost_1")));
    // Record still on disk for the next runtime.
    assert!(ghost.join("agent.json").exists());
    shutdown(tmp.path()).unwrap();
}
