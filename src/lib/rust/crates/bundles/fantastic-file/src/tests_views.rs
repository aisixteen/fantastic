//! The view agents — python `file_bridge/tests/test_views.py` and `test_views_units.py`,
//! case for case where the port reaches: `open_view` reuses one view per path; a file
//! view reads, writes and refuses another path; a folder view lists inside itself and
//! refuses an escape; `reveal` shows the way down; a view's watch relays its changes.

use super::views::{deliver, relay_event, relay_inbox, FILE, FOLDER};
use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
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
        json!([
            "file:notes.md",
            "read",
            "unwatch_path",
            "watch_path",
            "write"
        ])
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

// python `test_views_units.py` — who may reach a view: the bridge's callers, for the
// verbs the view answers.

#[test]
fn a_view_admits_whoever_reaches_its_bridge_for_the_verbs_it_answers() {
    let grants = json!({"$self": "*", "$parent": "*", "user": ["read", "list", "edit", "pump"]});
    let g = grants.as_object().unwrap();
    let file = crate::views::view_callers(g, Some("rest"), crate::views::FILE);
    assert_eq!(
        Value::Object(file),
        json!({"rest": ["read", "reflect", "watch", "write"], "user": ["read"]})
    );
    let folder = crate::views::view_callers(g, Some("rest"), crate::views::FOLDER);
    assert_eq!(
        Value::Object(folder),
        json!({"rest": ["list", "reflect", "reveal", "watch"], "user": ["list"]})
    );
}

#[test]
fn the_bridges_self_is_no_caller_and_its_parent_is_named_literally() {
    let g = json!({"$self": "*", "$parent": ["read"]});
    let got = crate::views::view_callers(g.as_object().unwrap(), Some("rest"), crate::views::FILE);
    assert_eq!(Value::Object(got), json!({"rest": ["read"]}));
    let orphan = json!({"$parent": "*"});
    let got = crate::views::view_callers(orphan.as_object().unwrap(), None, crate::views::FILE);
    assert_eq!(Value::Object(got), json!({}), "no parent, no caller named");
}

#[test]
fn a_caller_with_no_verb_the_view_answers_is_not_admitted() {
    let g = json!({"ai": ["edit", "pump"]});
    let got = crate::views::view_callers(g.as_object().unwrap(), None, crate::views::FILE);
    assert_eq!(Value::Object(got), json!({}));
}

/// Every event a watcher's inbox holds within `secs` (the full window: a count is the point).
async fn events_for(rx: &mut fantastic_kernel::inbox::InboxReceiver, secs: u64) -> Vec<Value> {
    let mut out = Vec::new();
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(item)) = tokio::time::timeout_at(end, rx.recv()).await {
        out.push(item.payload);
    }
    out
}

