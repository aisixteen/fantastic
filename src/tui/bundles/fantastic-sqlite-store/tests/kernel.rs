//! `sqlite_store` as the KERNEL'S store: the root's `memory_store_id` names it,
//! `kernel_start` boots it first, the flusher lands the execution rows in its
//! file — and a NEW PROCESS over the same dir gets the ledger, named dedup, the
//! dead letters, an AI agent's head (folds included) and the grants back with no
//! `dump`/`inject`. Also: the prune cascade seen on disk, and many agents writing
//! alongside the flusher.
//!
//! The restart test re-executes THIS test binary (`restart_child`), once to write
//! and once to read, so the second kernel shares nothing with the first but the
//! directory.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_ai_core::agent_loop::BackendConfig;
use fantastic_ai_core::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::test_support::{register_fake_store, register_root_loader, wire_fake_store};
use fantastic_kernel::{with_sender, AgentId, Item, Kernel, Outcome};
use serde_json::{json, Map, Value};

// ── a probe: counts per process, fails on purpose ─────────────────────────────

#[derive(Default)]
struct Probe(Mutex<HashMap<String, i64>>);

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "sql_probe.tools"
    }
    async fn handle(&self, id: &AgentId, payload: &Value, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        match payload["type"].as_str().unwrap_or("") {
            "boot" => Ok(None),
            "reflect" => Ok(Some(json!({"id": id.0, "sentence": "probe", "verbs": {"work": "", "count": ""}}))),
            "work" => Ok(Some(json!({"ok": true}))),
            // `pid`: which PROCESS ran it — a dedup that re-ran would answer n=1
            // from the new process too, so the count alone proves nothing.
            "count" => {
                let mut c = self.0.lock().unwrap();
                let n = c.entry(id.0.clone()).or_insert(0);
                *n += 1;
                Ok(Some(json!({"n": *n, "pid": std::process::id()})))
            }
            "fail" => Err("probe failed on purpose".into()),
            other => Ok(Some(json!({"error": format!("unknown '{other}'"), "reason": "unhandled"}))),
        }
    }
}

// ── a scripted AI agent: the real ai_core verb table, a canned provider ─────────

struct Canned;

#[async_trait]
impl Provider for Canned {
    async fn chat(&self, _m: &[Value], _max: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let ev = Ok(ProviderEvent::Token("noted <call name=\"done\"></call>".into()));
        Ok(Box::pin(futures_util::stream::iter(vec![ev])))
    }
    fn model(&self) -> String {
        "canned".into()
    }
}

const CFG: BackendConfig = BackendConfig { name: "canned_ai", provider_missing_error: None };

struct CannedAi;

#[async_trait]
impl Bundle for CannedAi {
    fn name(&self) -> &str {
        "canned_ai.tools"
    }
    async fn handle(&self, id: &AgentId, payload: &Value, k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        if payload["type"] == json!("reflect") {
            return Ok(Some(json!({"id": id.0, "sentence": "canned AI", "verbs": {"send": ""}})));
        }
        let out = fantastic_ai_core::verbs::handle_common(id, payload, k, CFG, |_| {
            Some(Arc::new(Canned) as Arc<dyn Provider>)
        })
        .await?;
        Ok(Some(out.unwrap_or_else(|| json!({"reason": "unhandled"}))))
    }
}

// ── wiring ──────────────────────────────────────────────────────────────

/// The records persist through the kernel's own test provider, rooted at the
/// loader's dir — registered on EVERY boot.
fn registry(dir: &Path) -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    register_fake_store(&mut reg, &dir.join(".fantastic"));
    reg.register(fantastic_sqlite_store::HANDLER_MODULE, fantastic_sqlite_store::SqliteStoreBundle::default());
    reg.register("sql_probe.tools", Probe::default());
    reg.register("canned_ai.tools", CannedAi);
    reg
}

fn boot(dir: &Path) -> Arc<Kernel> {
    bootstrap(registry(dir), BootstrapOptions::one_shot(dir)).expect("bootstrap").kernel
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)]).await.remove(0)
}

async fn call_as(k: &Arc<Kernel>, who: &str, target: &str, payload: Value) -> Value {
    with_sender(AgentId::from(who), k.send(vec![Item::new(AgentId::from(target), payload)]))
        .await
        .remove(0)
        .result
}

