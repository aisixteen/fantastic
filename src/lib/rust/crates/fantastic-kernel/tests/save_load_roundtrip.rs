//! Round-trip tests for the in-RAM [`Kernel::save`] / [`Kernel::load`]
//! foundation.
//!
//! [`KernelState`] is a Rust value — never a file on disk. Disk mode
//! mirrors agent records to per-agent `agent.yaml` files via
//! [`persistence::persist`] and hydrates them at boot; the
//! `save()` / `load()` API is for IN-RAM export (e.g. an embedding
//! brain kernel persisting state externally to an external
//! store (key-value, cloud sync, or a file).

use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::Item;
use fantastic_kernel::{AgentId, Kernel, KernelState, StorageMode};
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

fn registry_with_noop() -> BundleRegistry {
    let mut r = BundleRegistry::new();
    r.register("noop.tools", Noop);
    // The ROOT LOADER: the root's handler_module is `kernel_state.tools` now,
    // so a snapshot that carries the root weak-loads it unless it is registered.
    fantastic_kernel::test_support::register_root_loader(&mut r);
    r
}

/// Registry with noop + the fake `file_bridge` persistence provider rooted at
/// `store_root` — persistence is provider-routed, so disk tests must wire one.
fn registry_with_store(store_root: &std::path::Path) -> BundleRegistry {
    let mut r = registry_with_noop();
    fantastic_kernel::test_support::register_fake_store(&mut r, store_root);
    r
}

#[tokio::test]
async fn save_is_pure_in_ram_no_state_json_on_disk() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let booted = bootstrap(
        registry_with_store(&store_root),
        BootstrapOptions::daemon(tmp.path()),
    )
    .unwrap();
    let kernel = Arc::clone(&booted.kernel);
    fantastic_kernel::test_support::boot_root(&kernel).await;
    fantastic_kernel::test_support::wire_fake_store(&kernel, &store_root).await;
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":"alpha"}))])
        .await.remove(0).result;
    kernel.flush_records().await;

    // KernelState lives only in RAM. No `state.json` file is ever
    // created — the on-disk medium is the per-agent agent.yaml tree
    // (written THROUGH the provider).
    assert!(!tmp.path().join(".fantastic/state.json").exists());
    assert!(tmp
        .path()
        .join(".fantastic/agents/alpha/agent.yaml")
        .exists());

    let snap = kernel.save();
    assert_eq!(snap.version, fantastic_kernel::CURRENT_VERSION);
    assert!(snap.agents.iter().any(|a| a.id == "alpha"));
    assert!(snap.agents.iter().any(|a| a.id == "kernel_state"));

    fantastic_kernel::bootstrap::shutdown(tmp.path()).unwrap();
}

#[tokio::test]
async fn save_json_is_byte_deterministic() {
    let kernel = Arc::new(Kernel::with_storage(StorageMode::InMemory));
    let booted_kernel = {
        let mut k = Kernel::with_storage(StorageMode::InMemory);
        k.bundles = registry_with_noop();
        Arc::new(k)
    };
    drop(kernel);
    let booted = bootstrap(registry_with_noop(), BootstrapOptions::in_memory()).unwrap();
    let kernel = Arc::clone(&booted.kernel);
    drop(booted_kernel);

    for id in ["b", "a", "c"] {
        let _ = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":id}))])
            .await.remove(0).result;
    }
    let first = kernel.save_json();
    let second = kernel.save_json();
    assert_eq!(first, second, "save_json must be deterministic");
    // python `save`: `records.sort(key=lambda r: r["id"])` — sorted by id,
    // whatever the registration order.
    let snap: KernelState = serde_json::from_str(&first).unwrap();
    let ids: Vec<&str> = snap.agents.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c", "kernel_state"]);
}

#[tokio::test]
async fn save_and_load_round_trips_disk_to_memory() {
    let tmp = TempDir::new().unwrap();
    let booted = bootstrap(registry_with_noop(), BootstrapOptions::daemon(tmp.path())).unwrap();
    let disk_kernel = Arc::clone(&booted.kernel);
    for id in ["alpha", "beta"] {
        let _ = disk_kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":id}))])
            .await.remove(0).result;
    }
    let snapshot_json = disk_kernel.save_json();

    // Restore into a fresh InMemory kernel — agent tree matches.
    let mut mem_kernel = Kernel::with_storage(StorageMode::InMemory);
    mem_kernel.bundles = registry_with_noop();
    let mem_kernel = Arc::new(mem_kernel);
    mem_kernel.load_json(&snapshot_json).unwrap();
    for id in ["kernel_state", "alpha", "beta"] {
        assert!(mem_kernel.agents.contains_key(&AgentId::from(id)));
    }
    assert_eq!(mem_kernel.save_json(), snapshot_json);

    fantastic_kernel::bootstrap::shutdown(tmp.path()).unwrap();
}

