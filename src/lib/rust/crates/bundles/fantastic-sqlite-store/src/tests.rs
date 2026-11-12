//! `sqlite_store`'s own unit tests: the record's `path` rules, the file's pragmas,
//! the one row ram_store accepts that a file cannot hold. The verbs
//! themselves are ram_store's suite, run against both stores in
//! `tests/conformance.rs` and `tests/contract.rs`.

use super::*;
use fantastic_kernel::agent::Agent;
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::Item;

fn agent(id: &str, hm: Option<&str>, parent: Option<&str>, meta: Value) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        hm.map(str::to_string),
        parent.map(AgentId::from),
        meta.as_object().cloned().unwrap_or_default(),
        PathBuf::new(),
        false,
    )
}

/// A disk-backed kernel over `dir` with the root and `alice`; no store yet.
fn kernel(dir: &Path) -> Arc<Kernel> {
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: dir.to_path_buf().join(".fantastic"),
        work_dir: dir.to_path_buf(),
    });
    k.bundles.register(HANDLER_MODULE, SqliteStoreBundle);
    let k = Arc::new(k);
    let root = agent("kernel_state", None, None, json!({"guid": "g1"}));
    k.register(Arc::clone(&root));
    k.set_root(root);
    k.register(agent(
        "alice",
        None,
        Some("kernel_state"),
        json!({"can_call": {"mem": "*"}}),
    ));
    k
}

fn add_store(k: &Arc<Kernel>, path: &str) {
    k.register(agent(
        "mem",
        Some(HANDLER_MODULE),
        Some("kernel_state"),
        json!({"path": path, "callable_by": {"alice": "*"}}),
    ));
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value, as_: Option<&str>) -> Value {
    let item = Item::new(AgentId::from(target), payload);
    match as_ {
        None => k.send(vec![item]).await.remove(0).result,
        Some(who) => {
            as_agent(AgentId::from(who), k.send(vec![item]))
                .await
                .remove(0)
                .result
        }
    }
}

fn rec(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

const BAD_PATHS: [(Option<&str>, &str); 6] = [
    (None, "needs `path`"),
    (Some(""), "needs `path`"),
    (Some("  "), "needs `path`"),
    (Some("/tmp/x.db"), "must stay inside its folder"),
    (Some("../x.db"), "must stay inside its folder"),
    (Some("a/../../x.db"), "must stay inside its folder"),
];

#[tokio::test]
async fn path_is_required_relative_and_inside() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    for (path, says) in BAD_PATHS {
        let meta = path.map_or_else(Map::new, |p| rec(json!({"path": p})));
        let err = check_path(&meta).expect_err("refused");
        assert!(err.contains(says), "{path:?}: {err}");
        // …and through the kernel: the agent is never created
        let mut p = json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": HANDLER_MODULE});
        if let Some(path) = path {
            p["path"] = json!(path);
        }
        let out = call(&k, "kernel_state", p, None).await;
        match path {
            // the declared record refuses a missing or empty `path` by name
            None => {
                assert_eq!(out["reason"], json!("missing_arg"), "{path:?}: {out}");
                assert_eq!(out["arg"], json!("path"), "{path:?}: {out}");
            }
            Some("") => {
                assert_eq!(out["reason"], json!("bad_arg"), "{path:?}: {out}");
                assert_eq!(out["arg"], json!("path"), "{path:?}: {out}");
            }
            // what a type cannot say is the bundle's own refusal
            Some(_) => {
                assert!(
                    SqliteStoreBundle
                        .validate_meta(&meta, &Map::new())
                        .is_some_and(|e| e.contains(says)),
                    "{path:?}"
                );
                assert_eq!(out["reason"], json!("invalid_meta"), "{path:?}: {out}");
                assert!(
                    out["error"].as_str().unwrap().contains(says),
                    "{path:?}: {out}"
                );
            }
        }
    }
    assert!(check_path(&rec(json!({"path": "stores/mem.db"}))).is_ok());
}