async fn call_named(k: &Arc<Kernel>, target: &str, payload: Value, name: &str) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload).with_id(name)]).await.remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().unwrap().id.0.clone()
}

fn guid(k: &Arc<Kernel>) -> String {
    k.root().unwrap().meta.read().unwrap()["guid"].as_str().unwrap().to_string()
}

async fn create(k: &Arc<Kernel>, fields: Value) {
    let mut p = json!({"type": "create_agent", "parent_id": root_id(k)});
    for (key, v) in fields.as_object().unwrap() {
        p[key] = v.clone();
    }
    let r = call(k, "kernel", p).await.result;
    assert_eq!(r["id"], fields["id"], "{r}");
}

/// The store, the root naming it, `probes` probe agents, and the AI `ai` wired to
/// the store both ways — then `kernel_start`. The store file is `mem.db` in the
/// kernel's workdir.
async fn wire(k: &Arc<Kernel>, dir: &Path, probes: &[&str]) {
    fantastic_kernel::test_support::boot_root(k).await; // mints the guid
    wire_fake_store(k, &dir.join(".fantastic")).await;
    let mut readers = Map::new();
    readers.insert("ai".into(), json!("*"));
    for p in probes {
        readers.insert((*p).into(), json!("*"));
    }
    create(k, json!({
        "id": "memory", "handler_module": fantastic_sqlite_store::HANDLER_MODULE,
        "path": "mem.db", "operator": true, "admins": ["kernel"], "callable_by": readers,
    }))
    .await;
    let r = call(k, "kernel", json!({"type": "update_agent", "id": root_id(k), "memory_store_id": "memory"})).await;
    assert!(r.result.get("error").is_none(), "{}", r.result);
    for p in probes {
        create(k, json!({
            "id": p, "handler_module": "sql_probe.tools",
            "callable_by": {"rest": "*", "ai": "*", "$parent": "*"},
            "can_call": {"memory": "*"},
        }))
        .await;
    }
    create(k, json!({
        "id": "ai", "handler_module": "canned_ai.tools", "model": "canned",
        "context_window": 32768, "memory_store_id": "memory",
        "can_call": {"memory": "*", "cli": "*"},
    }))
    .await;
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    assert_eq!(out["booted"][1], json!("memory"), "{out}");
}

fn pid(o: &Outcome) -> i64 {
    o.pid.unwrap().get()
}

fn open_db(dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join("mem.db")).unwrap()
}

fn count(db: &rusqlite::Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0)).unwrap()
}

// ── restart: a new process, the same dir, nothing injected ───────────────────

fn child(phase: &str, dir: &Path) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "restart_child", "--nocapture", "--test-threads=1"])
        .env("SQLITE_STORE_PHASE", phase)
        .env("SQLITE_STORE_DIR", dir)
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "phase {phase} failed:\n{text}");
    assert!(text.contains(&format!("PHASE {phase} OK")), "phase {phase} never ran:\n{text}");
}

#[test]
fn a_new_process_gets_everything_back_without_inject() {
    let tmp = tempfile::TempDir::new().unwrap();
    child("write", tmp.path());
    child("read", tmp.path());
}

