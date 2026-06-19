//! Kernel-integration tests for `fantastic-host`: drive a REAL in-proc kernel
//! (the full host bundle set, bootstrapped in-memory + booted) and assert the
//! pure provisioning/reflect surface. NO model is ever invoked — these only
//! exercise `compose_manager` + the kernel's own `reflect`/`list_agents` verbs,
//! so they're fully deterministic and CI-safe.

use fantastic_host::compose_manager;
use fantastic_kernel::AgentId;
use serde_json::{json, Value};

/// `compose_manager` yields a live kernel whose `reflect`/`tree=ids` reports the
/// root (`core`). The `kernel` dispatch alias reaches the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compose_manager_reflect_tree_contains_root() {
    // An in-memory bootstrap auto-creates the root `core`; `loaded` enumerates
    // any *seeded* agents (empty here), so we assert on the kernel surface, not
    // its length.
    let (kernel, _loaded) = compose_manager().await.expect("compose host kernel");

    let reply = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids"}),
        )
        .await;

    assert!(
        reply.get("error").is_none(),
        "reflect must not error: {reply}"
    );
    let ids = reply
        .get("tree")
        .and_then(Value::as_array)
        .expect("reflect tree=ids returns a `tree` array");
    let ids: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
    assert!(
        ids.contains(&"core"),
        "tree ids should contain the root `core`, got {ids:?}"
    );
}

/// `list_agents` returns the registered agent records, including the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_agents_includes_root() {
    let (kernel, _loaded) = compose_manager().await.expect("compose host kernel");

    let reply = kernel
        .send(&AgentId::from("kernel"), json!({"type":"list_agents"}))
        .await;

    assert!(
        reply.get("error").is_none(),
        "list_agents must not error: {reply}"
    );
    let agents = reply
        .get("agents")
        .and_then(Value::as_array)
        .expect("list_agents returns an `agents` array");
    let ids: Vec<&str> = agents
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str))
        .collect();
    assert!(
        ids.contains(&"core"),
        "list_agents should include the root `core`, got {ids:?}"
    );
}
