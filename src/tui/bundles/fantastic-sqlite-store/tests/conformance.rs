//! CONFORMANCE: `sqlite_store` answers exactly as `ram_store` does.
//!
//! Every scenario of ram_store's own suite (`fantastic-ram-store/src/tests.rs`,
//! the twin of python `test_ram_store.py`) runs against BOTH bundles; every reply
//! is recorded, and the two transcripts must be IDENTICAL. ram_store's own
//! assertions run against both as well.

use std::sync::{Arc, Once};

use fantastic_kernel::agent::Agent;
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};

const RAM: &str = fantastic_ram_store::HANDLER_MODULE;
const SQL: &str = fantastic_sqlite_store::HANDLER_MODULE;

/// `path` is relative to the kernel's workdir: one temp dir for the binary.
fn work() -> std::path::PathBuf {
    static ONCE: Once = Once::new();
    let d = std::env::temp_dir().join(format!("fc-sqlite-conf-{}", std::process::id()));
    ONCE.call_once(|| {
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
    });
    d
}

fn agent(id: &str, hm: Option<&str>, parent: Option<&str>, meta: Value) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        hm.map(str::to_string),
        parent.map(AgentId::from),
        meta.as_object().cloned().unwrap_or_default(),
        std::path::PathBuf::new(),
        false,
    )
}

/// ram_store's `store()` fixture, for either bundle.
fn store(handler: &str, tag: &str) -> Arc<Kernel> {
    let dir = work();
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::disk(dir.clone()));
    k.bundles.register(RAM, fantastic_ram_store::RamStoreBundle::default());
    k.bundles.register(SQL, fantastic_sqlite_store::SqliteStoreBundle::default());
    let k = Arc::new(k);
    let root = agent("kernel_state", None, None, json!({"guid": "g1"}));
    k.register(Arc::clone(&root));
    k.set_root(root);
    for a in ["alice", "bob", "carol"] {
        k.register(agent(a, None, Some("kernel_state"), json!({"can_call": {"mem": "*"}})));
    }
    let mut meta = json!({"callable_by": {"alice": "*", "bob": "*", "carol": "*"}});
    if handler == SQL {
        let path = format!("stores/{tag}.db");
        let _ = std::fs::remove_file(dir.join(&path));
        meta["path"] = json!(path);
    }
    k.register(agent("mem", Some(handler), Some("kernel_state"), meta));
    k
}

struct T {
    k: Arc<Kernel>,
    log: Vec<Value>,
}

impl T {
    async fn call(&mut self, payload: Value, as_: Option<&str>) -> Value {
        let item = Item::new(AgentId::from("mem"), payload);
        let out = match as_ {
            None => self.k.send(vec![item]).await.remove(0).result,
            Some(who) => as_agent(AgentId::from(who), self.k.send(vec![item])).await.remove(0).result,
        };
        self.log.push(out.clone());
        out
    }
    async fn append(&mut self, who: &str, rows: Value) -> Value {
        self.call(json!({"type": "append", "rows": rows}), Some(who)).await
    }
}

fn ids(v: &Value) -> Vec<i64> {
    v["rows"].as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect()
}

/// Run `scenario` on both bundles; the transcripts must be identical.
macro_rules! conform {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            let mut logs = Vec::new();
            for h in [RAM, SQL] {
                let mut t = T { k: store(h, stringify!($name)), log: Vec::new() };
                $body(&mut t).await;
                logs.push(t.log);
            }
            assert_eq!(logs[0], logs[1], "sqlite_store's replies differ from ram_store's");
        }
    };
}

async fn s_rows_are_keyed_by_caller(t: &mut T) {
    let out = t.append("alice", json!([{"id": 101, "role": "user", "content": "hi"}])).await;
    assert_eq!(out, json!({"ids": [101], "appended": 1}));
    assert_eq!(t.append("bob", json!([{"id": 101, "content": "bob"}])).await["ids"], json!([101]));
    let a = t.call(json!({"type": "read"}), Some("alice")).await;
    let b = t.call(json!({"type": "read"}), Some("bob")).await;
    assert_eq!(a["rows"][0]["content"], json!("hi"));
    assert_eq!(b["rows"][0]["content"], json!("bob"));
    let dup = t.append("alice", json!([{"id": 102}, {"id": 101}])).await;
    assert_eq!(dup["reason"], json!("exists"));
    let a = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(a["rows"].as_array().unwrap().len(), 1);
}
conform!(rows_are_keyed_by_caller, s_rows_are_keyed_by_caller);

