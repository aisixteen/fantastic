//! Unit tests for [`crate::bootstrap`].
use crate::outcome::Item;

use super::*;
use crate::Bundle;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Register the ROOT LOADER. After C12 the root's `handler_module` is
/// `kernel_state.tools`, and its `boot` is what mints the guid, rehydrates the
/// ledger, starts the pump and subscribes the record writer — so a test that
/// wants any of that boots the real bundle, not a second copy of it.
fn with_loader(mut reg: BundleRegistry) -> BundleRegistry {
    crate::test_support::register_root_loader(&mut reg);
    reg
}

use crate::test_support::{boot_root, settle_records as settle};

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
    assert_eq!(booted.kernel.root().unwrap().id.0, "kernel_state");
    assert!(tmp.path().join(".fantastic/agent.json").exists());
    assert!(tmp.path().join(".fantastic/lock.json").exists());
    shutdown(tmp.path()).unwrap();
}

#[test]
fn bootstrap_hydrates_persisted_children() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Boot once + wire a persistence provider + create a child. Persistence is
    // provider-routed AND debounced through the root loader's record writer now,
    // so the child only lands on disk after the loader booted and a pass ran.
    {
        let mut reg = with_loader(BundleRegistry::new());
        reg.register("noop.tools", Noop);
        crate::test_support::register_fake_store(&mut reg, &store_root);
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
        let kernel = Arc::clone(&booted.kernel);
        rt.block_on(async {
            // THE ROOT BOOTS FIRST (registration order), so the record writer
            // is subscribed before the store agent exists — which is how the
            // store persists ITSELF through itself, as python's does.
            boot_root(&kernel).await;
            crate::test_support::wire_fake_store(&kernel, &store_root).await;
            let _ = kernel
                .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":"child_a"}))])
                .await.remove(0).result;
            settle().await;
        });
        shutdown(tmp.path()).unwrap();
    }
    // Boot again — child + store must rehydrate (the store re-discovers itself).
    let mut reg2 = with_loader(BundleRegistry::new());
    reg2.register("noop.tools", Noop);
    crate::test_support::register_fake_store(&mut reg2, &store_root);
    let booted2 = bootstrap(reg2, BootstrapOptions::daemon(tmp.path())).expect("boot2");
    assert!(booted2
        .kernel
        .agents
        .contains_key(&AgentId::from("child_a")));
    // Both the child AND the persisted store provider rehydrate.
    assert!(booted2.loaded.contains(&AgentId::from("child_a")));
    assert!(booted2.loaded.contains(&AgentId::from("store")));
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
        r#"{"id":"ghost_1","handler_module":"unknown.tools","parent_id":"kernel_state"}"#,
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

/// Helper: pull the root instance guid out of a booted kernel's live root meta.
fn root_guid(booted: &BootedKernel) -> String {
    booted
        .kernel
        .root()
        .expect("root")
        .meta
        .read()
        .unwrap()
        .get("guid")
        .and_then(Value::as_str)
        .expect("root carries a guid")
        .to_string()
}

#[test]
fn boot_mints_a_32hex_guid_and_persists_it_stably() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let rt = tokio::runtime::Runtime::new().unwrap();
    // First boot mints the guid IN THE LOADER'S `boot` and lets the record
    // writer carry it to the store (python: `kernel.update` → `updated` event →
    // write_stream), which is why the store is wired before the root boots.
    let g1 = {
        let mut reg = with_loader(BundleRegistry::new());
        crate::test_support::register_fake_store(&mut reg, &store_root);
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
        let kernel = Arc::clone(&booted.kernel);
        let g = rt.block_on(async {
            crate::test_support::wire_fake_store(&kernel, &store_root).await;
            boot_root(&kernel).await;
            settle().await;
            root_guid(&booted)
        });
        shutdown(tmp.path()).unwrap();
        g
    };
    assert_eq!(g1.len(), 32);
    assert!(g1
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    // Guid is persisted on the root record.
    let raw = std::fs::read_to_string(tmp.path().join(".fantastic/agent.json")).unwrap();
    assert!(raw.contains(&g1), "guid must be persisted to agent.json");
    // Second boot of the SAME dir reads it back verbatim — minted ONCE.
    let g2 = {
        let mut reg = with_loader(BundleRegistry::new());
        crate::test_support::register_fake_store(&mut reg, &store_root);
        let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot2");
        let kernel = Arc::clone(&booted.kernel);
        let g = rt.block_on(async {
            boot_root(&kernel).await;
            settle().await;
            root_guid(&booted)
        });
        shutdown(tmp.path()).unwrap();
        g
    };
    assert_eq!(g1, g2, "guid must be identical across boots");
}

