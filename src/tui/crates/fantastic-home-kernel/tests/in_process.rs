//! THE HOME KERNEL, IN-PROCESS: each test boots a real `HomeKernel` on a temp
//! home with a free port and `fantastic_bin` pointing at a temp file, and drives
//! it through its API and its real page socket.
//!
//! ONE AT A TIME: the `web` bundle keeps its live servers in one process-wide
//! table keyed by agent id, so two home kernels in one process would share one
//! `web`. Every test holds [`SERIAL`] and shuts its kernel down.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use fantastic_home_kernel::home::sys;
use fantastic_home_kernel::{hand_over, running, Config, HomeKernel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Launch logs go to a temp dir, never the real user's.
fn quiet_logs() {
    static LOGS: OnceLock<PathBuf> = OnceLock::new();
    let dir = LOGS.get_or_init(|| tempfile::tempdir().expect("a temp dir").keep());
    std::env::set_var("XDG_STATE_HOME", dir);
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        quiet_logs();
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = std::fs::canonicalize(dir.path()).expect("the temp dir resolves");
        let bin = root.join("fantastic");
        std::fs::write(&bin, b"not run here").expect("the fake binary");
        // The home is private, as the user store makes it (0700, or a DACL).
        let home = root.join("home");
        fantastic_user_home_store::ensure_private_dir(&home, &home).expect("a private home");
        Fixture {
            _dir: dir,
            home,
            bin,
            root,
        }
    }

    fn workdir(&self) -> PathBuf {
        self.home.join(".fantastic").join("app")
    }

    fn config(&self, port: Option<u16>) -> Config {
        Config {
            home: self.home.clone(),
            port,
            fantastic_bin: self.bin.clone(),
        }
    }

    async fn boot(&self) -> HomeKernel {
        HomeKernel::boot(self.config(Some(free_port())))
            .await
            .expect("the home boots")
    }

    /// A folder that is a fantastic (its `.fantastic/` exists), canonical.
    fn fantastic_folder(&self, name: &str) -> String {
        let f = self.root.join(name);
        std::fs::create_dir_all(f.join(".fantastic")).expect("a fantastic folder");
        f.display().to_string()
    }

    /// A registry entry for `path` under `guid`.
    fn register(&self, guid: &str, path: &str) {
        let dir = self.home.join("fantastics").join(guid);
        std::fs::create_dir_all(&dir).expect("a registry folder");
        let yaml = format!("guid: {guid}\nname: null\npath: {path}\npid: null\ncanvas: null\nstored_at: 1\nlast_seen: 2\n");
        std::fs::write(dir.join("fantastic.yaml"), yaml).expect("a registry entry");
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
    l.local_addr().expect("its address").port()
}

/// Every file under `dir`, relative.
fn files(dir: &Path) -> BTreeSet<String> {
    fn walk(base: &Path, at: &Path, out: &mut BTreeSet<String>) {
        let Ok(read) = std::fs::read_dir(at) else {
            return;
        };
        for e in read.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(base)
                        .expect("under base")
                        .display()
                        .to_string(),
                );
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

fn yaml(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `list` until `ok` holds of the state, or panic after 10 s.
async fn until(hk: &HomeKernel, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let state = hk
            .call(json!({"type": "list"}))
            .await
            .expect("list answers");
        if ok(&state) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the home never got to: {what}");
}

/// The next payload on `rx` whose `type` is `kind`, within 10 s.
async fn next_of(rx: &mut tokio::sync::mpsc::Receiver<Value>, kind: &str) -> Value {
    let wait = async {
        while let Some(p) = rx.recv().await {
            if p.get("type").and_then(Value::as_str) == Some(kind) {
                return p;
            }
        }
        panic!("the events closed before a {kind}");
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| panic!("no {kind} in 10 s"))
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_refuses_a_relative_home_or_a_missing_fantastic_bin() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let e = HomeKernel::boot(Config {
        home: "rel/home".into(),
        port: None,
        fantastic_bin: f.bin.clone(),
    })
    .await
    .unwrap_err();
    assert_eq!(e, "the home dir must be an absolute path, got rel/home");
    let e = HomeKernel::boot(Config {
        home: f.home.clone(),
        port: None,
        fantastic_bin: "fantastic".into(),
    })
    .await
    .unwrap_err();
    assert_eq!(e, "fantastic_bin must be an absolute path, got fantastic");
    let missing = f.root.join("no-such-binary");
    let e = HomeKernel::boot(Config {
        home: f.home.clone(),
        port: None,
        fantastic_bin: missing.clone(),
    })
    .await
    .unwrap_err();
    assert_eq!(
        e,
        format!("fantastic_bin {} is not a file", missing.display())
    );
    assert!(!f.workdir().exists(), "a refused config writes nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_registry_holds_exactly_the_home_bundles() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    let r = sys::forward(
        hk.port(),
        "kernel",
        json!({"type": "reflect", "bundles": "all", "tree": "none"}),
        Duration::from_secs(5),
    )
    .await
    .expect("the root reflects");
    let modules: BTreeSet<String> = r["bundles"]
        .as_array()
        .expect("a bundle catalog")
        .iter()
        .filter_map(|b| b["handler_module"].as_str().map(str::to_string))
        .collect();
    let want: BTreeSet<String> = [
        "kernel_state.tools",
        "file_bridge.tools",
        "sqlite_store.tools",
        "user.tools",
        "web.tools",
        "web_ws.tools",
        "canvas_page.tools",
        "home.tools",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(modules, want);
    for absent in [
        "exec_bridge.tools",
        "ram_store.tools",
        "user_store.tools",
        "user_home_store.tools",
    ] {
        assert!(!modules.contains(absent), "{absent} is not registered");
    }
    hk.shutdown().await.expect("a clean stop");
}

/// The grant table, as the records on disk hold it (the kernel merges the
/// `$self` baseline in RAM; it is not written).
fn table() -> Vec<(&'static str, &'static str, Value, Value)> {
    let verbs = json!([
        "list",
        "open",
        "register",
        "start",
        "stop",
        "remove",
        "select",
        "select_page",
        "close_page",
        "dismiss",
        "log",
        "focus",
        "reflect"
    ]);
    let loader = json!(["load_tree", "persist_record", "forget_record"]);
    vec![
        // (record path under W/.fantastic, id, can_call, callable_by)
        (
            "agent.yaml",
            "kernel_state",
            json!({}),
            json!({"user": ["reflect", "docs"]}),
        ),
        (
            "agents/home_memory/agent.yaml",
            "home_memory",
            json!({}),
            json!({}),
        ),
        ("agents/store/agent.yaml", "store", json!({}), json!({})),
        (
            "agents/user/agent.yaml",
            "user",
            json!({"home": verbs, "kernel_state": ["reflect", "docs"], "web_loader": loader}),
            json!({"home_ws": ["forward"]}),
        ),
        (
            "agents/user/agents/home/agent.yaml",
            "home",
            json!({}),
            json!({"user": verbs, "home_ws": ["watch"]}),
        ),
        (
            "agents/user/agents/web/agent.yaml",
            "web",
            json!({"home_ws": ["get_routes"], "canvas_page": ["read_stream"]}),
            json!({}),
        ),
        (
            "agents/user/agents/web/agents/home_ws/agent.yaml",
            "home_ws",
            json!({"user": ["forward"], "home": ["watch"]}),
            json!({"web": ["get_routes"]}),
        ),
        (
            "agents/user/agents/web/agents/canvas_page/agent.yaml",
            "canvas_page",
            json!({}),
            json!({"web": ["read_stream"]}),
        ),
        (
            "agents/user/agents/web/agents/canvas_page/agents/web_loader/agent.yaml",
            "web_loader",
            json!({}),
            json!({"user": loader}),
        ),
    ]
}

fn check_grants(w: &Path) {
    for (rel, id, can_call, callable_by) in table() {
        let rec = yaml(&w.join(".fantastic").join(rel));
        assert_eq!(rec["id"], json!(id), "{rel}");
        assert_eq!(rec["can_call"], can_call, "{id}.can_call");
        assert_eq!(rec["callable_by"], callable_by, "{id}.callable_by");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_builds_the_tree_and_writes_the_grants_whole() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    hk.shutdown().await.expect("a clean stop");
    // Disk position is the parent: each record is where the tree puts it.
    check_grants(&f.workdir());

    // A stray grant written into a record is gone after the next boot.
    let home_rec = f
        .workdir()
        .join(".fantastic/agents/user/agents/home/agent.yaml");
    let mut rec = yaml(&home_rec);
    rec["callable_by"]["stranger"] = json!("*");
    rec["can_call"]["store"] = json!("*");
    std::fs::write(&home_rec, serde_yaml::to_string(&rec).unwrap()).unwrap();
    let hk = f.boot().await;
    hk.shutdown().await.expect("a clean stop");
    check_grants(&f.workdir());
}

#[tokio::test(flavor = "multi_thread")]
async fn records_land_where_the_layout_table_says() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    let w = f.workdir();
    assert!(
        w.join(".fantastic/lock.yaml").is_file(),
        "the lock is held for the host's life"
    );
    let run = yaml(&w.join("home.yaml"));
    assert_eq!(
        run,
        json!({"pid": std::process::id(), "port": hk.port(), "url": hk.url()})
    );
    assert_eq!(
        hk.url(),
        format!("http://127.0.0.1:{}/canvas_page/file/home.html", hk.port())
    );
    hk.shutdown().await.expect("a clean stop");
    let root = yaml(&w.join(".fantastic/agent.yaml"));
    assert!(
        root["guid"].as_str().is_some_and(|g| !g.is_empty()),
        "{root}"
    );
    assert!(root["next_id"].is_u64(), "{root}");
    assert_eq!(root["memory_store_id"], json!("home_memory"));
    assert!(w.join(".fantastic/home.db").is_file());
    assert!(w
        .join(".fantastic/agents/user/agents/home/agent.yaml")
        .is_file());
    // Nothing is written under <home> outside W.
    let outside: Vec<String> = files(&f.home)
        .into_iter()
        .filter(|p| !Path::new(p).starts_with(Path::new(".fantastic").join("app")))
        .collect();
    assert!(outside.is_empty(), "{outside:?}");
    let levels: Vec<String> = std::fs::read_dir(&f.home)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(levels, [".fantastic"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_socket_reaches_only_its_grants() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    let port = hk.port();
    let ask = |target: &'static str, payload: Value| {
        sys::forward(port, target, payload, Duration::from_secs(5))
    };
    // allowed
    let list = ask("home", json!({"type": "list"}))
        .await
        .expect("home list");
    assert_eq!(list["type"], json!("state"));
    let reflect = ask("kernel", json!({"type": "reflect", "tree": "none"}))
        .await
        .expect("kernel reflect");
    assert!(reflect["guid"].as_str().is_some_and(|g| !g.is_empty()));
    ask("kernel", json!({"type": "docs", "of": "home"}))
        .await
        .expect("kernel docs");
    ask("web_loader", json!({"type": "load_tree"}))
        .await
        .expect("web_loader load_tree");
    // refused
    for (target, payload) in [
        (
            "kernel",
            json!({"type": "update_agent", "id": "home_ws", "can_call": {"store": "*"}}),
        ),
        (
            "kernel",
            json!({"type": "create_agent", "parent_id": "user", "handler_module": "file_bridge.tools"}),
        ),
        ("kernel", json!({"type": "list_agents"})),
        ("home_memory", json!({"type": "read"})),
        (
            "store",
            json!({"type": "read_stream", "path": "agent.yaml"}),
        ),
        ("web", json!({"type": "reflect"})),
    ] {
        let verb = payload["type"].clone();
        let e = ask(target, payload).await.expect_err("refused");
        assert!(
            e.contains("not permitted") || e.contains("unauthorized") || e.contains("may not"),
            "{target} {verb}: {e}"
        );
    }
    // `user`'s own verb, as a frame straight to `user`: the leg holds only `forward`.
    let r = raw_call(
        port,
        json!({"type": "call", "target": "user", "payload": {"type": "path"}, "id": "p-1"}),
    )
    .await;
    assert_eq!(r["type"], json!("reply"), "{r}");
    assert!(r["data"]["error"].is_string(), "{r}");
    assert_eq!(r["data"]["reason"], json!("unauthorized"), "{r}");
    // A foreign browser Origin is refused at the handshake.
    let mut req = format!("ws://127.0.0.1:{port}/user/ws")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Origin", "http://example.invalid".parse().unwrap());
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    match tokio_tungstenite::client_async(req, stream).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 403)
        }
        other => panic!(
            "a foreign origin must be refused with 403, got {:?}",
            other.map(|(_, r)| r.status())
        ),
    }
    hk.shutdown().await.expect("a clean stop");
}

/// One frame over `/user/ws`; the reply or error frame naming its id.
async fn raw_call(port: u16, frame: Value) -> Value {
    let url = format!("ws://127.0.0.1:{port}/user/ws");
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(url.as_str(), stream)
        .await
        .unwrap();
    ws.send(Message::Text(frame.to_string())).await.unwrap();
    let id = frame["id"].clone();
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["id"] == id {
                    return v;
                }
            }
        }
        panic!("the socket closed");
    })
    .await
    .expect("an answer in 5 s");
    let _ = ws.close(None).await;
    got
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_boot_of_one_home_is_refused_naming_the_pid() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    let e = HomeKernel::boot(f.config(None)).await.unwrap_err();
    assert_eq!(
        e,
        format!(
            "{}: already running (pid {})",
            f.home.display(),
            std::process::id()
        )
    );
    hk.shutdown().await.expect("a clean stop");
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_then_shutdown_releases_the_lock_and_removes_the_run_file() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    let w = f.workdir();
    assert_eq!(
        fantastic_kernel::lock::held(&w).unwrap(),
        Some(std::process::id())
    );
    assert!(w.join("home.yaml").is_file());
    hk.shutdown().await.expect("a clean stop");
    assert_eq!(fantastic_kernel::lock::held(&w).unwrap(), None);
    assert!(!w.join("home.yaml").exists());
    // A stopped home boots again.
    let hk = f.boot().await;
    hk.shutdown().await.expect("a clean stop");
}

