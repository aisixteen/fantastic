//! A bad snapshot is refused BEFORE the live tree is torn down.
//!
//! `Kernel::load` clears `agents` and rebuilds, so every check that can fail has
//! to run first — otherwise a hand-authored document leaves a kernel with an
//! empty tree and no root, bricked by its own recovery path.
//!
//! Measured 2026-09-02 (python-canonical, ported): a parent CYCLE resolved on
//! both ends (each `parent_id` named a record in the set) and passed validation,
//! then failed mid-hydration with "could not resolve N agents" — after the old
//! tree was already gone.

use fantastic_kernel::{Agent, AgentId, BundleRegistry, Kernel, KernelState};
use serde_json::{json, Map};
use std::sync::Arc;
use tempfile::TempDir;

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles = BundleRegistry::new();
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

fn state(records: serde_json::Value) -> KernelState {
    serde_json::from_value(json!({"version": 1, "agents": records})).unwrap()
}

#[test]
fn a_parent_cycle_is_refused_and_nothing_is_torn_down() {
    let (_t, kernel) = mk();
    let before: Vec<String> = kernel.agents.iter().map(|e| e.key().0.clone()).collect();
    let err = kernel
        .load(state(json!([
            {"id": "root", "parent_id": null, "meta": {}},
            {"id": "a", "parent_id": "b", "meta": {}},
            {"id": "b", "parent_id": "a", "meta": {}}
        ])))
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("unreachable"), "{msg}");
    assert!(msg.contains('a') && msg.contains('b'), "{msg}");
    // The live tree is intact.
    let after: Vec<String> = kernel.agents.iter().map(|e| e.key().0.clone()).collect();
    assert_eq!(before, after);
    assert!(kernel.root().is_some());
}

#[test]
fn a_reachable_tree_loads() {
    let (_t, kernel) = mk();
    kernel
        .load(state(json!([
            {"id": "root", "parent_id": null, "meta": {}},
            {"id": "a", "parent_id": "root", "meta": {}},
            {"id": "b", "parent_id": "a", "meta": {}}
        ])))
        .expect("a reachable tree must load");
    for id in ["root", "a", "b"] {
        assert!(
            kernel.agents.contains_key(&AgentId::from(id)),
            "{id} missing"
        );
    }
}

#[test]
fn an_orphan_chain_is_refused() {
    // `x -> y -> x` is a cycle; this is the other shape — a chain hanging off a
    // parent that exists but is itself unreachable.
    let (_t, kernel) = mk();
    let err = kernel
        .load(state(json!([
            {"id": "root", "parent_id": null, "meta": {}},
            {"id": "lost", "parent_id": "gone", "meta": {}},
            {"id": "gone", "parent_id": "lost", "meta": {}}
        ])))
        .unwrap_err();
    assert!(format!("{err:?}").contains("unreachable"));
}
