//! The agent, through `HomeBundle::handle` on a `Kernel::new()` with `home`
//! registered (the pattern of the `user_store.rs` tests), on a scripted machine.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fantastic_home::{Config as HomeConfig, Mode};
use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::Bundle;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use super::HomeBundle;
use crate::home::fake::{run, FakeOs, Folder};
use crate::home::{HomeOs, ID, VERBS};

struct T {
    kernel: Arc<Kernel>,
    os: Arc<FakeOs>,
    bundle: HomeBundle,
    id: AgentId,
}

/// A kernel holding a root (running dir `/h/.fantastic/app`) and `home`, whose
/// record is `meta`.
fn t_with(meta: Value, config: HomeConfig) -> T {
    let kernel = Arc::new(Kernel::new());
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Default::default(),
        PathBuf::from("/h/.fantastic/app/.fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    let id = AgentId::from(ID);
    let meta = meta.as_object().cloned().unwrap_or_default();
    kernel.register(Agent::new(
        id.clone(),
        Some(crate::home::HANDLER_MODULE.into()),
        Some(AgentId::from("kernel_state")),
        meta,
        PathBuf::new(),
        false,
    ));
    let os = Arc::new(FakeOs::new());
    let bundle = HomeBundle::new(Arc::clone(&os) as Arc<dyn HomeOs>, config);
    T {
        kernel,
        os,
        bundle,
        id,
    }
}

fn t() -> T {
    t_with(json!({}), HomeConfig::new(Mode::Lock))
}

fn entry(guid: &str, path: &str, pid: Option<u32>, port: Option<u16>) -> (String, String) {
    let mut yaml = format!("guid: {guid}\npath: {path}\nstored_at: 1\nlast_seen: 1760800000\n");
    if let Some(pid) = pid {
        yaml.push_str(&format!("pid: {pid}\n"));
    }
    if let Some(port) = port {
        yaml.push_str(&format!(
            "canvas: http://127.0.0.1:{port}/canvas_page/file/index.html\n"
        ));
    }
    (format!("f-{guid}"), yaml)
}

impl T {
    async fn call(&self, payload: Value) -> Value {
        self.bundle
            .handle(&self.id, &payload, &self.kernel)
            .await
            .expect("handled")
            .expect("a reply")
    }

    /// Boot, and wait for the first look to list the home.
    async fn boot(&self) -> Value {
        let r = self.call(json!({"type": "boot"})).await;
        for _ in 0..1000 {
            if self.bundle.runner().read(|h| h.listed()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        self.bundle.runner().settle().await;
        assert!(
            self.bundle.runner().read(|h| h.listed()),
            "listed after boot"
        );
        r
    }

    fn running(&self, path: &str, pid: u32, port: u16, guid: &str) {
        self.os.set_folder(
            path,
            Folder {
                is_fantastic: true,
                holder: Some(pid),
                run: Some(run(pid, port)),
            },
        );
        self.os.set_canvas_up(port, true);
        self.os.set_guid(port, guid);
    }

    fn stopped(&self, path: &str) {
        self.os.set_folder(
            path,
            Folder {
                is_fantastic: true,
                holder: None,
                run: None,
            },
        );
    }

    async fn shutdown(&self) {
        self.call(json!({"type": "shutdown"})).await;
    }
}

fn refused(r: &Value, reason: &str, error: &str) {
    assert_eq!(r["reason"], json!(reason), "{r}");
    assert_eq!(r["error"], json!(error), "{r}");
}

#[tokio::test]
async fn list_answers_the_state_shape() {
    let t = t();
    let mut notes = entry("g-1", "/home/a/notes", Some(4242), Some(8123));
    notes.1.push_str("name: notes\n");
    t.os.set_registry(vec![notes]);
    t.running("/home/a/notes", 4242, 8123, "g-1");
    t.os.set_log(
        "/home/a/notes",
        "/home/a/.local/state/fantastic/logs/notes-1a2b3c4d.log",
        "listening",
    );
    t.boot().await;
    t.call(json!({"type": "select", "id": "g-1"})).await;
    let mut got = t.call(json!({"type": "list"})).await;
    assert!(got["seq"].as_u64().is_some_and(|s| s >= 1), "{got}");
    got["seq"] = json!(7);
    let golden = json!({"type":"state","protocol":"fantastic.home","version":1,"seq":7,"mode":"lock","listed":true,
        "rows":[{"id":"g-1","guid":"g-1","name":"notes","path":"/home/a/notes","last_seen":1760800000,
                 "status":"running","canvas":{"url":"http://127.0.0.1:8123/canvas_page/file/index.html","port":8123},
                 "pid":4242,"actions":["stop","open","remove"],"log":"/home/a/.local/state/fantastic/logs/notes-1a2b3c4d.log"}],
        "pages":["g-1"],"selection":"g-1","problem":null});
    assert_eq!(got, golden);
    // the keys in the documented order
    let keys: Vec<&String> = got.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "type",
            "protocol",
            "version",
            "seq",
            "mode",
            "listed",
            "rows",
            "pages",
            "selection",
            "problem"
        ]
    );
    let row_keys: Vec<&String> = got["rows"][0].as_object().unwrap().keys().collect();
    assert_eq!(
        row_keys,
        [
            "id",
            "guid",
            "name",
            "path",
            "last_seen",
            "status",
            "canvas",
            "pid",
            "actions",
            "log"
        ]
    );
    t.shutdown().await;
}

#[tokio::test]
async fn an_unknown_id_is_not_found() {
    let t = t();
    t.boot().await;
    for verb in ["start", "stop", "remove", "log", "select"] {
        let r = t.call(json!({"type": verb, "id": "nope"})).await;
        refused(&r, "not_found", "home: no fantastic \"nope\" in the list");
        assert_eq!(r["arg"], json!("id"));
    }
    let r = t.call(json!({"type": "start"})).await;
    refused(&r, "missing_arg", "home: start: id required");
    t.shutdown().await;
}

#[tokio::test]
async fn open_refuses_a_relative_path_or_a_file_and_opens_nothing() {
    let t = t();
    t.boot().await;
    let before = t.call(json!({"type": "list"})).await;
    let r = t.call(json!({"type": "open", "paths": ["notes"]})).await;
    refused(
        &r,
        "bad_arg",
        "home: open: \"notes\" is not an absolute path",
    );
    assert_eq!(r["arg"], json!("paths"));
    t.os.set_resolve("/w/file.txt", Err("/w/file.txt is not a directory".into()));
    let r = t
        .call(json!({"type": "open", "paths": ["/w/ok", "/w/file.txt"]}))
        .await;
    refused(&r, "bad_arg", "home: open: \"/w/ok\" is not a folder");
    t.os.set_resolve("/w/ok", Ok("/w/ok".into()));
    let r = t
        .call(json!({"type": "open", "paths": ["/w/ok", "/w/file.txt"]}))
        .await;
    refused(&r, "bad_arg", "home: open: \"/w/file.txt\" is not a folder");
    assert_eq!(r["hint"], json!("/w/file.txt is not a directory"));
    for (payload, reason) in [
        (json!({"type": "open"}), "missing_arg"),
        (json!({"type": "open", "paths": []}), "missing_arg"),
        (json!({"type": "open", "paths": "/w/ok"}), "bad_arg"),
        (json!({"type": "open", "paths": [7]}), "bad_arg"),
    ] {
        let r = t.call(payload.clone()).await;
        assert_eq!(r["reason"], json!(reason), "{payload}: {r}");
        assert_eq!(r["arg"], json!("paths"), "{payload}: {r}");
    }
    t.bundle.runner().settle().await;
    assert!(
        t.os.spawned().is_empty(),
        "nothing was opened, so nothing started"
    );
    let after = t.call(json!({"type": "list"})).await;
    assert_eq!(after, before, "a refused call changes nothing");
    t.shutdown().await;
}

#[tokio::test]
async fn open_and_register_refuse_the_homes_own_folder() {
    let t = t_with(
        json!({}),
        HomeConfig::new(Mode::Lock).own_folder("/h/.fantastic/app".into()),
    );
    t.os.set_resolve("/h/.fantastic/app", Ok("/h/.fantastic/app".into()));
    t.os.set_resolve("/h/.fantastic/app/", Ok("/h/.fantastic/app".into()));
    t.boot().await;
    let r = t
        .call(json!({"type": "open", "paths": ["/h/.fantastic/app/"]}))
        .await;
    refused(
        &r,
        "bad_arg",
        "home: open: \"/h/.fantastic/app/\" is the home's own folder — it is never listed",
    );
    let r = t
        .call(json!({"type": "register", "path": "/h/.fantastic/app"}))
        .await;
    refused(
        &r,
        "bad_arg",
        "home: register: \"/h/.fantastic/app\" is the home's own folder — it is never listed",
    );
    assert_eq!(r["arg"], json!("path"));
    t.bundle.runner().settle().await;
    assert!(t.os.spawned().is_empty());
    t.shutdown().await;
}

#[tokio::test]
async fn register_refuses_a_listed_folder() {
    let t = t();
    t.os.set_registry(vec![entry("g1", "/w/atlas", None, None)]);
    t.stopped("/w/atlas");
    t.os.set_resolve("/w/link-to-atlas", Ok("/w/atlas".into()));
    t.boot().await;
    for path in ["/w/atlas", "/w/link-to-atlas"] {
        let r = t.call(json!({"type": "register", "path": path})).await;
        refused(
            &r,
            "bad_arg",
            "home: atlas is in the list already — open it",
        );
    }
    // a new folder is added, as open adds it
    t.os.set_resolve("/w/new", Ok("/w/new".into()));
    let s = t.call(json!({"type": "register", "path": "/w/new"})).await;
    assert_eq!(s["selection"], json!("adding:/w/new"));
    t.shutdown().await;
}

#[tokio::test]
async fn start_refuses_a_running_entry() {
    let t = t();
    t.os.set_registry(vec![entry("g2", "/w/b", Some(7), Some(8100))]);
    t.running("/w/b", 7, 8100, "g2");
    t.boot().await;
    let r = t.call(json!({"type": "start", "id": "g2"})).await;
    refused(&r, "bad_arg", "home: b cannot start now — it is running");
    assert_eq!(r["arg"], json!("id"));
    t.bundle.runner().settle().await;
    assert!(t.os.spawned().is_empty());
    t.shutdown().await;
}

#[tokio::test]
async fn select_page_out_of_range_is_refused() {
    let t = t();
    t.os.set_registry(vec![entry("g1", "/w/a", None, None)]);
    t.stopped("/w/a");
    t.boot().await;
    let r = t.call(json!({"type": "select_page", "index": 0})).await;
    refused(&r, "bad_arg", "home: select_page: no page 0 (there are 0)");
    t.call(json!({"type": "select", "id": "g1"})).await;
    t.call(json!({"type": "select", "id": null})).await;
    let r = t.call(json!({"type": "select_page", "index": 1})).await;
    refused(&r, "bad_arg", "home: select_page: no page 1 (there are 1)");
    let r = t.call(json!({"type": "select_page", "index": -1})).await;
    assert_eq!(r["reason"], json!("bad_arg"));
    let s = t.call(json!({"type": "select_page", "index": 0})).await;
    assert_eq!(s["selection"], json!("g1"));
    let r = t.call(json!({"type": "close_page", "id": "g9"})).await;
    refused(&r, "not_found", "home: no page \"g9\" is open");
    t.shutdown().await;
}

#[tokio::test]
async fn log_answers_the_tail_or_not_found() {
    let t = t();
    t.os.set_registry(vec![
        entry("g1", "/w/a", None, None),
        entry("g2", "/w/b", None, None),
    ]);
    t.stopped("/w/a");
    t.stopped("/w/b");
    t.os.set_log("/w/a", "/logs/a-12345678.log", "line 3\nline 4");
    t.boot().await;
    let r = t.call(json!({"type": "log", "id": "g1"})).await;
    assert_eq!(
        r,
        json!({"id": "g1", "file": "/logs/a-12345678.log", "tail": "line 3\nline 4"})
    );
    let r = t.call(json!({"type": "log", "id": "g2"})).await;
    refused(&r, "not_found", "home: no launch log for b");
    t.shutdown().await;
}

#[tokio::test]
async fn focus_emits_focus_and_changes_nothing() {
    let t = t();
    t.boot().await;
    let watcher = AgentId::from("w");
    let mut inbox = t.kernel.ensure_inbox(&watcher);
    assert!(t.kernel.watch(&t.id, watcher.clone()).await.is_none());
    let before = t.call(json!({"type": "list"})).await;
    let after = t.call(json!({"type": "focus"})).await;
    assert_eq!(after, before, "the state and its seq are unchanged");
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let item = inbox.recv().await.expect("an inbox item");
            if item.payload["type"] == json!("focus") {
                return item.payload;
            }
        }
    })
    .await
    .expect("a focus within 5 s");
    assert_eq!(seen, json!({"type": "focus"}));
    t.shutdown().await;
}