async fn s_owner_is_never_a_writable_field(t: &mut T) {
    // an `owner` on the call names nobody: the row lands under the CALLER's key
    let out = t.call(json!({"type": "append", "owner": "bob", "rows": [{"id": 5}]}), Some("alice")).await;
    assert_eq!(out["ids"], json!([5]), "{out}");
    let bob = t.call(json!({"type": "read"}), Some("bob")).await;
    assert_eq!(bob["rows"], json!([]));
    let alice = t.call(json!({"type": "read", "owner": "bob"}), Some("alice")).await;
    assert_eq!(ids(&alice), vec![5]);
}
conform!(owner_is_never_a_writable_field, s_owner_is_never_a_writable_field);

async fn s_the_operator_is_keyed_as_kernel(t: &mut T) {
    t.call(json!({"type": "append", "rows": [{"id": 7, "kind": "call"}]}), None).await;
    let got = t.call(json!({"type": "read", "kinds": ["call"]}), None).await;
    assert_eq!(ids(&got), vec![7]);
    let a = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(a["rows"], json!([]));
}
conform!(the_operator_is_keyed_as_kernel, s_the_operator_is_keyed_as_kernel);

async fn s_refs_must_name_own_rows(t: &mut T) {
    t.append("bob", json!([{"id": 10}])).await;
    let out = t.append("alice", json!([{"id": 20, "kind": "fold", "refs": [10]}])).await;
    assert_eq!(out["reason"], json!("not_found"));
    let out = t.append("alice", json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}])).await;
    assert_eq!(out["ids"], json!([1, 2, 3]));
    t.append("alice", json!([{"id": 4, "refs": ["x"]}])).await;
    t.append("alice", json!([{"id": "5"}])).await;
}
conform!(refs_must_name_own_rows, s_refs_must_name_own_rows);

async fn s_prune_cascades_to_fold_refs(t: &mut T) {
    t.append("alice", json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}])).await;
    let out = t.call(json!({"type": "prune", "before_id": 2}), Some("alice")).await;
    assert_eq!(out, json!({"pruned": 1}));
    let fold = t.call(json!({"type": "read", "ids": [3]}), Some("alice")).await;
    assert_eq!(fold["rows"][0]["refs"], json!([2]));
    // every member pruned: the fold keeps an EMPTY refs list
    t.call(json!({"type": "prune", "ids": [2]}), Some("alice")).await;
    let fold = t.call(json!({"type": "read", "ids": [3]}), Some("alice")).await;
    assert_eq!(fold["rows"][0]["refs"], json!([]));
    // keep: kinds survive a prune
    t.append("alice", json!([{"id": 8, "kind": "view"}, {"id": 9}])).await;
    t.call(json!({"type": "prune", "before_id": 99, "keep": ["view", "fold"]}), Some("alice")).await;
    t.call(json!({"type": "read"}), Some("alice")).await;
    t.call(json!({"type": "prune"}), Some("alice")).await;
}
conform!(prune_cascades_to_fold_refs, s_prune_cascades_to_fold_refs);