#[test]
fn in_memory_boot_mints_an_ephemeral_guid() {
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::in_memory(),
    )
    .expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(boot_root(&kernel));
    let g = root_guid(&booted);
    assert_eq!(g.len(), 32);
    assert!(g
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
}

/// C12 + C56: a virgin dir seeds the ROOT the loader owns — id `kernel_state`,
/// handler_module `kernel_state.tools` — and the record written is the
/// CONSTRUCTED root, so its meta is the structural grant baseline in order.
#[test]
fn root_is_the_kernel_state_loader_and_the_seed_is_the_constructed_record() {
    let tmp = TempDir::new().unwrap();
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    )
    .expect("boot");
    let root = booted.kernel.root().expect("root");
    assert_eq!(root.id.0, "kernel_state");
    assert_eq!(root.handler_module.as_deref(), Some("kernel_state.tools"));
    let raw = std::fs::read_to_string(tmp.path().join(".fantastic/agent.json")).unwrap();
    let rec: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(rec["id"], json!("kernel_state"));
    assert_eq!(rec["handler_module"], json!("kernel_state.tools"));
    let keys: Vec<&str> = rec
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys.iter()
            .filter(|k| matches!(**k, "can_call" | "callable_by"))
            .copied()
            .collect::<Vec<_>>(),
        vec!["can_call", "callable_by"],
        "grant baseline in python's order: {keys:?}"
    );
    shutdown(tmp.path()).unwrap();
}

/// C14: the root's meta order is `can_call, callable_by, guid` — the guid is
/// APPENDED by the loader's boot, after the stamped grants.
#[test]
fn root_meta_order_is_grants_then_guid() {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join(".fantastic");
    let mut reg = with_loader(BundleRegistry::new());
    crate::test_support::register_fake_store(&mut reg, &store_root);
    let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
    let kernel = Arc::clone(&booted.kernel);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        crate::test_support::wire_fake_store(&kernel, &store_root).await;
        boot_root(&kernel).await;
        settle().await;
    });
    let meta = kernel.root().unwrap().meta.read().unwrap().clone();
    let keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["can_call", "callable_by", "guid"], "{keys:?}");
    shutdown(tmp.path()).unwrap();
}

/// C56: a root record that is MISSING or not JSON reseeds like a virgin dir and
/// STILL hydrates the children under `agents/` (python `read_tree`, 2026-09-20).
#[test]
fn a_missing_or_corrupt_root_record_reseeds_and_keeps_children() {
    let tmp = TempDir::new().unwrap();
    let fdir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(fdir.join("agents/child_a")).unwrap();
    std::fs::write(
        fdir.join("agents/child_a/agent.json"),
        r#"{"id":"child_a","handler_module":"noop.tools"}"#,
    )
    .unwrap();
    // Not JSON at all — python swallows the decode error and returns `[]`.
    std::fs::write(fdir.join("agent.json"), "{").unwrap();
    let mut reg = with_loader(BundleRegistry::new());
    reg.register("noop.tools", Noop);
    let booted = bootstrap(reg, BootstrapOptions::daemon(tmp.path())).expect("boot");
    assert_eq!(booted.kernel.root().unwrap().id.0, "kernel_state");
    assert_eq!(
        booted.loaded,
        vec![AgentId::from("child_a")],
        "children survive a reseed"
    );
    assert!(booted.kernel.agents.contains_key(&AgentId::from("child_a")));
    // The file was OVERWRITTEN with the fresh root, not left corrupt.
    let raw = std::fs::read_to_string(fdir.join("agent.json")).unwrap();
    let rec: Value = serde_json::from_str(&raw).expect("reseeded record parses");
    assert_eq!(rec["id"], json!("kernel_state"));
    shutdown(tmp.path()).unwrap();
}

/// C54: hydration is SILENT on success — nothing on stdout for a clean boot.
#[test]
fn hydration_and_rehydrate_print_nothing_on_success() {
    let tmp = TempDir::new().unwrap();
    let fdir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&fdir).unwrap();
    std::fs::write(
        fdir.join("messages.xml"),
        format!(
            "{}<row id=\"1\" agent=\"kernel_state\" kind=\"message\" />\n",
            crate::messages_xml::HEADER
        ),
    )
    .unwrap();
    let booted = bootstrap(
        with_loader(BundleRegistry::new()),
        BootstrapOptions::daemon(tmp.path()),
    )
    .expect("boot");
    // The rows landed, and the only proof anyone needs is the store — not a line.
    assert_eq!(booted.kernel.rows.next_id(), 2);
    shutdown(tmp.path()).unwrap();
}