/// The verbs `docs of=<agent>` names, sorted.
async fn docs_verbs(k: &Arc<Kernel>, of: &str) -> Vec<String> {
    let r = k
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "docs", "of": of}),
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
    assert!(k.watch(&AgentId::from(v.as_str()), w).await.is_none());
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
    assert!(k.watch(&AgentId::from(v.as_str()), w).await.is_none());
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
    tokio::time::sleep(Duration::from_millis(300)).await; // the disk watcher is up
    std::fs::write(served(&tmp).join("shared.txt"), "t\n").unwrap();
    // one relay: one event per change, not one per watch
    let events = events_for(&mut rx, 1).await;
    let changed: Vec<&Value> = events.iter().filter(|e| e["type"] == "changed").collect();
    assert_eq!(changed.len(), 1, "{events:?}");
    assert_eq!(changed[0]["path"], "shared.txt");
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
    assert!(k.watch(&AgentId::from("fb"), w).await.is_none());
    call(&k, &v, json!({"type": "watch"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // the disk watcher is up
    std::fs::write(served(&tmp).join("gone.txt"), "h\n").unwrap();
    let before = events_for(&mut rx, 1).await;
    assert!(
        before.iter().any(|e| e["type"] == "changed"),
        "the bridge sees the change while the view watches: {before:?}"
    );
    let del = k
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": v}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(del["deleted"], true, "{del}");
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
    let made = k
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                   "id": "fb_docs", "root": served(&tmp).display().to_string(),
                   "ingress_rule": "allow_all_ingress", "allow": ["docs"]}),
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
async fn a_view_documents_only_its_own_verbs() {
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
    assert_eq!(
        docs_verbs(&k, &f).await,
        ["read", "reflect", "watch", "write"]
    );
    assert_eq!(
        docs_verbs(&k, &d).await,
        ["list", "reflect", "reveal", "watch"]
    );
    // the bridge still answers its whole table
    assert!(docs_verbs(&k, "fb").await.contains(&"delete".to_string()));
    // and a verb a view does not answer is not in its docs, nor its advertisement
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
    let read = |p: &str| json!({"type": "read", "path": p});
    let refused = view_scope(&k, &fb, Some(&fv), "read", &read("b.txt")).expect("refused");
    assert_eq!(refused["reason"], "unauthorized", "{refused}");
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("outside this file view"));
    assert!(view_scope(&k, &fb, Some(&fv), "read", &read("a.txt")).is_none());
    let watch = json!({"path": "dir"});
    assert_eq!(
        view_scope(&k, &fb, Some(&fv), "watch_path", &watch).unwrap()["reason"],
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
    assert_eq!(
        view_scope(&k, &fb, Some(&fv), "read", &json!({"path": 7})).unwrap()["reason"],
        "bad_arg"
    );
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

/// Replace one caller's entry in the bridge's `callable_by` (the scope a view is held to).
fn set_callable_by(k: &Arc<Kernel>, bridge: &str, caller: &str, entry: Value) {
    let b = k.agents.get(&AgentId::from(bridge)).unwrap();
    let mut cb = b
        .record()
        .meta
        .get("callable_by")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    cb.insert(caller.to_string(), entry);
    b.update_meta(Map::from_iter([(
        "callable_by".to_string(),
        Value::Object(cb),
    )]));
}

#[tokio::test]
async fn the_bridge_holds_a_view_to_the_scope_in_its_callable_by() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::create_dir_all(served(&tmp).join("dir/sub")).unwrap();
    std::fs::write(served(&tmp).join("dir/x.txt"), "x\n").unwrap();
    let dv = open(&k, "folder", "dir").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let fb = AgentId::from("fb");
    let read = |p: &str| json!({"type": "read", "path": p});
    assert!(view_scope(
        &k,
        &fb,
        Some(&AgentId::from(dv.as_str())),
        "read",
        &read("dir/x.txt")
    )
    .is_none());
    // the bridge's entry narrows the view to `dir/sub`: `dir/x.txt` is refused at the
    // bridge, although the view's own record still says `dir`
    set_callable_by(
        &k,
        "fb",
        &dv,
        json!(["folder:dir/sub", "list", "watch_path", "unwatch_path"]),
    );
    let dv_id = AgentId::from(dv.as_str());
    let refused = view_scope(&k, &fb, Some(&dv_id), "read", &read("dir/x.txt")).expect("refused");
    assert_eq!(refused["reason"], "unauthorized", "{refused}");
    assert!(view_scope(&k, &fb, Some(&dv_id), "list", &json!({"path": "dir/sub"})).is_none());
    // an entry with no single scope token is a view refused in full
    set_callable_by(&k, "fb", &dv, json!(["list", "watch_path", "unwatch_path"]));
    let refused =
        view_scope(&k, &fb, Some(&dv_id), "list", &json!({"path": "dir/sub"})).expect("refused");
    assert_eq!(refused["reason"], "unauthorized", "{refused}");
}

#[tokio::test]
async fn the_bridge_relays_a_view_only_the_changes_in_its_scope() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(&tmp).await;
    std::fs::create_dir(served(&tmp).join("dir")).unwrap();
    std::fs::write(served(&tmp).join("other.txt"), "o\n").unwrap();
    let dv = open(&k, "folder", "dir").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let fb = AgentId::from("fb");
    // the view's relay inbox, and the holder of `dir` the bridge pushes to
    let mut rx = k.ensure_inbox(&relay_inbox(&dv));
    let holders: BTreeMap<String, BTreeSet<String>> =
        BTreeMap::from([("dir".to_string(), BTreeSet::from([dv.clone()]))]);
    let none = |rx: &mut fantastic_kernel::inbox::InboxReceiver| rx.try_recv().is_none();
    crate::watch::relay_to_views(&k, &fb, &holders, "dir/a.md", "added");
    let got = rx.try_recv().expect("in scope").payload;
    assert_eq!(
        got,
        json!({"type": "changed", "path": "dir/a.md", "kind": "added"})
    );
    crate::watch::relay_to_views(&k, &fb, &holders, "other.txt", "modified");
    assert!(none(&mut rx), "outside the view's folder");
    // the entry narrows the view to `dir/sub`: `dir/a.md` is no longer relayed
    set_callable_by(
        &k,
        "fb",
        &dv,
        json!(["folder:dir/sub", "list", "watch_path"]),
    );
    crate::watch::relay_to_views(&k, &fb, &holders, "dir/a.md", "added");
    assert!(none(&mut rx), "narrowed scope");
    // no single scope token: fail closed, nothing relayed
    set_callable_by(&k, "fb", &dv, json!(["list", "watch_path"]));
    crate::watch::relay_to_views(&k, &fb, &holders, "dir/a.md", "added");
    assert!(none(&mut rx), "no scope token");
    // a view with no relay inbox gets nothing, and the drop is reported
    assert!(!deliver(&k, "gone", json!({"type": "changed"})));
}