#[tokio::test]
async fn path_cannot_be_dropped_by_update() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    add_store(&k, "mem.db");
    let good = rec(json!({"path": "mem.db"}));
    assert!(SqliteStoreBundle
        .validate_meta(&rec(json!({"path": ""})), &good)
        .is_some());
    assert!(SqliteStoreBundle
        .validate_meta(&rec(json!({"path": "../x"})), &good)
        .is_some());
    assert!(SqliteStoreBundle
        .validate_meta(&rec(json!({"path": "other.db"})), &good)
        .is_none());
    assert!(SqliteStoreBundle
        .validate_meta(&rec(json!({"callable_by": {}})), &good)
        .is_none());
    let out = call(
        &k,
        "kernel_state",
        json!({"type": "update_agent", "id": "mem", "path": ""}),
        None,
    )
    .await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
    assert_eq!(out["arg"], json!("path"), "{out}");
    let out = call(
        &k,
        "kernel_state",
        json!({"type": "update_agent", "id": "mem", "path": "../x"}),
        None,
    )
    .await;
    assert_eq!(out["reason"], json!("invalid_meta"), "{out}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_dir_is_refused_at_boot() {
    let tmp = tempfile::TempDir::new().unwrap();
    let work = tmp.path().join("work");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, work.join("stores")).unwrap();
    let k = kernel(&work);
    add_store(&k, "stores/mem.db");
    let out = call(&k, "mem", json!({"type": "boot"}), None).await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
    assert_eq!(out["arg"], json!("path"), "{out}");
    assert!(
        !outside.join("mem.db").exists(),
        "nothing was created outside the dir"
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

#[tokio::test]
async fn boot_opens_the_file_with_the_pythons_pragmas() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    add_store(&k, "stores/mem.db");
    assert_eq!(
        call(&k, "mem", json!({"type": "boot"}), None).await,
        Value::Null
    );
    assert!(tmp.path().join("stores/mem.db").is_file());
    let c = conns(&k)
        .0
        .lock()
        .unwrap()
        .get("mem")
        .cloned()
        .expect("the open connection");
    let c = c.lock().unwrap();
    let pragma = |name: &str| -> rusqlite::types::Value {
        c.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        pragma("journal_mode"),
        rusqlite::types::Value::Text("wal".into())
    );
    assert_eq!(pragma("synchronous"), rusqlite::types::Value::Integer(1));
    assert_eq!(
        pragma("busy_timeout"),
        rusqlite::types::Value::Integer(5000)
    );
    assert_eq!(pragma("foreign_keys"), rusqlite::types::Value::Integer(1));
    assert_eq!(pragma("secure_delete"), rusqlite::types::Value::Integer(1));
}

/// The one number the kernel keeps whose text is not JSON (`Infinity`) cannot be
/// typed into the declared `rows`: the call is refused whole, and the store (its
/// lock) still answers.
#[tokio::test]
async fn a_row_that_is_not_json_is_refused_whole() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    add_store(&k, "mem.db");
    let inf = fantastic_kernel::test_support::non_finite("Infinity");
    assert_eq!(inf.to_string(), "Infinity", "the token under test");
    let mut bad = Map::new();
    bad.insert("id".to_string(), Value::from(2));
    bad.insert("job_id".to_string(), inf);
    let mut payload = Map::new();
    payload.insert("type".to_string(), Value::from("append"));
    payload.insert(
        "rows".to_string(),
        Value::Array(vec![json!({"id": 1}), Value::Object(bad)]),
    );
    let out = call(&k, "mem", Value::Object(payload), Some("alice")).await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
    let rows = call(&k, "mem", json!({"type": "read"}), Some("alice")).await;
    assert_eq!(rows["rows"], json!([]), "all or none");
    let ok = call(
        &k,
        "mem",
        json!({"type": "append", "rows": [{"id": 3, "content": "after"}]}),
        Some("alice"),
    )
    .await;
    assert_eq!(ok["appended"], json!(1), "the store still answers: {ok}");
}

#[test]
fn the_catalog_name_is_the_kind_not_the_handler_module() {
    assert_eq!(SqliteStoreBundle.name(), "sqlite_store");
    assert_eq!(HANDLER_MODULE, "sqlite_store.tools");
}

