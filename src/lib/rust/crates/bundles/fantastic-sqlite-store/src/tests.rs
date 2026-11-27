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
    fantastic_kernel::test_support::register(&k, Arc::clone(&root));
    k.set_root(root);
    fantastic_kernel::test_support::register(&k, agent(
        "alice",
        None,
        Some("kernel_state"),
        json!({"can": {"mem": {"call": ["*"]}}}),
    ));
    k
}

fn add_store(k: &Arc<Kernel>, path: &str) {
    fantastic_kernel::test_support::register(k, agent(
        "mem",
        Some(HANDLER_MODULE),
        Some("kernel_state"),
        json!({"path": path}),
    ));
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value, as_: Option<&str>) -> Value {
    let item = Item::new(AgentId::from(target), payload);
    match as_ {
        None => fantastic_kernel::test_support::send_settled(k, vec![item]).await.remove(0).result,
        Some(who) => {
            as_agent(AgentId::from(who), fantastic_kernel::test_support::send_settled(k, vec![item]))
                .await
                .remove(0)
                .result
        }
    }
}

fn rec(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

/// A `path` the declared record refuses: absent, empty, blank, absolute, climbing.
const BAD_PATHS: [Option<&str>; 8] = [
    None,
    Some(""),
    Some("  "),
    Some("/tmp/x.db"),
    Some("../x.db"),
    Some("a/../../x.db"),
    Some("@state/../x.db"),
    Some("a/.."),
];

#[tokio::test]
async fn path_is_required_relative_and_inside() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    for path in BAD_PATHS {
        // the agent is never created
        let mut p = json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": HANDLER_MODULE});
        if let Some(path) = path {
            p["path"] = json!(path);
        }
        let out = call(&k, "kernel_state", p, None).await;
        let reason = if path.is_none() { "missing_arg" } else { "bad_arg" };
        assert_eq!(out["reason"], json!(reason), "{path:?}: {out}");
        assert_eq!(out["arg"], json!("path"), "{path:?}: {out}");
    }
    for good in ["stores/mem.db", "mem.db", "./mem.db", "@state/memory.db", "@self/m.db", "a..b/c.db", ".hidden.db"] {
        let p = json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": HANDLER_MODULE, "path": good});
        let out = call(&k, "kernel_state", p, None).await;
        assert!(out.get("error").is_none() && out.get("reason").is_none(), "{good}: {out}");
    }
}

#[tokio::test]
async fn path_cannot_be_dropped_by_update() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    add_store(&k, "mem.db");
    for bad in ["", "../x", "/abs.db"] {
        let out = call(
            &k,
            "kernel_state",
            json!({"type": "update_agent", "id": "mem", "path": bad}),
            None,
        )
        .await;
        assert_eq!(out["reason"], json!("bad_arg"), "{bad}: {out}");
        assert_eq!(out["arg"], json!("path"), "{bad}: {out}");
    }
    let out = call(
        &k,
        "kernel_state",
        json!({"type": "update_agent", "id": "mem", "path": "other.db"}),
        None,
    )
    .await;
    assert!(out.get("reason").is_none(), "{out}");
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
    assert_eq!(refused["reason"], json!("bad_arg"), "{refused}");
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
        as_agent(AgentId::from("alice"), fantastic_kernel::test_support::send_settled(&k, vec![item]))
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
        fantastic_kernel::test_support::settled(&k2, vec![Item::new(
            AgentId::from("mem"),
            json!({"type": "get_bytes", "ref": "p1"}),
        )]),
    )
    .await
    .remove(0);
    assert_eq!(out.result["media_type"], json!("image/png"), "{}", out.result);
    assert_eq!(out.body_bytes(), Some(&[9u8, 0, 9][..]));
}

// ─── query (issue #696) ──────────────────────────────────────────────────────

/// A kernel over `dir` with a store `mem` allowed to query `data.db` (30 rows of `t(n, s, b)`).
fn query_kernel(dir: &Path, allow: Value) -> Arc<Kernel> {
    let c = Connection::open(dir.join("data.db")).unwrap();
    c.execute_batch("CREATE TABLE t (n INTEGER, s TEXT, b BLOB); WITH RECURSIVE x(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM x WHERE i < 30) INSERT INTO t SELECT i, 'row' || i, zeroblob(4) FROM x;").unwrap();
    drop(c);
    let k = kernel(dir);
    fantastic_kernel::test_support::register(&k, agent(
        "mem",
        Some(HANDLER_MODULE),
        Some("kernel_state"),
        json!({"path": "mem.db", "query_allow": allow}),
    ));
    k
}

async fn query(k: &Arc<Kernel>, p: Value) -> Value {
    let mut p = p;
    p["type"] = json!("query");
    call(k, "mem", p, Some("alice")).await
}