async fn s_search_lines_and_fold_descent(t: &mut T) {
    t.append("alice", json!([
        {"id": 1, "content": "alpha\nthe audit marker is 7\nomega"},
        {"id": 2, "content": "nothing"},
        {"id": 3, "kind": "fold", "refs": [1, 2], "content": "[fold] survey"},
        {"id": 4, "title": "Audit Title", "content": {"structured": "audit in json"}},
        {"id": 5, "content": "glob chars *?[] and 100% _under_score, ünïcödé AUDIT"},
    ])).await;
    let got = t.call(json!({"type": "search", "query": "AUDIT"}), Some("alice")).await;
    assert!(got["total"].as_i64().unwrap() >= 3);
    let scoped = t.call(json!({"type": "search", "query": "marker", "ids": [3]}), Some("alice")).await;
    assert_eq!(scoped["total"], json!(1));
    // PARITY on the hard cases: 1–2 char queries (under the trigram), glob and
    // LIKE metacharacters, unicode case folding, limit/width/truncation
    for q in ["a", "au", "*?[", "100%", "_under", "ÜNÏ", "audit in", "zzz"] {
        t.call(json!({"type": "search", "query": q}), Some("alice")).await;
    }
    t.call(json!({"type": "search", "query": "a", "limit": 2, "width": 5}), Some("alice")).await;
    t.call(json!({"type": "search", "query": ""}), Some("alice")).await;
}
conform!(search_lines_and_fold_descent, s_search_lines_and_fold_descent);

async fn s_read_paging_and_selectors(t: &mut T) {
    t.append("alice", json!([
        {"id": 1, "kind": "call", "name": "job"},
        {"id": 2, "kind": "clear"},
        {"id": 3},
        {"id": 4, "kind": "view"},
    ])).await;
    let head = t.call(json!({"type": "read", "from_last_kind": "clear"}), Some("alice")).await;
    assert_eq!(ids(&head), vec![2, 3, 4]);
    let named = t.call(json!({"type": "read", "match": {"name": "job"}}), Some("alice")).await;
    assert_eq!(ids(&named), vec![1]);
    let last_view = t.call(json!({"type": "read", "kinds": ["view"], "order": "desc", "limit": 1}), Some("alice")).await;
    assert_eq!(ids(&last_view), vec![4]);
    let page = t.call(json!({"type": "read", "limit": 2}), Some("alice")).await;
    assert_eq!(ids(&page), vec![1, 2]);
    assert_eq!(page["next"], json!(2));
    let rest = t.call(json!({"type": "read", "after": 2}), Some("alice")).await;
    assert_eq!(ids(&rest), vec![3, 4]);
    t.call(json!({"type": "read", "order": "desc", "after": 4, "before": 2}), Some("alice")).await;
    t.call(json!({"type": "read", "ids": [4, "1", 99]}), Some("alice")).await;
}
conform!(read_paging_and_selectors, s_read_paging_and_selectors);

async fn s_dump_inject_roundtrip(t: &mut T) {
    t.append("alice", json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}])).await;
    // a plain verb — the gate decides who reaches it
    let by_alice = t.call(json!({"type": "dump"}), Some("alice")).await;
    assert!(by_alice["tables"].is_object(), "{by_alice}");
    let tables = t.call(json!({"type": "dump"}), None).await["tables"].clone();
    t.call(json!({"type": "prune", "before_id": 99}), Some("alice")).await;
    let empty = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(empty["rows"], json!([]));
    let out = t.call(json!({"type": "inject", "tables": tables}), None).await;
    assert_eq!(out, json!({"rows": 3}));
    assert_eq!(t.call(json!({"type": "dump"}), None).await["tables"], tables);
    // a broken FK is refused and nothing changes
    let mut bad = tables.clone();
    bad["fold_refs"][0]["members"] = json!([999]);
    let refused = t.call(json!({"type": "inject", "tables": bad}), None).await;
    assert_eq!(refused["reason"], json!("bad_arg"));
    assert_eq!(t.call(json!({"type": "dump"}), None).await["tables"], tables);
}
conform!(dump_inject_roundtrip, s_dump_inject_roundtrip);

