//! The view agents — python `file_bridge/tests/test_views.py` and `test_views_units.py`,
//! case for case where the port reaches: `open_view` reuses one view per path; a file
//! view reads, writes and refuses another path; a folder view lists inside itself and
//! refuses an escape; `reveal` shows the way down; a view's watch relays its changes.

use super::views::{relay_event, FILE, FOLDER};
use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;
use std::time::Duration;

async fn kernel(tmp: &tempfile::TempDir) -> Arc<Kernel> {
    std::fs::create_dir_all(tmp.path().join("served")).unwrap();
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
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
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(root);
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                   "id": "fb", "root": tmp.path().join("served").display().to_string(), "allow": ["."]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "fb", "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(AgentId::from(target), payload)])
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
    let rec = k.resolve_target(first["id"].as_str().unwrap())
        .unwrap()
        .record();
    assert_eq!(rec.meta["view"], "file");
    assert_eq!(rec.meta["bridge"], "fb");
    assert_eq!(rec.meta["path"], "notes.md");
    assert_eq!(rec.handler_module.as_deref(), Some(HANDLER_MODULE));
    // the view's one line: its grant on its bridge, its parent; the bridge keeps
    // no list of callers
    assert_eq!(
        rec.meta["can"],
        json!({"$parent": {"call": ["read", "write", "watch_path", "unwatch_path"]}})
    );
    assert!(k.resolve_target("fb").unwrap().record().meta.get("can").is_none());
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
    assert!(fantastic_kernel::send::as_operator(k.watch(&AgentId::from(v.as_str()), w)).await.is_none());
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

// python `test_views_units.py` — who may reach a view: the bridge's callers, for the
// verbs the view answers.

/// Every event a watcher's inbox holds within `secs` (the full window: a count is the point).
async fn events_for(rx: &mut fantastic_kernel::inbox::InboxReceiver, secs: u64) -> Vec<Value> {
    let mut out = Vec::new();
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(item)) = tokio::time::timeout_at(end, rx.recv()).await {
        out.push(item.payload);
    }
    out
}

