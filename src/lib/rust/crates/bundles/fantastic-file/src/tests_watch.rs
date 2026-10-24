//! `watch_path` — python `file_bridge/tests/test_file_watch.py`, case for case:
//! against the real disk, a direct write (an editor), an atomic save (a temp file
//! renamed over), a write through the bridge, a directory's entries; refusals;
//! `unwatch_path` ends it; deleting the bridge stops its watcher.

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
    let w = AgentId::from("_t_watch");
    let rx = kernel.ensure_inbox(&w);
    assert!(kernel.watch(&AgentId::from("fb"), w).await.is_none());
    rx
}

/// The next `changed` for `path` within `secs` (other events skipped).
async fn next_changed(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    path: &str,
    secs: u64,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.checked_duration_since(tokio::time::Instant::now())?;
        let item = tokio::time::timeout(left, rx.recv()).await.ok()??;
        if item.payload["type"] == "changed" && item.payload["path"] == path {
            return Some(item.payload);
        }
    }
}

fn served(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("served")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_write_an_atomic_save_and_a_bridge_write_are_seen() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    std::fs::write(served(&tmp).join("prompt.md"), "one\n").unwrap();
    let mut rx = inbox(&k).await;
    let r = call(&k, json!({"type": "watch_path", "path": "prompt.md"})).await;
    assert_eq!(r, json!({"watching": "prompt.md", "paths": ["prompt.md"]}));
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(served(&tmp).join("prompt.md"), "two\n").unwrap();
    let ev = next_changed(&mut rx, "prompt.md", 5)
        .await
        .expect("an editor's save");
    assert!(
        matches!(ev["kind"].as_str(), Some("modified" | "added")),
        "{ev}"
    );
    std::fs::write(served(&tmp).join(".prompt.md.tmp"), "three\n").unwrap();
    std::fs::rename(
        served(&tmp).join(".prompt.md.tmp"),
        served(&tmp).join("prompt.md"),
    )
    .unwrap();
    next_changed(&mut rx, "prompt.md", 5)
        .await
        .expect("an atomic save");
    let w = call(
        &k,
        json!({"type": "write", "path": "prompt.md", "bytes": "four\n"}),
    )
    .await;
    assert!(w.get("version").is_some(), "{w}");
    next_changed(&mut rx, "prompt.md", 5)
        .await
        .expect("a write through the bridge");
    std::fs::remove_file(served(&tmp).join("prompt.md")).unwrap();
    let ev = next_changed(&mut rx, "prompt.md", 5)
        .await
        .expect("a delete");
    assert_eq!(ev["kind"], "deleted", "{ev}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_directory_reports_its_entries() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    std::fs::create_dir_all(served(&tmp).join("notes")).unwrap();
    let mut rx = inbox(&k).await;
    assert_eq!(
        call(&k, json!({"type": "watch_path", "path": "notes"})).await["watching"],
        "notes"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(served(&tmp).join("notes/a.md"), "x").unwrap();
    next_changed(&mut rx, "notes/a.md", 5)
        .await
        .expect("a new entry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_watched_path_is_reported_and_unwatch_ends_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    std::fs::write(served(&tmp).join("prompt.md"), "one\n").unwrap();
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "prompt.md"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(served(&tmp).join("other.md"), "noise").unwrap();
    assert!(next_changed(&mut rx, "other.md", 1).await.is_none());
    let r = call(&k, json!({"type": "unwatch_path", "path": "prompt.md"})).await;
    assert_eq!(r, json!({"watching": false, "paths": []}));
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(served(&tmp).join("prompt.md"), "two\n").unwrap();
    assert!(next_changed(&mut rx, "prompt.md", 1).await.is_none());
}

#[tokio::test]
async fn what_is_not_exposed_or_not_there_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["docs"])).await;
    std::fs::write(served(&tmp).join("secret.md"), "no").unwrap();
    for p in [json!({"path": "secret.md"}), json!({})] {
        let mut payload = json!({"type": "watch_path"});
        for (key, v) in p.as_object().unwrap() {
            payload[key] = v.clone();
        }
        let r = call(&k, payload.clone()).await;
        assert_eq!(r["reason"], "not_found", "{payload}: {r}");
    }
}