/// Not a test of its own: the body of the two phases above. Inert unless the
/// parent set the phase.
#[tokio::test]
async fn restart_child() {
    let Ok(phase) = std::env::var("SQLITE_STORE_PHASE") else { return };
    let dir = std::path::PathBuf::from(std::env::var("SQLITE_STORE_DIR").unwrap());
    let facts = dir.join("facts.json");
    if phase == "write" {
        let k = boot(&dir);
        wire(&k, &dir, &["probe"]).await;
        let named = call_named(&k, "probe", json!({"type": "count"}), "nightly").await;
        assert_eq!(named.result["n"], json!(1));
        let dead = call(&k, "probe", json!({"type": "fail"})).await;
        assert_eq!(dead.status.as_str(), "failed");
        // an AI turn, then a clear-fold over it
        let turn = call(&k, "ai", json!({"type": "send", "text": "remember the lighthouse"})).await;
        assert!(turn.result.get("error").is_none(), "{}", turn.result);
        let cleared = call(&k, "ai", json!({"type": "context_clear", "why": "new thread"})).await.result;
        let fold = cleared["cleared"].as_i64().unwrap_or_else(|| panic!("no clear-fold: {cleared}"));
        let head = call(&k, "ai", json!({"type": "history"})).await.result;
        // the AI shares its thread with the probe
        let g = call_as(&k, "ai", "memory", json!({"type": "share", "with": "probe", "scope": {"fold": fold}})).await;
        let grant = g["grant"].as_i64().unwrap_or_else(|| panic!("share refused: {g}"));
        k.shutdown().await;
        let f = json!({
            "guid": guid(&k), "named": pid(&named), "dead": pid(&dead), "fold": fold,
            "grant": grant, "head": head["rows"], "pid": std::process::id(), "next_id": k.rows.next_id(),
        });
        std::fs::write(&facts, serde_json::to_vec(&f).unwrap()).unwrap();
    } else {
        let f: Value = serde_json::from_slice(&std::fs::read(&facts).unwrap()).unwrap();
        let k = boot(&dir);
        assert_eq!(json!(guid(&k)), f["guid"], "the same kernel identity");
        let out = k.start(false).await;
        assert_eq!(out["started"], json!(true), "{out}");
        assert!(k.rows.next_id() >= f["next_id"].as_i64().unwrap(), "ids never repeat across a restart");
        // the ledger: read back from the file
        let led = call(&k, "kernel", json!({"type": "ledger"})).await.result;
        let by: HashMap<i64, Value> = led["executions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["id"].as_i64().unwrap(), e.clone()))
            .collect();
        let named = f["named"].as_i64().unwrap();
        let dead = f["dead"].as_i64().unwrap();
        assert_eq!(by[&named]["status"], json!("completed"), "{led}");
        assert_eq!(by[&dead]["status"], json!("failed"), "the dead letter is back");
        assert!(by[&dead].get("acked").is_none());
        // named dedup: the earlier process's answer, the step not run again
        let again = call_named(&k, "probe", json!({"type": "count"}), "nightly").await;
        assert_eq!(again.result["n"], json!(1));
        assert_eq!(again.result["pid"], f["pid"], "answered from the store, not re-run here");
        // the AI's head, read from the store by a fresh ai_core
        let head = call(&k, "ai", json!({"type": "history"})).await.result;
        assert_eq!(head["rows"], f["head"], "{head}");
        let members = call(&k, "ai", json!({"type": "history", "fold": f["fold"]})).await.result;
        let text = serde_json::to_string(&members).unwrap();
        assert!(text.contains("remember the lighthouse"), "the fold's members are back: {members}");
        // the grant: the probe still reads under it
        let rec = call_as(&k, "probe", "memory", json!({"type": "grants", "side": "received"})).await;
        assert_eq!(rec["grants"][0]["grant"], f["grant"], "{rec}");
        let read = call_as(&k, "probe", "memory", json!({"type": "read", "grant": f["grant"]})).await;
        assert!(serde_json::to_string(&read).unwrap().contains("remember the lighthouse"), "{read}");
        // the dead letter can be acked now, in the new process
        let acked = call(&k, "kernel", json!({"type": "ack_deadletter", "id": dead})).await.result;
        assert_eq!(acked, json!({"acked": true, "id": dead}));
        k.shutdown().await;
    }
    println!("PHASE {phase} OK");
}

// ── the cascade, on disk ─────────────────────────────────────────────

#[tokio::test]
async fn pruning_the_anchor_drops_the_grant_in_the_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let k = in_dir(&dir, &["alice", "bob"]).await;
    let row = k.mint_id();
    let r = call_as(&k, "alice", "memory", json!({"type": "append", "rows": [{"id": row, "content": "x"}]})).await;
    assert!(r.get("reason").is_none(), "{r}");
    let g = call_as(&k, "alice", "memory", json!({"type": "share", "with": "bob", "scope": {"row": row}})).await;
    assert!(g["grant"].is_i64(), "{g}");
    let db = open_db(&dir);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM grants"), 1);
    let pr = call_as(&k, "alice", "memory", json!({"type": "prune", "ids": [row]})).await;
    assert!(pr.get("reason").is_none(), "{pr}");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM grants"), 0, "the FK cascade took the grant");
    let rec = call_as(&k, "bob", "memory", json!({"type": "read", "grant": g["grant"]})).await;
    assert_eq!(rec["reason"], json!("not_found"), "{rec}");
    k.shutdown().await;
}

