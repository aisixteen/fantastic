//! `children_dir` BECOMES A DIRECTORY (`<agent>/<children_dir>/<child>`).
//!
//! Python (canonical) holds it to the agent-id rule at the write
//! (`create_agent` / `update_agent`, `guard_agent_id(..., arg="children_dir")`),
//! and its cold `read_tree` walks only through the `fs` clamp, so an escaping
//! `children_dir` or an entry linked outside `.fantastic` refuses the boot
//! (`fs: path '<rel>' escapes root`). The rust walk joined the value raw and
//! followed symlinks, hydrating records from another directory.

use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::test_support::symlink;
use fantastic_kernel::{Agent, AgentId, BundleRegistry, Item, Kernel};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let kernel = Arc::new(Kernel::new());
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    (tmp, kernel)
}

async fn call(kernel: &Arc<Kernel>, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result
}

fn bad_values() -> Vec<(Value, &'static str)> {
    vec![
        (
            json!("../sibling"),
            "a single path segment (no separators); got '../sibling'",
        ),
        (
            json!("a/b"),
            "a single path segment (no separators); got 'a/b'",
        ),
        (json!(".."), "a name that is not a path traversal; got '..'"),
        (json!(".hidden"), "a name that is not hidden; got '.hidden'"),
        (json!(""), "a non-empty string"),
        (Value::Null, "a non-empty string"),
    ]
}

#[tokio::test]
async fn children_dir_is_one_safe_segment_at_create() {
    let (_tmp, kernel) = mk();
    for (value, want) in bad_values() {
        let r = call(
            &kernel,
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": "noop.tools", "id": "x", "children_dir": value}),
        )
        .await;
        assert_eq!(
            r,
            json!({"error": format!("create_agent: children_dir must be {want}"),
                   "reason": "bad_arg", "arg": "children_dir"}),
        );
        assert!(kernel.agents.get(&AgentId::from("x")).is_none());
    }
    let ok = call(
        &kernel,
        json!({"type": "create_agent", "parent_id": "kernel_state",
               "handler_module": "noop.tools", "id": "x", "children_dir": "web_agents"}),
    )
    .await;
    assert_eq!(ok["children_dir"], "web_agents", "{ok}");
}

#[tokio::test]
async fn children_dir_is_one_safe_segment_at_update() {
    let (_tmp, kernel) = mk();
    for (value, want) in bad_values() {
        let r = call(
            &kernel,
            json!({"type": "update_agent", "id": "kernel_state", "children_dir": value}),
        )
        .await;
        assert_eq!(
            r,
            json!({"error": format!("update_agent: children_dir must be {want}"),
                   "reason": "bad_arg", "arg": "children_dir"}),
        );
    }
    let ok = call(
        &kernel,
        json!({"type": "update_agent", "id": "kernel_state", "children_dir": "host_agents"}),
    )
    .await;
    assert_eq!(ok["updated"], true, "{ok}");
    assert_eq!(ok["agent"]["children_dir"], "host_agents", "{ok}");
}

fn seed(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn boot_err(work: &Path) -> String {
    match bootstrap(BundleRegistry::new(), BootstrapOptions::one_shot(work)) {
        Ok(_) => panic!("the boot must refuse"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn cold_walk_refuses_an_escaping_children_dir() {
    let tmp = TempDir::new().unwrap();
    let work = tmp.path().join("work");
    seed(
        &work.join(".fantastic/agent.yaml"),
        "id: kernel_state\nchildren_dir: ../sibling\n",
    );
    seed(&work.join("sibling/probe_a/agent.yaml"), "id: probe_a\n");
    let err = boot_err(&work);
    assert!(
        err.ends_with(": fs: path '../sibling' escapes root"),
        "{err}"
    );
}

#[test]
fn cold_walk_refuses_an_outward_symlinked_entry() {
    let tmp = TempDir::new().unwrap();
    let work = tmp.path().join("work");
    seed(&work.join(".fantastic/agent.yaml"), "id: kernel_state\n");
    seed(&tmp.path().join("outside/agent.yaml"), "id: probe_out\n");
    std::fs::create_dir_all(work.join(".fantastic/agents")).unwrap();
    symlink(tmp.path().join("outside"), work.join(".fantastic/agents/evil")).unwrap();
    let err = boot_err(&work);
    assert!(
        err.ends_with(": fs: path 'agents/evil/agent.yaml' escapes root"),
        "{err}"
    );
}

#[test]
fn cold_walk_follows_an_inward_symlinked_entry() {
    let tmp = TempDir::new().unwrap();
    let work = tmp.path().join("work");
    let fdir = work.join(".fantastic");
    seed(&fdir.join("agent.yaml"), "id: kernel_state\n");
    seed(&fdir.join("stash/a/agent.yaml"), "id: a\n");
    std::fs::create_dir_all(fdir.join("agents")).unwrap();
    symlink(fdir.join("stash/a"), fdir.join("agents/a")).unwrap();
    let booted = bootstrap(BundleRegistry::new(), BootstrapOptions::one_shot(&work)).unwrap();
    let a = booted
        .kernel
        .agents
        .get(&AgentId::from("a"))
        .expect("a hydrated");
    assert_eq!(
        a.parent_id.as_ref().map(|p| p.0.as_str()),
        Some("kernel_state")
    );
}
