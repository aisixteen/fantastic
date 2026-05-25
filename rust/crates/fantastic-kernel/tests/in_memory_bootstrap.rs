//! End-to-end test that `BootstrapOptions::in_memory()` boots a
//! fully-functional kernel WITHOUT touching the filesystem.
//!
//! This is the foundation for the Swift app's "brain" kernel — an
//! always-running in-process kernel that never persists to disk and
//! exposes its state on demand via [`Kernel::save`] / [`Kernel::load`].

use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

struct Noop;

#[async_trait::async_trait]
impl Bundle for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(Value::Null))
    }
}

fn registry() -> BundleRegistry {
    let mut r = BundleRegistry::new();
    r.register("noop.tools", Noop);
    r
}

#[tokio::test]
async fn in_memory_boots_and_creates_agents_without_touching_disk() {
    // Set cwd to a tempdir so accidental relative-path writes would
    // be observable. The kernel itself doesn't care about cwd — this
    // is purely a post-hoc check.
    let cwd_guard = TempDir::new().unwrap();
    let prev_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(cwd_guard.path()).unwrap();

    let booted = bootstrap(registry(), BootstrapOptions::in_memory()).unwrap();
    let kernel = Arc::clone(&booted.kernel);

    // Root is registered and accessible.
    let root = kernel.root().expect("root set");
    assert_eq!(root.id.0, "core");

    // Create + mutate + delete an agent — all should work in
    // memory.
    let v = kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":"noop.tools","id":"brainpipe"}),
        )
        .await;
    assert_eq!(v["id"], "brainpipe");
    assert!(kernel.agents.contains_key(&AgentId::from("brainpipe")));

    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"update_agent","id":"brainpipe","note":"tutor"}),
        )
        .await;
    let snap = kernel.save();
    let bp = snap.agents.iter().find(|a| a.id == "brainpipe").unwrap();
    assert_eq!(bp.meta.get("note"), Some(&json!("tutor")));

    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"delete_agent","id":"brainpipe"}),
        )
        .await;
    assert!(!kernel.agents.contains_key(&AgentId::from("brainpipe")));

    // Critical assertion: no `.fantastic/` dir appeared anywhere
    // under the cwd guard.
    let stray = cwd_guard.path().join(".fantastic");
    assert!(
        !stray.exists(),
        ".fantastic/ leaked at {} after InMemory bootstrap + mutations",
        stray.display(),
    );

    if let Some(p) = prev_cwd {
        let _ = std::env::set_current_dir(p);
    }
}

#[tokio::test]
async fn in_memory_save_load_roundtrips() {
    let booted = bootstrap(registry(), BootstrapOptions::in_memory()).unwrap();
    let kernel = Arc::clone(&booted.kernel);
    for id in ["a", "b", "c"] {
        kernel
            .send(
                &AgentId::from("core"),
                json!({"type":"create_agent","handler_module":"noop.tools","id":id}),
            )
            .await;
    }
    let snapshot = kernel.save_json();

    // Fresh InMemory kernel — load → state matches.
    let mut kernel2 = Kernel::with_storage(fantastic_kernel::StorageMode::InMemory);
    kernel2.bundles = registry();
    let kernel2 = Arc::new(kernel2);
    kernel2.load_json(&snapshot).unwrap();
    for id in ["core", "a", "b", "c"] {
        assert!(kernel2.agents.contains_key(&AgentId::from(id)));
    }
    assert_eq!(kernel2.save_json(), snapshot);
}