/// `allow` is read live: narrowed after the watch was armed, the path's changes
/// stop reaching watchers.
///
/// THE FIRST WRITE CAN BE REPORTED LATE. macOS FSEvents may deliver the event of a
/// write made before the watch was armed (its stream latency), and that event
/// lands in the inbox before `allow` is narrowed, so the assertion below read
/// it. Write a marker after arming and drain the inbox up to the marker's own
/// event: a stream delivers its events in event-id order, so nothing older can
/// arrive after it, and the watch is known live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_path_the_bridge_stops_exposing_reports_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    std::fs::create_dir_all(served(&tmp).join("notes")).unwrap();
    std::fs::write(served(&tmp).join("notes/a.md"), "x").unwrap();
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "notes"})).await;
    std::fs::write(served(&tmp).join("notes/marker.md"), "m").unwrap();
    assert!(
        next_changed(&mut rx, "notes/marker.md", 10).await.is_some(),
        "the armed watch reports the marker"
    );
    let mut patch = serde_json::Map::new();
    patch.insert("allow".into(), json!(["other.md"]));
    k.update(&AgentId::from("fb"), patch);
    std::fs::write(served(&tmp).join("notes/a.md"), "y").unwrap();
    assert!(next_changed(&mut rx, "notes/a.md", 1).await.is_none());
}

/// Every `changed` for `path` within `secs`, in order of arrival (other paths
/// and other events skipped).
async fn changes_for(
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

/// A file that exists when it is watched: one write is ONE `modified`. The
/// create that a platform may replay after the arm (FSEvents replays the events
/// of the stream's start) must not read as an `added` for the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_arm_write_gives_exactly_one_modified() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    std::fs::write(served(&tmp).join("f.md"), "one").unwrap();
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "f.md"})).await;
    std::fs::write(served(&tmp).join("f.md"), "two, longer").unwrap();
    let kinds = changes_for(&mut rx, "f.md", 1).await;
    assert_eq!(kinds, vec!["modified".to_string()], "one write, one event");
}

/// An atomic save (a temp file renamed over the path) is a NEW inode: `added`,
/// once. A later open-and-close with no write changes nothing on disk, so it
/// reports nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_atomic_save_after_arm_gives_added_and_a_no_op_event_gives_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    std::fs::write(served(&tmp).join("f.md"), "one").unwrap();
    let mut rx = inbox(&k).await;
    call(&k, json!({"type": "watch_path", "path": "f.md"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(served(&tmp).join(".f.md.tmp"), "two").unwrap();
    std::fs::rename(served(&tmp).join(".f.md.tmp"), served(&tmp).join("f.md")).unwrap();
    let kinds = changes_for(&mut rx, "f.md", 1).await;
    assert_eq!(
        kinds,
        vec!["added".to_string()],
        "an atomic save is one added"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    // opened for writing and closed again: no byte, no stamp change
    drop(
        std::fs::OpenOptions::new()
            .write(true)
            .open(served(&tmp).join("f.md"))
            .unwrap(),
    );
    let kinds = changes_for(&mut rx, "f.md", 1).await;
    assert!(kinds.is_empty(), "a no-op event reported {kinds:?}");
}

/// A held path the watch cannot arm is refused: an error reply, no `watching`,
/// and no hold left behind. A directory that cannot be read cannot be watched
/// by inotify (its `inotify_add_watch` is EACCES); the test is Linux-only.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watch_that_cannot_arm_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp, json!(["."])).await;
    let locked = served(&tmp).join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = call(&k, json!({"type": "watch_path", "path": "locked"})).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r["watching"].is_null(), "a watch that did not arm: {r}");
    assert!(r["error"].is_string(), "a refusal carries an error: {r}");
    let r = call(&k, json!({"type": "unwatch_path", "path": "locked"})).await;
    assert_eq!(r, json!({"watching": false, "paths": []}), "no hold left");
}