#[tokio::test]
async fn load_weak_loads_unknown_handler_modules() {
    let snapshot = json!({
        "version": 1,
        "agents": [
            {"id": "kernel_state", "parent_id": null},
            {"id": "ghost", "parent_id": "kernel_state", "handler_module": "nonexistent.tools"},
            {"id": "alive", "parent_id": "kernel_state", "handler_module": "noop.tools"},
        ],
    })
    .to_string();

    let mut kernel = Kernel::with_storage(StorageMode::InMemory);
    kernel.bundles = registry_with_noop();
    let kernel = Arc::new(kernel);
    kernel.load_json(&snapshot).unwrap();

    assert!(kernel.agents.contains_key(&AgentId::from("kernel_state")));
    assert!(kernel.agents.contains_key(&AgentId::from("alive")));
    assert!(
        !kernel.agents.contains_key(&AgentId::from("ghost")),
        "ghost dropped — bundle unknown"
    );
}

#[tokio::test]
async fn load_rejects_missing_root() {
    let snapshot = json!({
        "version": 1,
        "agents": [
            {"id": "orphan", "parent_id": "missing", "handler_module": "noop.tools"},
        ],
    })
    .to_string();
    let mut kernel = Kernel::with_storage(StorageMode::InMemory);
    kernel.bundles = registry_with_noop();
    let kernel = Arc::new(kernel);
    let err = kernel.load_json(&snapshot).unwrap_err();
    assert!(format!("{err}").contains("no root"));
}

#[tokio::test]
async fn load_rejects_duplicate_ids() {
    let snapshot = json!({
        "version": 1,
        "agents": [
            {"id": "kernel_state", "parent_id": null},
            {"id": "dup", "parent_id": "kernel_state", "handler_module": "noop.tools"},
            {"id": "dup", "parent_id": "kernel_state", "handler_module": "noop.tools"},
        ],
    })
    .to_string();
    let mut kernel = Kernel::with_storage(StorageMode::InMemory);
    kernel.bundles = registry_with_noop();
    let kernel = Arc::new(kernel);
    let err = kernel.load_json(&snapshot).unwrap_err();
    assert!(format!("{err}").contains("duplicate"));
}

#[tokio::test]
async fn load_rejects_future_version() {
    let snapshot = json!({
        "version": 9999,
        "agents": [{"id": "kernel_state", "parent_id": null}],
    })
    .to_string();
    let kernel = Arc::new(Kernel::with_storage(StorageMode::InMemory));
    let err = kernel.load_json(&snapshot).unwrap_err();
    assert!(format!("{err}").contains("exceeds"));
}

#[tokio::test]
async fn persist_merge_preserves_extra_fields_on_disk() {
    // The dirty-binding contract: existing agent.yaml fields the
    // kernel doesn't manage survive persist calls. Stage an agent
    // dir + agent.yaml with a custom field, then drive an
    // `update_agent` that touches a different field and confirm the
    // custom one's still there.
    use serde_json::Map;
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let booted = bootstrap(
        registry_with_store(&store_root),
        BootstrapOptions::daemon(tmp.path()),
    )
    .unwrap();
    let kernel = Arc::clone(&booted.kernel);
    fantastic_kernel::test_support::boot_root(&kernel).await;
    fantastic_kernel::test_support::wire_fake_store(&kernel, &store_root).await;

    // Create the agent normally.
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":"merge_test","port":8080}))])
        .await.remove(0).result;
    kernel.flush_records().await;
    let agent_json = tmp.path().join(".fantastic/agents/merge_test/agent.yaml");
    assert!(agent_json.exists());

    // Stash a custom field that the kernel doesn't know about.
    let mut existing: Map<String, Value> =
        fantastic_kernel::persistence::parse_record(&std::fs::read_to_string(&agent_json).unwrap())
            .unwrap();
    existing.insert("user_note".into(), json!("don't lose me"));
    std::fs::write(
        &agent_json,
        fantastic_kernel::yaml_io::dump(&Value::Object(existing)),
    )
    .unwrap();

    // Update something else via the kernel — should merge, not overwrite.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type":"update_agent","id":"merge_test","port":9090}),
        )])
        .await
        .remove(0)
        .result;
    kernel.flush_records().await;

    let after: Map<String, Value> =
        fantastic_kernel::persistence::parse_record(&std::fs::read_to_string(&agent_json).unwrap())
            .unwrap();
    assert_eq!(after.get("user_note"), Some(&json!("don't lose me")));
    assert_eq!(after.get("port"), Some(&json!(9090)));

    fantastic_kernel::bootstrap::shutdown(tmp.path()).unwrap();
}
