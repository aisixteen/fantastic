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
    for p in [
        json!({"path": "secret.md"}),
        json!({"path": "docs/missing/deep.md"}),
        json!({}),
    ] {
        let mut payload = json!({"type": "watch_path"});
        for (key, v) in p.as_object().unwrap() {
            payload[key] = v.clone();
        }
        let r = call(&k, payload.clone()).await;
        assert_eq!(r["reason"], "not_found", "{payload}: {r}");
    }
}