#[tokio::test]
async fn query_returns_columns_and_rows_and_blobs_as_sizes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = query_kernel(tmp.path(), json!(["data.db"]));
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT n, s, b FROM t WHERE n <= 2 ORDER BY n;"})).await;
    assert_eq!(out["columns"], json!(["n", "s", "b"]), "{out}");
    assert_eq!(out["rows"], json!([[1, "row1", {"bytes": 4}], [2, "row2", {"bytes": 4}]]), "{out}");
    assert_eq!(out["truncated"], json!(false), "{out}");
}

#[tokio::test]
async fn query_binds_params_and_never_splices_them() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = query_kernel(tmp.path(), json!(["data.db"]));
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT n FROM t WHERE n = ? AND s = ?", "params": [3, "row3"]})).await;
    assert_eq!(out["rows"], json!([[3]]), "{out}");
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT n FROM t WHERE s = ?", "params": ["x' OR '1'='1"]})).await;
    assert_eq!(out["rows"], json!([]), "{out}");
    for bad in [json!([[1]]), json!([{"a": 1}]), json!([1, 2])] {
        let out = query(&k, json!({"db": "data.db", "sql": "SELECT n FROM t WHERE n = ?", "params": bad})).await;
        assert_eq!(out["reason"], json!("bad_arg"), "{bad}: {out}");
        assert_eq!(out["arg"], json!("params"), "{bad}: {out}");
    }
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT n FROM t WHERE n = ?"})).await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
}

#[tokio::test]
async fn query_limit_defaults_caps_and_reports_truncated() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = query_kernel(tmp.path(), json!(["data.db"]));
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT n FROM t", "limit": 10})).await;
    assert_eq!(out["rows"].as_array().unwrap().len(), 10, "{out}");
    assert_eq!(out["truncated"], json!(true), "{out}");
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT n FROM t", "limit": 30})).await;
    assert_eq!(out["rows"].as_array().unwrap().len(), 30, "{out}");
    assert_eq!(out["truncated"], json!(false), "{out}");
    // default 500 and hard cap 5000, over a generated set larger than both
    let big = "WITH RECURSIVE x(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM x WHERE i < 6000) SELECT i FROM x";
    let out = query(&k, json!({"db": "data.db", "sql": big})).await;
    assert_eq!(out["rows"].as_array().unwrap().len(), 500, "{out}");
    assert_eq!(out["truncated"], json!(true), "{out}");
    let out = query(&k, json!({"db": "data.db", "sql": big, "limit": 100000})).await;
    assert_eq!(out["rows"].as_array().unwrap().len(), 5000, "{out}");
    assert_eq!(out["truncated"], json!(true), "{out}");
}

#[tokio::test]
async fn query_refuses_a_file_not_allowed_or_missing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = query_kernel(tmp.path(), json!(["data.db", "gone.db"]));
    std::fs::copy(tmp.path().join("data.db"), tmp.path().join("other.db")).unwrap();
    for db in ["other.db", "./data.db", "mem.db", "/etc/passwd"] {
        let out = query(&k, json!({"db": db, "sql": "SELECT 1"})).await;
        assert_eq!(out["reason"], json!("not_allowed"), "{db}: {out}");
    }
    let out = query(&k, json!({"db": "gone.db", "sql": "SELECT 1"})).await;
    assert_eq!(out["reason"], json!("not_found"), "{out}");
    // no allowlist at all: none
    let tmp2 = tempfile::TempDir::new().unwrap();
    let k2 = kernel(tmp2.path());
    add_store(&k2, "mem.db");
    let out = query(&k2, json!({"db": "mem.db", "sql": "SELECT 1"})).await;
    assert_eq!(out["reason"], json!("not_allowed"), "{out}");
}

#[tokio::test]
async fn query_reaches_the_stores_own_tables_only_when_its_file_is_listed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = query_kernel(tmp.path(), json!(["data.db", "mem.db"]));
    let out = call(&k, "mem", json!({"type": "boot"}), None).await;
    assert!(out.get("reason").is_none(), "{out}");
    let out = query(&k, json!({"db": "mem.db", "sql": "SELECT COUNT(*) FROM rows"})).await;
    assert_eq!(out["rows"], json!([[0]]), "{out}");
}

#[tokio::test]
async fn query_refuses_two_statements_and_anything_that_writes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = query_kernel(tmp.path(), json!(["data.db"]));
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT 1; SELECT 2"})).await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
    assert_eq!(out["arg"], json!("sql"), "{out}");
    for sql in [
        "DELETE FROM t",
        "INSERT INTO t VALUES (1, 'x', NULL)",
        "UPDATE t SET n = 0",
        "DROP TABLE t",
        "CREATE TABLE u (x)",
        "PRAGMA journal_mode = DELETE",
        "ATTACH DATABASE ':memory:' AS m",
        "VACUUM INTO 'copy.db'",
        "WITH x AS (SELECT 1) DELETE FROM t",
    ] {
        let out = query(&k, json!({"db": "data.db", "sql": sql})).await;
        assert_eq!(out["reason"], json!("bad_arg"), "{sql}: {out}");
        assert_eq!(out["arg"], json!("sql"), "{sql}: {out}");
    }
    let out = query(&k, json!({"db": "data.db", "sql": "SELECT COUNT(*) FROM t"})).await;
    assert_eq!(out["rows"], json!([[30]]), "nothing was written: {out}");
    assert!(!tmp.path().join("copy.db").exists());
}