/// The verbs the verb docs of `of`'s reflect name, sorted.
async fn docs_verbs(k: &Arc<Kernel>, of: &str) -> Vec<String> {
    let r = fantastic_kernel::test_support::send_settled(k, vec![Item::new(
            AgentId::from(of),
            json!({"type": "reflect", "tier": "oneliners"}),
        )])
        .await
        .remove(0)
        .result;
    let mut names: Vec<String> = r["verbs"].as_object().unwrap().keys().cloned().collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_view_relays_added_changed_removed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::create_dir_all(served(&tmp).join("box")).unwrap();
    let v = open(&k, "folder", "box").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let w = AgentId::from("_t_folder");
    let mut rx = k.ensure_inbox(&w);
    assert!(fantastic_kernel::send::as_operator(k.watch(&AgentId::from(v.as_str()), w)).await.is_none());
    call(&k, &v, json!({"type": "watch"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // the disk watcher is up
    std::fs::write(served(&tmp).join("box/n.md"), "n").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let first = loop {
        let left = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .expect("no entry event for box/n.md within 5s");
        let item = tokio::time::timeout(left, rx.recv())
            .await
            .expect("timed out")
            .expect("inbox open");
        if matches!(item.payload["type"].as_str(), Some("added" | "changed")) {
            break item.payload;
        }
    };
    assert_eq!(first["path"], "box/n.md", "{first}");
    assert_eq!(first["name"], "n.md", "{first}");
    std::fs::remove_file(served(&tmp).join("box/n.md")).unwrap();
    let gone = loop {
        let left = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .expect("no removed event within 5s");
        let item = tokio::time::timeout(left, rx.recv())
            .await
            .expect("timed out")
            .expect("inbox open");
        if item.payload["type"] == "removed" {
            break item.payload;
        }
    };
    assert_eq!(gone["path"], "box/n.md", "{gone}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_opens_of_one_file_share_one_relay() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("shared.txt"), "s\n").unwrap();
    let v = open(&k, "file", "shared.txt").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let w = AgentId::from("_t_shared");
    let mut rx = k.ensure_inbox(&w);
    assert!(fantastic_kernel::send::as_operator(k.watch(&AgentId::from(v.as_str()), w)).await.is_none());
    let reflect = call(&k, &v, json!({"type": "reflect"})).await;
    assert_eq!(reflect["watching"], false, "{reflect}");
    let first = call(&k, &v, json!({"type": "watch"})).await;
    assert_eq!(first["relayed"], false, "{first}");
    // the second open is the same agent, and its watch reuses the relay
    let again = open(&k, "file", "shared.txt").await;
    assert_eq!(again["id"], v.as_str(), "{again}");
    assert_eq!(again["created"], false);
    let second = call(&k, &v, json!({"type": "watch"})).await;
    assert_eq!(second["relayed"], true, "{second}");
    let reflect = call(&k, &v, json!({"type": "reflect"})).await;
    assert_eq!(reflect["watching"], true, "{reflect}");
    // the first watch reply comes after the bridge's watch() has armed the disk stream
    std::fs::write(served(&tmp).join("shared.txt"), "t\n").unwrap();
    // the replay of the file's pre-arm create (FSEvents) can add `added` events
    // to this one write, so only that a `changed` arrives is asserted (#159)
    let events = events_for(&mut rx, 1).await;
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "changed" && e["path"] == "shared.txt"),
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_view_ends_its_relay() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("gone.txt"), "g\n").unwrap();
    let v = open(&k, "file", "gone.txt").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    // the bridge's own watcher: it sees a change while a view holds the path
    let w = AgentId::from("_t_bridge_watch");
    let mut rx = k.ensure_inbox(&w);
    assert!(fantastic_kernel::send::as_operator(k.watch(&AgentId::from("fb"), w)).await.is_none());
    call(&k, &v, json!({"type": "watch"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // the disk watcher is up
    std::fs::write(served(&tmp).join("gone.txt"), "h\n").unwrap();
    let before = events_for(&mut rx, 1).await;
    assert!(
        before.iter().any(|e| e["type"] == "changed"),
        "the bridge sees the change while the view watches: {before:?}"
    );
    let del = fantastic_kernel::test_support::send_settled(&k, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": v}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(del["deleted"], true, "{del}");
    // the view's `unwatch_path` to the bridge is a call (#795): its settle comes after
    // the delete's, so the write below waits until nothing is in flight but the read
    let mut quiet = false;
    for _ in 0..500 {
        let l = fantastic_kernel::test_support::send_settled(&k, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "ledger"}),
        )])
        .await
        .remove(0)
        .result;
        quiet = l["executions"].as_array().is_some_and(|rows| {
            !rows
                .iter()
                .any(|e| e["status"] == "in_flight" && e["target"] != "kernel_state")
        });
        if quiet {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(quiet, "the bridge's unwatch never settled");
    std::fs::write(served(&tmp).join("gone.txt"), "i\n").unwrap();
    // the relay let go of the bridge's watch: no `changed` reaches the bridge's watchers
    // (they also hear the calls made to it, so only `changed` is judged)
    let after = events_for(&mut rx, 1).await;
    let changed: Vec<&Value> = after.iter().filter(|e| e["type"] == "changed").collect();
    assert!(
        changed.is_empty(),
        "the relay ended with the view: {after:?}"
    );
}

#[tokio::test]
async fn open_view_refuses_a_path_the_bridge_does_not_expose() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::create_dir_all(served(&tmp).join("docs")).unwrap();
    std::fs::create_dir_all(served(&tmp).join("secret")).unwrap();
    std::fs::write(served(&tmp).join("secret/s.txt"), "s").unwrap();
    // a second bridge exposing only `docs`
    let made = fantastic_kernel::test_support::send_settled(&k, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                   "id": "fb_docs", "root": served(&tmp).display().to_string(), "allow": ["docs"]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], "fb_docs", "{made}");
    let open_on = |kind: &str, path: &str| json!({"type": "open_view", "kind": kind, "path": path});
    let folder = call(&k, "fb_docs", open_on("folder", "secret")).await;
    assert!(
        folder.get("error").is_some() && folder.get("id").is_none(),
        "{folder}"
    );
    let file = call(&k, "fb_docs", open_on("file", "secret/s.txt")).await;
    assert!(
        file.get("error").is_some() && file.get("id").is_none(),
        "{file}"
    );
    // an exposed path is granted as usual
    let ok = call(&k, "fb_docs", open_on("folder", "docs")).await;
    assert_eq!(ok["created"], true, "{ok}");
}

#[test]
fn a_folder_view_relays_only_its_entries() {
    let at = |p: &str| json!({"path": p, "kind": "added"});
    // the folder's own entries: an added/changed/removed event, named
    let got = crate::views::relay_event(crate::views::FOLDER, "box", &at("box/n.md")).unwrap();
    assert_eq!(
        got,
        json!({"type": "added", "path": "box/n.md", "name": "n.md"})
    );
    let removed = json!({"path": "box/m.md", "kind": "deleted"});
    let got = crate::views::relay_event(crate::views::FOLDER, "box", &removed).unwrap();
    assert_eq!(got["type"], "removed");
    let modified = json!({"path": "box/m.md", "kind": "modified"});
    let got = crate::views::relay_event(crate::views::FOLDER, "box", &modified).unwrap();
    assert_eq!(got["type"], "changed");
    // the folder itself, something deeper, and another folder are not its events
    assert!(crate::views::relay_event(crate::views::FOLDER, "box", &at("box")).is_none());
    assert!(crate::views::relay_event(crate::views::FOLDER, "box", &at("box/a/deep.md")).is_none());
    assert!(crate::views::relay_event(crate::views::FOLDER, "box", &at("other/n.md")).is_none());
    // the root folder's entries are the top level only
    assert!(crate::views::relay_event(crate::views::FOLDER, ".", &at("top.md")).is_some());
    assert!(crate::views::relay_event(crate::views::FOLDER, ".", &at("a/top.md")).is_none());
}

#[test]
fn a_file_view_relays_only_its_own_path() {
    let ev = json!({"path": "live.txt", "kind": "modified"});
    let got = crate::views::relay_event(crate::views::FILE, "live.txt", &ev).unwrap();
    assert_eq!(
        got,
        json!({"type": "changed", "path": "live.txt", "kind": "modified"})
    );
    let other = json!({"path": "other.txt", "kind": "modified"});
    assert!(crate::views::relay_event(crate::views::FILE, "live.txt", &other).is_none());
    assert!(crate::views::relay_event(
        crate::views::FILE,
        "live.txt",
        &json!({"kind": "modified"})
    )
    .is_none());
}

#[tokio::test]
async fn a_view_declares_its_own_verbs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("a.txt"), "one\n").unwrap();
    std::fs::create_dir_all(served(&tmp).join("box")).unwrap();
    let f = open(&k, "file", "a.txt").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let d = open(&k, "folder", "box").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    // a view's own verbs (`watch`, a folder's `reveal`) are declared on the view alone
    let file_verbs = docs_verbs(&k, &f).await;
    assert!(file_verbs.contains(&"watch".to_string()), "{file_verbs:?}");
    assert!(!file_verbs.contains(&"reveal".to_string()), "{file_verbs:?}");
    let folder_verbs = docs_verbs(&k, &d).await;
    assert!(folder_verbs.contains(&"watch".to_string()), "{folder_verbs:?}");
    assert!(folder_verbs.contains(&"reveal".to_string()), "{folder_verbs:?}");
    let bridge_verbs = docs_verbs(&k, "fb").await;
    assert!(!bridge_verbs.contains(&"watch".to_string()), "{bridge_verbs:?}");
    assert!(!bridge_verbs.contains(&"reveal".to_string()), "{bridge_verbs:?}");
    // the bridge still answers its whole table
    assert!(docs_verbs(&k, "fb").await.contains(&"delete".to_string()));
    // and a verb a view does not answer is refused by the view
    let r = call(&k, &f, json!({"type": "delete", "path": "a.txt"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
}

#[tokio::test]
async fn open_view_takes_a_free_chosen_id_and_refuses_a_taken_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("a.txt"), "x\n").unwrap();
    std::fs::write(served(&tmp).join("b.txt"), "y\n").unwrap();
    let made = open_with(&k, "file", "a.txt", json!("fv-a")).await;
    assert_eq!(
        made,
        json!({"id": "fv-a", "kind": "file", "path": "a.txt", "created": true})
    );
    let again = open_with(&k, "file", "a.txt", json!("fv-a")).await;
    assert_eq!(
        again,
        json!({"id": "fv-a", "kind": "file", "path": "a.txt", "created": false})
    );
    // another agent's id, a view of another path, a second id for one path: refused
    assert_eq!(
        open_with(&k, "file", "a.txt", json!("fb")).await["reason"],
        "exists"
    );
    assert_eq!(
        open_with(&k, "file", "b.txt", json!("fv-a")).await["reason"],
        "exists"
    );
    assert_eq!(
        open_with(&k, "folder", "a.txt", json!("fv-a")).await["reason"],
        "exists"
    );
    let other = open_with(&k, "file", "a.txt", json!("fv-other")).await;
    assert_eq!(other["reason"], "exists", "{other}");
    assert_eq!(other["id"], "fv-a");
    let escape = format!("{}escape", "../");
    assert_eq!(
        open_with(&k, "file", "b.txt", json!(escape)).await["reason"],
        "bad_arg"
    );
    assert_eq!(
        open_with(&k, "file", "b.txt", json!(7)).await["reason"],
        "bad_arg"
    );
}

async fn open_with(k: &Arc<Kernel>, kind: &str, path: &str, id: Value) -> Value {
    call(
        k,
        "fb",
        json!({"type": "open_view", "kind": kind, "path": path, "id": id}),
    )
    .await
}

#[tokio::test]
async fn a_view_caller_is_held_to_its_path_at_the_bridge() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("a.txt"), "one\n").unwrap();
    std::fs::write(served(&tmp).join("b.txt"), "two\n").unwrap();
    std::fs::create_dir(served(&tmp).join("dir")).unwrap();
    let fv = AgentId::from(open(&k, "file", "a.txt").await["id"].as_str().unwrap());
    let dv = AgentId::from(open(&k, "folder", "dir").await["id"].as_str().unwrap());
    let fb = AgentId::from("fb");
    let read = |p: &str| p.to_string();
    let refused = view_scope(&k, &fb, Some(&fv), "read", &read("b.txt")).expect("refused");
    assert_eq!(refused["reason"], "unauthorized", "{refused}");
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("outside this file view"));
    assert!(view_scope(&k, &fb, Some(&fv), "read", &read("a.txt")).is_none());
    let watch = "dir";
    assert_eq!(
        view_scope(&k, &fb, Some(&fv), "watch_path", watch).unwrap()["reason"],
        "unauthorized"
    );
    let refused = view_scope(&k, &fb, Some(&dv), "read", &read("a.txt")).expect("refused");
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("outside this folder view"));
    let climb = format!("dir/{}a.txt", "../");
    assert!(view_scope(&k, &fb, Some(&dv), "read", &read(&climb)).is_some());
    assert!(view_scope(&k, &fb, Some(&dv), "read", &read("dir/x.txt")).is_none());
    // an operator, or a plain agent, is not path-scoped here
    assert!(view_scope(&k, &fb, None, "read", &read("b.txt")).is_none());
}