// A CURSOR READ ("after my newest row") with every other filter, both orders:
// the SQL store prunes by id before decoding rows, and must answer exactly what
// ram_store answers.
async fn s_cursor_reads_match_with_every_filter(t: &mut T) {
    t.append("alice", json!([
        {"id": 1, "agent": "a", "kind": "call"}, {"id": 2, "agent": "b", "kind": "settle"},
        {"id": 3, "agent": "a", "kind": "settle"}, {"id": 4, "agent": "a", "kind": "call"},
        {"id": 5, "agent": "b", "kind": "call"}, {"id": 6, "agent": "a", "kind": "flush"},
        {"id": 7, "agent": "b", "kind": "flush", "a.b": "x"},
    ])).await;
    for q in [
        json!({"type": "read", "after": 2}),
        json!({"type": "read", "after": 2, "match": {"agent": "a"}}),
        json!({"type": "read", "after": 1, "before": 6, "kinds": ["call"]}),
        json!({"type": "read", "after": 5, "order": "desc", "match": {"agent": "a"}}),
        json!({"type": "read", "after": 6}),
        json!({"type": "read", "after": 2, "limit": 2}),
        json!({"type": "read", "after": 3, "from_last_kind": "call"}),
        // no cursor: the named-call lookup shape, a dotted key, a non-string value
        json!({"type": "read", "kinds": ["call"], "match": {"agent": "a"}, "limit": 1000}),
        json!({"type": "read", "match": {"a.b": "x"}}),
        json!({"type": "read", "match": {"id": 4}}),
        json!({"type": "read", "kinds": ["call", "flush"], "match": {"agent": "b"}}),
    ] {
        t.call(q, Some("alice")).await;
    }
}
conform!(cursor_reads_match_with_every_filter, s_cursor_reads_match_with_every_filter);

/// A ram_store dump injects into sqlite and back, identically.
#[tokio::test]
async fn a_ram_dump_injects_into_sqlite_and_back() {
    let ram = store(RAM, "x-ram");
    let sql = store(SQL, "x-sql");
    let mut r = T { k: ram, log: Vec::new() };
    let mut s = T { k: sql, log: Vec::new() };
    r.append("alice", json!([{"id": 1, "content": "a"}, {"id": 2}, {"id": 3, "kind": "clear", "refs": [1, 2]}])).await;
    r.append("bob", json!([{"id": 1, "content": "b"}])).await;
    let tables = r.call(json!({"type": "dump"}), None).await["tables"].clone();
    assert_eq!(s.call(json!({"type": "inject", "tables": tables.clone()}), None).await, json!({"rows": 4}));
    let back = s.call(json!({"type": "dump"}), None).await["tables"].clone();
    assert_eq!(back, tables, "sqlite gives back exactly the ram dump");
    let mut r2 = T { k: store(RAM, "x-ram2"), log: Vec::new() };
    r2.call(json!({"type": "inject", "tables": back}), None).await;
    assert_eq!(r2.call(json!({"type": "dump"}), None).await["tables"], tables);
}

#[tokio::test]
async fn reflect_names_the_same_verbs_and_says_durable() {
    for h in [RAM, SQL] {
        let mut t = T { k: store(h, &format!("reflect-{}", h.len())), log: Vec::new() };
        t.append("alice", json!([{"id": 1}])).await;
        let r = t.call(json!({"type": "reflect"}), None).await;
        assert_eq!(r["durable"], json!(h == SQL));
        assert_eq!(r["owners"], json!(1));
        assert_eq!(r["rows"], json!(1));
        assert_eq!(r["optional_verbs"], json!(["search"]));
        // HOW TO CALL it is the kernel's `docs`, never on a reflect
        assert!(r.get("verbs").is_none(), "a reflect carries no verbs");
        let d = t.k.send(vec![Item::new(AgentId::from("kernel"), json!({"type": "docs", "of": "mem"}))]).await.remove(0).result;
        let verbs: Vec<&String> = d["verbs"].as_object().unwrap().keys().collect();
        assert_eq!(
            verbs,
            vec!["reflect", "boot", "append", "read", "search", "prune", "inject", "dump"]
        );
    }
}

/// Every verb doc but `boot` (which names the file) is ram_store's, word for word.
#[test]
fn verb_docs_match_ram_store() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../lib/rust/crates/bundles/fantastic-ram-store/src/lib.rs"),
    )
    .unwrap();
    for (name, doc) in fantastic_sqlite_store::VERB_DOCS {
        if name == "boot" {
            continue;
        }
        assert!(src.contains(&format!("(\"{name}\", \"{doc}\")")), "{name} drifted from ram_store");
    }
}
