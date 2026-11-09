//! A RELATIVE state folder (`.fantastic`) finds its store and persists records.
//!
//! `chdir` is process-global; this binary holds exactly one test, so it owns the
//! cwd (same pattern as `fantastic-static-loader/tests/agentic.rs`).

use fantastic_kernel::bootstrap::{self, BootstrapOptions};
use fantastic_kernel::{test_support, AgentId, BundleRegistry, Item};
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

#[tokio::test]
async fn a_relative_state_folder_finds_its_store_and_persists() {
    let tmp = TempDir::new().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let mut reg = BundleRegistry::new();
    test_support::register_root_loader(&mut reg);
    test_support::register_fake_store(&mut reg, &tmp.path().join(".fantastic"));
    let booted =
        bootstrap::bootstrap(reg, BootstrapOptions::daemon(".fantastic", ".")).expect("boot");
    let kernel = booted.kernel;
    test_support::boot_root(&kernel).await;
    let made = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": "store", "root": "@state",
                "ingress_rule": "allow_all_ingress",
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], "store", "{made}");
    assert_eq!(
        fantastic_kernel::persistence::find_store(&kernel),
        Some(AgentId::from("store"))
    );
    kernel.flush_records().await;
    assert!(tmp
        .path()
        .join(".fantastic/agents/store/agent.yaml")
        .exists());
    bootstrap::shutdown(Path::new(".fantastic")).unwrap();
}