#[tokio::test(flavor = "multi_thread")]
async fn running_is_none_without_a_live_lock_holder_and_names_the_host_with_one() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    assert_eq!(running(&f.home).await.unwrap(), None);
    let hk = f.boot().await;
    let r = running(&f.home).await.unwrap().expect("the host is named");
    assert_eq!(
        (r.pid, r.port, r.url.as_str()),
        (std::process::id(), hk.port(), hk.url())
    );
    hk.shutdown().await.expect("a clean stop");
    assert_eq!(running(&f.home).await.unwrap(), None);
    // A lock held with no run file naming its holder is not a home host.
    let w = f.workdir();
    fantastic_kernel::lock::acquire(&w).unwrap();
    let e = running(&f.home).await.unwrap_err();
    assert_eq!(
        e,
        format!(
            "{} is held by pid {}, but no run file names it a home host",
            f.home.display(),
            std::process::id()
        )
    );
    fantastic_kernel::lock::release(&w).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn call_is_user_forward_and_a_refusal_is_err() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    let mut rx = hk.events();
    let state = hk.call(json!({"type": "list"})).await.expect("list");
    assert_eq!(
        (state["type"].clone(), state["protocol"].clone()),
        (json!("state"), json!("fantastic.home"))
    );
    // the call reached `home` as a send, mirrored to its watchers
    next_of(&mut rx, "list").await;
    // a verb the grants do not name never reaches `home`: `user` may not send it
    let e = hk.call(json!({"type": "nope"})).await.unwrap_err();
    assert!(e.starts_with("user may not call 'nope' on home"), "{e}");
    // a refusal of `home`'s own
    let e = hk
        .call(json!({"type": "close_page", "id": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e, "home: no page \"x\" is open");
    let e = hk
        .call(json!({"type": "open", "paths": ["relative"]}))
        .await
        .unwrap_err();
    assert_eq!(e, "home: open: \"relative\" is not an absolute path");
    hk.shutdown().await.expect("a clean stop");
}

#[tokio::test(flavor = "multi_thread")]
async fn events_carries_every_payload_on_home() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    let mut rx = hk.events();
    let mut second = hk.events();
    hk.call(json!({"type": "dismiss"})).await.expect("dismiss");
    let mirrored = next_of(&mut rx, "dismiss").await;
    assert_eq!(mirrored, json!({"type": "dismiss"}));
    hk.call(json!({"type": "focus"})).await.expect("focus");
    // the mirrored call, then the emit: both `focus`
    assert_eq!(next_of(&mut rx, "focus").await, json!({"type": "focus"}));
    assert_eq!(next_of(&mut rx, "focus").await, json!({"type": "focus"}));
    // a state is emitted on a change: once the first look has listed the home
    let f2 = f.fantastic_folder("later");
    f.register("g-later", &f2);
    let state = next_of(&mut second, "state").await;
    assert_eq!(state["protocol"], json!("fantastic.home"));
    drop(second);
    until(&hk, "the new entry listed", |s| {
        s["rows"].as_array().is_some_and(|r| r.len() == 1)
    })
    .await;
    hk.shutdown().await.expect("a clean stop");
    // shut down: every receiver reads None
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while rx.recv().await.is_some() {}
    })
    .await;
    assert!(end.is_ok(), "the events end at shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn hand_over_sends_open_then_focus() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let hk = f.boot().await;
    until(&hk, "listed", |s| s["listed"] == json!(true)).await;
    let mut rx = hk.events();
    let to = running(&f.home).await.unwrap().expect("running");
    let folder = f.fantastic_folder("handed");
    hand_over(&to, vec![folder.clone()])
        .await
        .expect("handed over");
    let mut calls = Vec::new();
    let wait = async {
        while calls.len() < 2 {
            let p = rx.recv().await.expect("events");
            if matches!(p["type"].as_str(), Some("open" | "focus")) {
                calls.push(p);
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("both calls in 10 s");
    assert_eq!(calls[0], json!({"type": "open", "paths": [folder]}));
    assert_eq!(calls[1], json!({"type": "focus"}));
    // with no paths, only the focus
    hand_over(&to, Vec::new()).await.expect("focused");
    assert_eq!(next_of(&mut rx, "focus").await, json!({"type": "focus"}));
    hk.shutdown().await.expect("a clean stop");
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_and_selection_survive_a_restart() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let folder = f.fantastic_folder("notes");
    f.register("g-notes", &folder);
    let hk = f.boot().await;
    until(&hk, "the entry listed", |s| {
        s["listed"] == json!(true) && s["rows"][0]["id"] == json!("g-notes")
    })
    .await;
    let state = hk
        .call(json!({"type": "select", "id": "g-notes"}))
        .await
        .expect("select");
    assert_eq!(
        (state["pages"].clone(), state["selection"].clone()),
        (json!(["g-notes"]), json!("g-notes"))
    );
    hk.shutdown().await.expect("a clean stop");
    let rec = yaml(
        &f.workdir()
            .join(".fantastic/agents/user/agents/home/agent.yaml"),
    );
    assert_eq!(
        (rec["pages"].clone(), rec["selection"].clone()),
        (json!(["g-notes"]), json!("g-notes"))
    );

    let hk = f.boot().await;
    let state = until(&hk, "listed again", |s| s["listed"] == json!(true)).await;
    assert_eq!(
        (state["pages"].clone(), state["selection"].clone()),
        (json!(["g-notes"]), json!("g-notes"))
    );
    hk.shutdown().await.expect("a clean stop");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_home_never_lists_its_own_folder() {
    let _one = SERIAL.lock().await;
    let f = Fixture::new();
    let other = f.fantastic_folder("other");
    // W itself, before the first boot made it: made here, as canonical as the home will name it
    let w = f.workdir();
    fantastic_user_home_store::ensure_private_dir(&f.home, &w).unwrap();
    let own = std::fs::canonicalize(&w).unwrap().display().to_string();
    f.register("g-own", &own);
    f.register("g-other", &other);
    let before = files(&f.home.join("fantastics"));
    let hk = f.boot().await;
    let state = until(&hk, "listed", |s| {
        s["listed"] == json!(true) && s["rows"].as_array().is_some_and(|r| !r.is_empty())
    })
    .await;
    let ids: Vec<&str> = state["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert_eq!(ids, ["g-other"]);
    let e = hk
        .call(json!({"type": "open", "paths": [own.clone()]}))
        .await
        .unwrap_err();
    assert!(
        e.contains("is the home's own folder — it is never listed"),
        "{e}"
    );
    let e = hk
        .call(json!({"type": "register", "path": own}))
        .await
        .unwrap_err();
    assert!(
        e.contains("is the home's own folder — it is never listed"),
        "{e}"
    );
    hk.shutdown().await.expect("a clean stop");
    assert_eq!(
        files(&f.home.join("fantastics")),
        before,
        "the home never writes the registry"
    );
}