/// The bridge's binary channel, called AS `caller` (the kernel's dispatch identity).
async fn binary_as(
    k: &Arc<Kernel>,
    caller: &AgentId,
    header: Value,
    blob: &[u8],
) -> (Value, Option<Vec<u8>>) {
    let out = fantastic_kernel::test_support::as_agent(
        caller.clone(),
        fantastic_kernel::test_support::send_settled(k, vec![
            Item::new(AgentId::from("fb"), header).with_binary_key("bytes", blob.to_vec())
        ]),
    )
    .await
    .remove(0);
    (out.result, out.body.map(|b| b.bytes))
}

#[tokio::test]
async fn a_view_caller_is_held_to_its_path_on_the_binary_channel() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("a.txt"), "one\n").unwrap();
    std::fs::write(served(&tmp).join("b.txt"), "two\n").unwrap();
    let fv = AgentId::from(open(&k, "file", "a.txt").await["id"].as_str().unwrap());
    // a read or write of another path, as a binary frame, is refused as on the text channel
    let (r, body) = binary_as(&k, &fv, json!({"type": "read", "path": "b.txt"}), b"").await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("outside this file view"),
        "{r}"
    );
    assert_eq!(body, None);
    let (w, body) = binary_as(&k, &fv, json!({"type": "write", "path": "b.txt"}), b"x").await;
    assert_eq!(w["reason"], "unauthorized", "{w}");
    assert_eq!(body, None);
    assert_eq!(
        std::fs::read_to_string(served(&tmp).join("b.txt")).unwrap(),
        "two\n"
    );
    // its own path is still reached on the binary channel
    let (r, _) = binary_as(&k, &fv, json!({"type": "read", "path": "a.txt"}), b"").await;
    assert_eq!(r["lines"][0]["text"], "one", "{r}");
}