#[tokio::test]
async fn boot_restores_pages_and_selection_from_the_record() {
    let t = t_with(
        json!({"pages": ["g2", "gone", "g1"], "selection": "g1"}),
        HomeConfig::new(Mode::Lock),
    );
    t.os.set_registry(vec![
        entry("g1", "/w/a", None, None),
        entry("g2", "/w/b", None, None),
    ]);
    t.stopped("/w/a");
    t.stopped("/w/b");
    assert_eq!(t.boot().await, json!({"booted": true}));
    let s = t.call(json!({"type": "list"})).await;
    assert_eq!(
        (s["pages"].clone(), s["selection"].clone()),
        (json!(["g2", "g1"]), json!("g1"))
    );
    // the kept values now on the record, once they changed
    let record = t
        .kernel
        .agents
        .get(&t.id)
        .unwrap()
        .value()
        .record()
        .meta
        .clone();
    assert_eq!(
        (record["pages"].clone(), record["selection"].clone()),
        (json!(["g2", "g1"]), json!("g1"))
    );
    // a second boot changes nothing
    assert_eq!(
        t.call(json!({"type": "boot"})).await,
        json!({"booted": true})
    );
    t.shutdown().await;
    assert!(!t.bundle.runner().polling());
}

#[tokio::test]
async fn boot_refuses_a_malformed_pages_record() {
    let t = t_with(json!({"pages": "g1"}), HomeConfig::new(Mode::Lock));
    let r = t.call(json!({"type": "boot"})).await;
    refused(
        &r,
        "bad_arg",
        "home: its record's `pages` must be a list of ids, got \"g1\"",
    );
    assert!(
        !t.bundle.runner().polling(),
        "a refused boot starts no poll"
    );
    let t = t_with(json!({"pages": ["g1", 2]}), HomeConfig::new(Mode::Lock));
    let r = t.call(json!({"type": "boot"})).await;
    refused(
        &r,
        "bad_arg",
        "home: its record's `pages` must be a list of ids, got [\"g1\",2]",
    );
    let t = t_with(json!({"selection": 3}), HomeConfig::new(Mode::Lock));
    let r = t.call(json!({"type": "boot"})).await;
    refused(
        &r,
        "bad_arg",
        "home: its record's `selection` must be an id or null, got 3",
    );
}