#[tokio::test]
async fn query_allow_refuses_absolute_and_climbing_paths_and_empty_entries() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    for bad in ["/tmp/x.db", "../x.db", "a/../../x.db", "@state/../x.db", ""] {
        let p = json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": HANDLER_MODULE, "path": "m.db", "query_allow": [bad]});
        let out = call(&k, "kernel_state", p, None).await;
        assert_eq!(out["reason"], json!("bad_arg"), "{bad}: {out}");
        assert!(out["arg"].as_str().unwrap().starts_with("query_allow"), "{bad}: {out}");
    }
    let p = json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": HANDLER_MODULE, "path": "m.db", "query_allow": ["a/data.db", "@state/x.db", "@self/y.db"]});
    let out = call(&k, "kernel_state", p, None).await;
    assert!(out.get("reason").is_none(), "{out}");
}

#[tokio::test]
async fn reflect_lists_the_allowlist_as_file_and_size_rows_with_a_read_only_view() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = query_kernel(tmp.path(), json!(["data.db", "gone.db"]));
    let out = call(&k, "mem", json!({"type": "reflect"}), None).await;
    let rows = out["query_allow"].as_array().unwrap();
    assert_eq!(rows[0]["file"], json!("data.db"), "{out}");
    assert!(rows[0]["size"].as_u64().unwrap() > 0, "{out}");
    assert_eq!(rows[1], json!({"file": "gone.db", "size": null}), "{out}");
    assert_eq!(out["view"]["kind"], json!("html"), "{out}");
    assert!(out["view"]["bytes"].as_str().unwrap().contains("\"mem\""), "{out}");
    assert_eq!(out["view"]["calls"], json!({"mem": ["reflect", "watch"]}), "{out}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_query_allow_entry_linked_out_of_the_folder_is_refused_without_a_host_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    Connection::open(outside.join("out.db")).unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::os::unix::fs::symlink(&outside, work.join("ext")).unwrap();
    let k = query_kernel(&work, json!(["ext/out.db"]));
    let out = query(&k, json!({"db": "ext/out.db", "sql": "SELECT 1;"})).await;
    assert!(out.get("reason").is_some(), "refused: {out}");
    let text = out.to_string();
    assert!(text.contains("query_allow"), "names the field: {out}");
    assert!(!text.contains(&tmp.path().display().to_string()), "no host path: {out}");
}

/// THE STORE'S WRITES ARE PUBLISHED on its own inbox, as `appended` is: a prune that
/// removed rows (`pruned {n}`), a whole-store inject (`injected {rows}`) and stored bytes
/// (`bytes_put {ref}`) — what its view (the file sizes) re-reads on. A prune that removed
/// nothing and a refused inject publish nothing.
#[tokio::test]
async fn prune_inject_and_put_bytes_are_published_on_the_stores_inbox() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = kernel(tmp.path());
    add_store(&k, "mem.db");
    let mut rx = k.ensure_inbox(&AgentId::from("mem"));
    let out = call(&k, "mem", json!({"type": "append", "rows": [{"content": "a"}, {"content": "b"}]}), Some("alice")).await;
    let first = out["ids"][0].clone();
    call(&k, "mem", json!({"type": "prune", "ids": [first]}), Some("alice")).await;
    call(&k, "mem", json!({"type": "prune", "ids": [424242]}), Some("alice")).await;
    let dup = json!({"rows": [{"owner": "alice", "row": {"id": 1}}, {"owner": "alice", "row": {"id": 1}}]});
    call(&k, "mem", json!({"type": "inject", "tables": dup}), Some("alice")).await;
    let one = json!({"rows": [{"owner": "alice", "row": {"id": 1}}]});
    call(&k, "mem", json!({"type": "inject", "tables": one}), Some("alice")).await;
    let item = Item::new(AgentId::from("mem"), json!({"type": "put_bytes", "ref": "p1", "media_type": "image/png"}))
        .with_binary_key("data", vec![9, 0, 9]);
    as_agent(AgentId::from("alice"), fantastic_kernel::test_support::send_settled(&k, vec![item])).await;
    let mut got = Vec::new();
    while let Some(item) = rx.try_recv() {
        if matches!(item.payload["type"].as_str(), Some("pruned" | "injected" | "bytes_put")) {
            got.push(item.payload);
        }
    }
    assert_eq!(
        got,
        vec![
            json!({"type": "pruned", "n": 1}),
            json!({"type": "injected", "rows": 1}),
            json!({"type": "bytes_put", "ref": "p1"}),
        ]
    );
}