#[test]
fn a_relay_event_without_a_known_kind_is_dropped() {
    for ev in [
        json!({"path": "a.md"}),
        json!({"path": "a.md", "kind": null}),
        json!({"path": "a.md", "kind": "moved"}),
    ] {
        assert!(relay_event(FILE, "a.md", &ev).is_none(), "{ev}");
    }
    for ev in [
        json!({"path": "docs/n.md"}),
        json!({"path": "docs/n.md", "kind": "moved"}),
    ] {
        assert!(relay_event(FOLDER, "docs", &ev).is_none(), "{ev}");
    }
    let changed = json!({"path": "docs/n.md", "kind": "modified"});
    assert_eq!(
        relay_event(FOLDER, "docs", &changed).unwrap()["type"],
        "changed"
    );
}


/// The events of `type: changed` a watcher's inbox holds within `secs`, as (path, kind).
async fn changed_for(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    secs: u64,
) -> Vec<(String, String)> {
    events_for(rx, secs)
        .await
        .into_iter()
        .filter(|e| e["type"] == "changed")
        .map(|e| {
            (
                e["path"].as_str().unwrap_or("").to_string(),
                e["kind"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_view_gets_one_relayed_event_per_edit_with_stamp_kinds() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    let f = served(&tmp).join("live.txt");
    std::fs::write(&f, "one\n").unwrap();
    let v = open(&k, "file", "live.txt").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let w = AgentId::from("_t_stamp");
    let mut rx = k.ensure_inbox(&w);
    assert!(fantastic_kernel::send::as_operator(k.watch(&AgentId::from(v.as_str()), w)).await.is_none());
    call(&k, &v, json!({"type": "watch"})).await;
    tokio::time::sleep(Duration::from_millis(500)).await; // the disk watcher is up
    let live = |kind: &str| vec![("live.txt".to_string(), kind.to_string())];
    std::fs::write(&f, "two, longer\n").unwrap();
    assert_eq!(changed_for(&mut rx, 2).await, live("modified"));
    std::fs::write(&f, "three, longer still\n").unwrap();
    assert_eq!(changed_for(&mut rx, 2).await, live("modified"));
    std::fs::remove_file(&f).unwrap();
    assert_eq!(changed_for(&mut rx, 2).await, live("deleted"));
    std::fs::write(&f, "back\n").unwrap();
    assert_eq!(changed_for(&mut rx, 2).await, live("added"));
}





#[tokio::test]
async fn a_view_reflects_the_html_that_draws_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::write(served(&tmp).join("a.txt"), "x\n").unwrap();
    std::fs::create_dir(served(&tmp).join("box")).unwrap();
    let f = open(&k, "file", "a.txt").await["id"].as_str().unwrap().to_string();
    let d = open(&k, "folder", "box").await["id"].as_str().unwrap().to_string();
    let r = call(&k, &f, json!({"type": "reflect"})).await;
    let view = &r["view"];
    assert_eq!(view["kind"], "html", "{r}");
    assert_eq!(view["calls"], json!({f.as_str(): ["read", "watch"], "kernel": ["reflect"]}));
    let bytes = view["bytes"].as_str().unwrap();
    assert!(bytes.contains("PATH = \"a.txt\""), "the path is in the page");
    assert!(bytes.contains("KIND = \"file\""));
    assert!(bytes.contains(&format!("AGENT = \"{f}\"")));
    assert!(bytes.contains("function trail("), "the shared header logic is spliced in");
    assert!(!bytes.contains("export "), "no module syntax reaches the page");
    assert!(!bytes.contains("{{"), "every placeholder is filled");
    let r = call(&k, &d, json!({"type": "reflect"})).await;
    assert_eq!(r["view"]["calls"], json!({d.as_str(): ["list", "watch"], "kernel": ["reflect"]}));
    assert!(r["view"]["bytes"].as_str().unwrap().contains("KIND = \"folder\""));
}

#[test]
fn a_path_cannot_end_the_script() {
    assert_eq!(crate::views::js_string("a</script>\"b"), "\"a\\u003c/script>\\\"b\"");
}