#[tokio::test]
async fn sqlite_store_refusals_are_kernel_shape() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    add_store(&k, "mem.db");
    let dup = json!({"rows": [
        {"owner": "alice", "row": {"id": 1}},
        {"owner": "alice", "row": {"id": 1}},
    ]});
    let out = call(
        &k,
        "mem",
        json!({"type": "inject", "tables": dup}),
        Some("alice"),
    )
    .await;
    assert_eq!(out["reason"], json!("exists"), "{out}");
    let sentence = out["error"].as_str().unwrap();
    assert!(
        sentence.contains("duplicate row") && sentence != "exists",
        "{out}"
    );
}

#[test]
fn path_at_state_lands_in_the_state_folder_and_a_plain_path_in_work() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    let state = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&state).unwrap();
    let real = |p: &Path| std::fs::canonicalize(p).unwrap();
    let own = Path::new("");
    let at_state = resolve_path(&rec(json!({"path": "@state/memory.db"})), &k, own).unwrap();
    assert_eq!(at_state, real(&state).join("memory.db"));
    let plain = resolve_path(&rec(json!({"path": "plain.db"})), &k, own).unwrap();
    assert_eq!(plain, real(tmp.path()).join("plain.db"));
    let err = resolve_path(&rec(json!({"path": "@state/../x.db"})), &k, own).unwrap_err();
    assert!(err.contains("inside its folder"), "{err}");
}

#[tokio::test]
async fn append_emits_one_appended_event_per_new_row() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    add_store(&k, "mem.db");
    let state: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let tap = state.clone();
    k.add_state_subscriber(std::sync::Arc::new(move |ev: &Value, _body| {
        tap.lock().unwrap().push(ev.clone());
    }));
    let mut rx = k.ensure_inbox(&AgentId::from("mem"));
    let out = call(
        &k,
        "mem",
        json!({"type": "append", "rows": [{"content": "a"}, {"content": "b"}]}),
        Some("alice"),
    )
    .await;
    let ids = out["ids"].as_array().unwrap().clone();
    assert_eq!(ids.len(), 2, "{out}");
    // the inbox also holds the append request itself; keep the events
    let mut events = Vec::new();
    while let Some(item) = rx.try_recv() {
        if item.payload["type"] == json!("appended") {
            events.push(item.payload);
        }
    }
    let want: Vec<Value> = ids
        .iter()
        .map(|id| json!({"type": "appended", "table": "rows", "id": id}))
        .collect();
    assert_eq!(events, want);
    // and each one went out as an emit on the state stream too
    let on_state: Vec<Value> = state
        .lock()
        .unwrap()
        .iter()
        .filter(|ev| ev["kind"] == json!("emit") && ev["payload"]["type"] == json!("appended"))
        .map(|ev| ev["payload"].clone())
        .collect();
    assert_eq!(on_state, want);
    // a refused append emits nothing
    let refused = call(
        &k,
        "mem",
        json!({"type": "append", "rows": []}),
        Some("alice"),
    )
    .await;
    assert_eq!(refused["reason"], json!("missing_arg"), "{refused}");
    while let Some(item) = rx.try_recv() {
        assert_ne!(item.payload["type"], json!("appended"), "{refused}");
    }
}

#[tokio::test]
async fn stored_bytes_survive_a_reopen_of_the_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let put = |k: Arc<Kernel>| async move {
        let item = Item::new(
            AgentId::from("mem"),
            json!({"type": "put_bytes", "ref": "p1", "media_type": "image/png"}),
        )
        .with_binary_key("data", vec![9, 0, 9]);
        as_agent(AgentId::from("alice"), k.send(vec![item]))
            .await
            .remove(0)
            .result
    };
    let k = kernel(tmp.path());
    add_store(&k, "mem.db");
    assert_eq!(put(Arc::clone(&k)).await["bytes"], json!(3));
    close_all(&k);
    let k2 = kernel(tmp.path());
    add_store(&k2, "mem.db");
    let out = as_agent(
        AgentId::from("alice"),
        k2.send(vec![Item::new(
            AgentId::from("mem"),
            json!({"type": "get_bytes", "ref": "p1"}),
        )]),
    )
    .await
    .remove(0);
    assert_eq!(out.result["media_type"], json!("image/png"), "{}", out.result);
    assert_eq!(out.body_bytes(), Some(&[9u8, 0, 9][..]));
}
