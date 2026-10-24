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

/// Every `changed` for `path` within `secs`, in order of arrival.
async fn changed_kinds(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    path: &str,
    secs: u64,
) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut kinds = Vec::new();
    while let Some(left) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(item)) if item.payload["type"] == "changed" && item.payload["path"] == path => {
                kinds.push(
                    item.payload["kind"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    kinds
}

/// The race of `a_file_created_later_in_a_missing_folder_fires_added`, repeated:
/// folders and the file are created back to back, and each run must report
/// `added` for the file. Before the fix, a run could lose the create event of
/// the file to the OS (inotify does not replay a folder's events).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifty_quick_nested_creates_each_fire_added() {
    for run in 0..50 {
        let tmp = tempfile::TempDir::new().unwrap();
        let k = kernel(&tmp, json!(["."])).await;
        let mut rx = inbox(&k).await;
        call(&k, json!({"type": "watch_path", "path": "a/b/c.txt"})).await;
        let served = tmp.path().join("served");
        std::fs::create_dir(served.join("a")).unwrap();
        std::fs::create_dir(served.join("a").join("b")).unwrap();
        std::fs::File::create(served.join("a/b/c.txt")).unwrap();
        assert!(
            next_changed(&mut rx, "a/b/c.txt", "added", 10)
                .await
                .is_some(),
            "run {run}: no added event for a/b/c.txt"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_in_a_deep_missing_folder_created_at_once_fires_added() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "a/b/c/d/e.txt"})).await;
    let served = tmp.path().join("served");
    std::fs::create_dir_all(served.join("a/b/c/d")).unwrap();
    std::fs::File::create(served.join("a/b/c/d/e.txt")).unwrap();
    assert!(
        next_changed(&mut rx, "a/b/c/d/e.txt", "added", 10)
            .await
            .is_some(),
        "no added event for a/b/c/d/e.txt"
    );
    std::fs::write(served.join("a/b/c/d/e.txt"), b"deep").unwrap();
    assert!(
        next_changed(&mut rx, "a/b/c/d/e.txt", "modified", 10)
            .await
            .is_some(),
        "no modified event after the deep path appeared"
    );
}

/// A file that exists when it is first watched is not pending: no `pending`
/// flag, no `added`; its later writes read as `modified`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_exists_when_watched_is_not_pending() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let served = tmp.path().join("served");
    std::fs::create_dir_all(served.join("a")).unwrap();
    std::fs::write(served.join("a/c.txt"), "x").unwrap();
    let mut rx = inbox(&k).await;
    let r = call(&k, json!({"type": "watch_path", "path": "a/c.txt"})).await;
    assert_eq!(r["watching"], "a/c.txt", "{r}");
    assert!(r["pending"].is_null(), "{r}");
    // the disk watch starts asynchronously (as in tests_watch.rs)
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(served.join("a/c.txt"), "y").unwrap();
    assert!(
        next_changed(&mut rx, "a/c.txt", "modified", 10)
            .await
            .is_some(),
        "no modified event for an existing file"
    );
    assert!(
        next_changed(&mut rx, "a/c.txt", "added", 1).await.is_none(),
        "an existing file was announced as added"
    );
}

/// The create event and the disk check see the same file: one `added`, then
/// `modified` for later writes, never a second `added`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_created_file_is_announced_added_once_then_modified() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "a/b/c.txt"})).await;
    let served = tmp.path().join("served");
    std::fs::create_dir(served.join("a")).unwrap();
    std::fs::create_dir(served.join("a").join("b")).unwrap();
    std::fs::File::create(served.join("a/b/c.txt")).unwrap();
    assert!(
        next_changed(&mut rx, "a/b/c.txt", "added", 10)
            .await
            .is_some(),
        "no added event for a/b/c.txt"
    );
    // the create is settled before the write, so the write is its own event
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(served.join("a/b/c.txt"), b"hello").unwrap();
    let kinds = changed_kinds(&mut rx, "a/b/c.txt", 2).await;
    assert!(
        kinds.iter().all(|k| k == "modified"),
        "a second added after the first, or a non-modified event: {kinds:?}"
    );
    assert!(kinds.contains(&"modified".to_string()), "{kinds:?}");
}
