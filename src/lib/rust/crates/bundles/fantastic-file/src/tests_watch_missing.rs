//! `watch_path` on a path that does not exist yet: accepted as pending, goes live
//! when it appears (`added`), then ordinary `modified`. Refusals and `unwatch_path`
//! still hold for a pending path.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;
use std::time::Duration;

async fn kernel(tmp: &tempfile::TempDir, allow: Value) -> Arc<Kernel> {
    std::fs::create_dir_all(tmp.path().join("served")).unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(HANDLER_MODULE, FileBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        serde_json::Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                   "id": "fb", "root": tmp.path().join("served").display().to_string(),
                   "ingress_rule": "allow_all_ingress", "allow": allow}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "fb", "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from("fb"), payload)])
        .await
        .remove(0)
        .result
}

async fn inbox(kernel: &Arc<Kernel>) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from("_t_watch_missing");
    let rx = kernel.ensure_inbox(&w);
    assert!(kernel.watch(&AgentId::from("fb"), w).await.is_none());
    rx
}

/// The next `changed` for `path` with `kind` within `secs` (others skipped).
async fn next_changed(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    path: &str,
    kind: &str,
    secs: u64,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.checked_duration_since(tokio::time::Instant::now())?;
        let item = tokio::time::timeout(left, rx.recv()).await.ok()??;
        if item.payload["type"] == "changed"
            && item.payload["path"] == path
            && item.payload["kind"] == kind
        {
            return Some(item.payload);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_of_a_missing_file_is_accepted_pending() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let r = call(&k, json!({"type": "watch_path", "path": "a/b/c.txt"})).await;
    assert_eq!(r["watching"], "a/b/c.txt", "{r}");
    assert_eq!(r["pending"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_created_later_in_a_missing_folder_fires_added() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "a/b/c.txt"})).await;
    let served = tmp.path().join("served");
    std::fs::create_dir(served.join("a")).unwrap();
    std::fs::create_dir(served.join("a").join("b")).unwrap();
    // an empty create: a single Create event, so the batch reads as `added`
    std::fs::File::create(served.join("a/b/c.txt")).unwrap();
    assert!(
        next_changed(&mut rx, "a/b/c.txt", "added", 10)
            .await
            .is_some(),
        "no added event for a/b/c.txt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_writes_fire_modified_after_the_path_appears() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "a/b/c.txt"})).await;
    let served = tmp.path().join("served");
    std::fs::create_dir(served.join("a")).unwrap();
    std::fs::create_dir(served.join("a").join("b")).unwrap();
    std::fs::File::create(served.join("a/b/c.txt")).unwrap();
    assert!(next_changed(&mut rx, "a/b/c.txt", "added", 10)
        .await
        .is_some());
    std::fs::write(served.join("a/b/c.txt"), b"hello").unwrap();
    assert!(
        next_changed(&mut rx, "a/b/c.txt", "modified", 10)
            .await
            .is_some(),
        "no modified event after the path appeared"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unwatch_of_a_pending_path_stops_events() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "a/b/c.txt"})).await;
    call(&k, json!({"type": "unwatch_path", "path": "a/b/c.txt"})).await;
    let served = tmp.path().join("served");
    std::fs::create_dir_all(served.join("a").join("b")).unwrap();
    std::fs::File::create(served.join("a/b/c.txt")).unwrap();
    assert!(
        next_changed(&mut rx, "a/b/c.txt", "added", 1)
            .await
            .is_none(),
        "events kept coming after unwatch of a pending path"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_path_outside_the_root_is_still_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let r = call(
        &k,
        json!({"type": "watch_path", "path": "../outside/x.txt"}),
    )
    .await;
    assert!(r["watching"].is_null(), "{r}");
    assert!(r["pending"].is_null(), "{r}");
}