// ── many writers and the flusher on one file ───────────────────────────

#[tokio::test]
async fn many_agents_and_the_flusher_share_one_file() {
    const AGENTS: usize = 12;
    const ROWS: usize = 40;
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let names: Vec<String> = (0..AGENTS).map(|i| format!("w{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let k = in_dir(&dir, &refs).await;
    let mut tasks = Vec::new();
    for name in names.clone() {
        let k = Arc::clone(&k);
        tasks.push(tokio::spawn(async move {
            for i in 0..ROWS {
                let id = k.mint_id();
                let r = call_as(&k, &name, "memory", json!({"type": "append", "rows": [{"id": id, "content": format!("{name} row {i}")}]})).await;
                assert!(r.get("reason").is_none(), "{r}");
                // kernel traffic for the flusher to land meanwhile
                let _ = call(&k, &name, json!({"type": "work"})).await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    k.flush_executions().await;
    for name in &names {
        let r = call_as(&k, name, "memory", json!({"type": "read", "limit": 1000})).await;
        assert_eq!(r["rows"].as_array().map(Vec::len), Some(ROWS), "{name}: {r}");
        let hit = call_as(&k, name, "memory", json!({"type": "search", "query": format!("{name} row 7")})).await;
        assert!(!hit["matches"].as_array().unwrap().is_empty(), "{name}: FTS finds its own row: {hit}");
    }
    let db = open_db(&dir);
    let kernel_owner = format!("{}|kernel", guid(&k));
    let settles: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM rows WHERE owner = ?1 AND json_extract(doc, '$.kind') = 'settle'",
            [&kernel_owner],
            |r| r.get(0),
        )
        .unwrap();
    assert!(settles >= (AGENTS * ROWS) as i64, "every work call settled into the file: {settles}");
    let ok: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(ok, "ok");
    k.shutdown().await;
}

/// A kernel over `dir`, wired; its store file is `dir/mem.db`.
async fn in_dir(dir: &Path, probes: &[&str]) -> Arc<Kernel> {
    let k = boot(dir);
    wire(&k, dir, probes).await;
    k
}

// ── the file's life: a stop keeps it, `delete_agent` removes it ─────────────

#[tokio::test]
async fn a_stop_keeps_the_file_and_delete_agent_removes_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let k = in_dir(&dir, &["alice"]).await;
    // a second store, so the kernel keeps its own while this one is deleted
    create(&k, json!({
        "id": "notes", "handler_module": fantastic_sqlite_store::HANDLER_MODULE,
        "path": "notes.db", "callable_by": {"alice": "*"},
    }))
    .await;
    let r = call(&k, "kernel", json!({"type": "update_agent", "id": "alice", "can_call": {"memory": "*", "notes": "*"}})).await;
    assert!(r.result.get("error").is_none(), "{}", r.result);
    let row = k.mint_id();
    let r = call_as(&k, "alice", "notes", json!({"type": "append", "rows": [{"id": row, "content": "keep me"}]})).await;
    assert!(r.get("reason").is_none(), "{r}");
    for _ in 0..5 {
        call(&k, "alice", json!({"type": "work"})).await;
    }
    k.flush_executions().await;
    let before = count(&open_db(&dir), "SELECT COUNT(*) FROM rows");
    k.shutdown().await; // kernel_stop: the teardown walk runs over both stores
    assert!(dir.join("notes.db").is_file() && dir.join("mem.db").is_file(), "a stop deletes nothing");
    let after = count(&open_db(&dir), "SELECT COUNT(*) FROM rows");
    assert!(after >= before, "every row is still in the file: {before} → {after}");
    let notes = rusqlite::Connection::open(dir.join("notes.db")).unwrap();
    assert_eq!(count(&notes, "SELECT COUNT(*) FROM rows"), 1);
    drop(notes);

    // a new process's kernel over the same dir: an explicit delete removes the file
    let k = boot(&dir);
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    let r = call(&k, "kernel", json!({"type": "delete_agent", "id": "notes"})).await;
    assert!(r.result.get("error").is_none(), "{}", r.result);
    for f in ["notes.db", "notes.db-wal", "notes.db-shm"] {
        assert!(!dir.join(f).exists(), "{f} survived delete_agent");
    }
    assert!(dir.join("mem.db").is_file(), "only the deleted store's file goes");
    k.shutdown().await;
}
