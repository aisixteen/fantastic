//! Persistence integration — Agent records survive write→read cycles
//! and weak-load skip+log fires the documented contract line.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Kernel, Reply, StorageMode};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Disk-mode storage rooted at the tempdir — short helper used by
/// the persist callsites below.
fn disk_storage(tmp: &TempDir) -> StorageMode {
    StorageMode::Disk(tmp.path().to_path_buf())
}

// A noop bundle so we can register a recognized handler_module for
// the weak-load contrast tests (known vs unknown).
struct NoopBundle;

#[async_trait]
impl Bundle for NoopBundle {
    fn name(&self) -> &str {
        "noop"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &std::sync::Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(None)
    }
}

fn fresh_workdir() -> TempDir {
    TempDir::new().expect("tempdir")
}

#[test]
fn persist_then_read_record_round_trips() {
    let tmp = fresh_workdir();
    let root_path = tmp.path().join("agents/test_1");
    let mut meta = Map::new();
    meta.insert("display_name".into(), json!("Testy"));
    meta.insert("port".into(), json!(18181));
    let agent = Agent::new(
        AgentId::from("test_1"),
        Some("noop.tools".to_string()),
        Some(AgentId::from("core")),
        meta.clone(),
        root_path.clone(),
        false,
    );
    fantastic_kernel::persistence::persist(&agent, &disk_storage(&tmp)).expect("write");
    let rec = fantastic_kernel::persistence::read_record_at(&root_path.join("agent.json"))
        .expect("read ok")
        .expect("file exists");
    assert_eq!(rec.id, "test_1");
    assert_eq!(rec.handler_module.as_deref(), Some("noop.tools"));
    assert_eq!(rec.parent_id.as_deref(), Some("core"));
    assert_eq!(rec.meta["port"], json!(18181));
    assert_eq!(rec.meta["display_name"], json!("Testy"));
}

#[test]
fn ephemeral_agents_skip_persistence() {
    let tmp = fresh_workdir();
    let root_path = tmp.path().join("agents/eph_1");
    let agent = Agent::new(
        AgentId::from("eph_1"),
        None,
        Some(AgentId::from("core")),
        Map::new(),
        root_path.clone(),
        true, // ephemeral
    );
    fantastic_kernel::persistence::persist(&agent, &disk_storage(&tmp)).expect("noop ok");
    assert!(!root_path.exists(), "ephemeral must not create dir");
}

#[test]
fn seed_readme_is_idempotent_and_preserves_user_edits() {
    let tmp = fresh_workdir();
    let root_path = tmp.path().join("agents/r_1");
    let agent = Agent::new(
        AgentId::from("r_1"),
        Some("noop.tools".to_string()),
        Some(AgentId::from("core")),
        Map::new(),
        root_path.clone(),
        false,
    );
    let storage = disk_storage(&tmp);
    fantastic_kernel::persistence::seed_readme(&agent, "first", &storage).expect("seed");
    let readme = root_path.join("readme.md");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "first");
    // User edits the readme.
    std::fs::write(&readme, "USER EDIT").unwrap();
    // Second seed call must NOT overwrite.
    fantastic_kernel::persistence::seed_readme(&agent, "second", &storage).expect("seed");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "USER EDIT");
}

#[test]
fn load_children_hydrates_registered_agents() {
    let tmp = fresh_workdir();
    let root = tmp.path();
    // Stage two children under root: agents/known + agents/known/agents/grandchild.
    let known_dir = root.join("agents/known");
    let grand_dir = known_dir.join("agents/grandchild");
    std::fs::create_dir_all(&grand_dir).unwrap();
    std::fs::write(
        known_dir.join("agent.json"),
        r#"{"id":"known","handler_module":"noop.tools","parent_id":"core"}"#,
    )
    .unwrap();
    std::fs::write(
        grand_dir.join("agent.json"),
        r#"{"id":"grandchild","handler_module":"noop.tools","parent_id":"known"}"#,
    )
    .unwrap();
    let kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    // Synthetic parent that points at the workdir root.
    let parent = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");
    assert_eq!(loaded.len(), 2);
    assert!(kernel.agents.contains_key(&AgentId::from("known")));
    assert!(kernel.agents.contains_key(&AgentId::from("grandchild")));
    // Grandchild also wired into known's children map:
    let known = kernel.agents.get(&AgentId::from("known")).unwrap().clone();
    assert!(known.has_child(&AgentId::from("grandchild")));
}

#[test]
fn load_children_weak_load_skips_unknown_handler_module_and_subtree() {
    // Plant a ghost agent whose handler_module isn't registered, AND
    // a real child underneath it. Both must be skipped; neither should
    // appear in kernel.agents. The on-disk records stay untouched.
    let tmp = fresh_workdir();
    let root = tmp.path();
    let ghost_dir = root.join("agents/ghost_1");
    let ghost_child = ghost_dir.join("agents/ghost_child");
    std::fs::create_dir_all(&ghost_child).unwrap();
    std::fs::write(
        ghost_dir.join("agent.json"),
        r#"{"id":"ghost_1","handler_module":"unknown.tools","parent_id":"core"}"#,
    )
    .unwrap();
    std::fs::write(
        ghost_child.join("agent.json"),
        r#"{"id":"ghost_child","handler_module":"noop.tools","parent_id":"ghost_1"}"#,
    )
    .unwrap();
    // And a sibling with a recognized handler — must STILL load.
    let real_dir = root.join("agents/real_1");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(
        real_dir.join("agent.json"),
        r#"{"id":"real_1","handler_module":"noop.tools","parent_id":"core"}"#,
    )
    .unwrap();

    let kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    let parent = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");

    // Only `real_1` loaded; ghost branch skipped wholesale.
    assert_eq!(loaded, vec![AgentId::from("real_1")]);
    assert!(kernel.agents.contains_key(&AgentId::from("real_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("ghost_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("ghost_child")));

    // On-disk records survived untouched (Reboot under a runtime with
    // unknown.tools and ghost_1 rehydrates intact).
    assert!(ghost_dir.join("agent.json").exists());
    assert!(ghost_child.join("agent.json").exists());
}

#[test]
fn load_children_tolerates_corrupt_agent_json() {
    let tmp = fresh_workdir();
    let root = tmp.path();
    let bad = root.join("agents/broken");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("agent.json"), "{NOT_JSON").unwrap();

    let kernel = Kernel::new();
    let reg = BundleRegistry::new();
    let parent = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");
    assert!(loaded.is_empty());
    assert!(!kernel.agents.contains_key(&AgentId::from("broken")));
}
