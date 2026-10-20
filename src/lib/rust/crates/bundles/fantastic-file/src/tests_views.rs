//! The view agents — python `file_bridge/tests/test_views.py` and `test_views_units.py`,
//! case for case where the port reaches: `open_view` reuses one view per path; a file
//! view reads, writes and refuses another path; a folder view lists inside itself and
//! refuses an escape; `reveal` shows the way down; a view's watch relays its changes.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;
use std::time::Duration;

async fn kernel(tmp: &tempfile::TempDir) -> Arc<Kernel> {
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
                   "ingress_rule": "allow_all_ingress", "allow": ["."]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "fb", "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn open(kernel: &Arc<Kernel>, kind: &str, path: &str) -> Value {
    call(
        kernel,
        "fb",
        json!({"type": "open_view", "kind": kind, "path": path}),
    )
    .await
}

fn served(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("served")
}

#[tokio::test]
async fn open_view_makes_one_view_per_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("notes.md"), "hi\n").unwrap();
    let first = open(&k, "file", "notes.md").await;
    assert_eq!(first["created"], true, "{first}");
    let again = open(&k, "file", "notes.md").await;
    assert_eq!(again["id"], first["id"]);
    assert_eq!(again["created"], false);
    let rec = k
        .agents
        .get(&AgentId::from(first["id"].as_str().unwrap()))
        .unwrap()
        .record();
    assert_eq!(rec.meta["view"], "file");
    assert_eq!(rec.meta["bridge"], "fb");
    assert_eq!(rec.meta["path"], "notes.md");
    assert_eq!(rec.handler_module.as_deref(), Some(HANDLER_MODULE));
    // the bridge admits the view for exactly its grant
    let fb = k.agents.get(&AgentId::from("fb")).unwrap().record();
    assert_eq!(
        fb.meta["callable_by"][first["id"].as_str().unwrap()],
        json!(["read", "unwatch_path", "watch", "watch_path", "write"])
    );
}

#[tokio::test]
async fn open_view_refuses_bad_arguments_and_unexposed_paths() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    assert_eq!(open(&k, "weird", "x").await["reason"], "bad_arg");
    assert_eq!(open(&k, "file", ".").await["reason"], "bad_arg");
    assert_eq!(open(&k, "file", "/etc").await["reason"], "bad_arg");
}

#[tokio::test]
async fn a_file_view_reads_writes_and_refuses_another_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("a.txt"), "one\n").unwrap();
    std::fs::write(served(&tmp).join("b.txt"), "two\n").unwrap();
    let v = open(&k, "file", "a.txt").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(&k, &v, json!({"type": "read"})).await;
    assert_eq!(r["lines"][0]["text"], "one", "{r}");
    let w = call(
        &k,
        &v,
        json!({"type": "write", "bytes": "two!\n", "if_match": r["version"]}),
    )
    .await;
    assert!(w["version"].is_string(), "{w}");
    assert_eq!(
        std::fs::read_to_string(served(&tmp).join("a.txt")).unwrap(),
        "two!\n"
    );
    let other = call(&k, &v, json!({"type": "read", "path": "b.txt"})).await;
    assert_eq!(other["reason"], "unauthorized", "{other}");
    let w = call(
        &k,
        &v,
        json!({"type": "write", "path": "b.txt", "bytes": "x"}),
    )
    .await;
    assert_eq!(w["reason"], "unauthorized", "{w}");
    assert_eq!(
        std::fs::read_to_string(served(&tmp).join("b.txt")).unwrap(),
        "two\n"
    );
    // a verb the view does not own is unhandled, never forwarded
    let d = call(&k, &v, json!({"type": "delete", "path": "a.txt"})).await;
    assert_eq!(d["reason"], "unhandled", "{d}");
    assert!(served(&tmp).join("a.txt").exists());
}

#[tokio::test]
async fn a_folder_view_lists_inside_itself_and_reveals_the_way_down() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::create_dir_all(served(&tmp).join("docs/a/b")).unwrap();
    std::fs::write(served(&tmp).join("docs/a/b/c.md"), "c").unwrap();
    std::fs::write(served(&tmp).join("other.md"), "x").unwrap();
    let v = open(&k, "folder", "docs").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let listed = call(&k, &v, json!({"type": "list"})).await;
    let names: Vec<&str> = listed["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a"], "{listed}");
    let escape = call(&k, &v, json!({"type": "list", "path": "../"})).await;
    assert_eq!(escape["reason"], "unauthorized", "{escape}");
    let r = call(&k, &v, json!({"type": "reveal", "path": "a/b/c.md"})).await;
    assert_eq!(
        r,
        json!({"revealed": "a/b/c.md", "folder_path": "docs", "dirs": ["a", "a/b"]})
    );
    let gone = call(&k, &v, json!({"type": "reveal", "path": "a/nope.md"})).await;
    assert_eq!(gone["reason"], "not_found", "{gone}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_view_relays_its_changes_to_its_watchers() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("live.txt"), "one\n").unwrap();
    let v = open(&k, "file", "live.txt").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let w = AgentId::from("_t_view");
    let mut rx = k.ensure_inbox(&w);
    assert!(k.watch(&AgentId::from(v.as_str()), w).await.is_none());
    let r = call(&k, &v, json!({"type": "watch"})).await;
    assert_eq!(r["watching"], "live.txt", "{r}");
    assert_eq!(r["relayed"], false);
    tokio::time::sleep(Duration::from_millis(300)).await; // the disk watcher is up
    std::fs::write(served(&tmp).join("live.txt"), "two\n").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .expect("no `changed` for live.txt within 5s");
        let item = tokio::time::timeout(left, rx.recv())
            .await
            .expect("timed out")
            .expect("inbox open");
        if item.payload["type"] == "changed" {
            assert_eq!(item.payload["path"], "live.txt");
            assert!(item.payload["kind"] == "modified" || item.payload["kind"] == "added");
            break;
        }
    }
}