#[tokio::test]
async fn verb_docs_are_verbs_plus_boot_and_shutdown() {
    let t = t();
    let docs = t.bundle.verb_docs();
    let keys: Vec<&str> = docs
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut want: Vec<&str> = VERBS.to_vec();
    want.extend(["boot", "shutdown"]);
    assert_eq!(keys, want);
    assert!(docs
        .as_object()
        .unwrap()
        .values()
        .all(|d| d.as_str().is_some_and(|s| !s.is_empty())));
}

#[tokio::test]
async fn an_unknown_verb_is_unhandled() {
    let t = t();
    let r = t.call(json!({"type": "nope"})).await;
    refused(&r, "unhandled", "home: unknown type \"nope\"");
    assert_eq!(r["verb"], json!("nope"));
}

#[tokio::test]
async fn reflect_names_the_protocol_and_version() {
    let t = t();
    let r = t.call(json!({"type": "reflect"})).await;
    assert_eq!(r["id"], json!("home"));
    assert_eq!(r["protocol"], json!("fantastic.home"));
    assert_eq!(r["version"], json!(1));
    assert_eq!(r["mode"], json!("lock"));
    assert_eq!(r["home"], json!("/h"));
    assert_eq!(r["poll_ms"], json!(2000));
    assert!(r["sentence"].as_str().is_some_and(|s| !s.is_empty()));
}
