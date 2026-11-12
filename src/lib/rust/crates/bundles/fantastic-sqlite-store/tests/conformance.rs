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
    let mut k = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: dir.clone().join(".fantastic"),
        work_dir: dir.clone(),
    });
    k.bundles
        .register(RAM, fantastic_ram_store::RamStoreBundle::default());
    k.bundles
        .register(SQL, fantastic_sqlite_store::SqliteStoreBundle::default());
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
    h: &'static str,
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
    /// A call whose request body is `bytes` at `data`; the reply is logged with the
    /// bytes it carried back, so the two stores' transcripts cover the body too.
    async fn call_bytes(&mut self, payload: Value, bytes: Option<&[u8]>, who: &str) -> (Value, Option<Vec<u8>>) {
        let mut item = Item::new(AgentId::from("mem"), payload);
        if let Some(b) = bytes {
            item = item.with_binary_key("data", b.to_vec());
        }
        let out = as_agent(AgentId::from(who), self.k.send(vec![item])).await.remove(0);
        let body = out.body_bytes().map(<[u8]>::to_vec);
        self.log.push(json!([out.result.clone(), body]));
        (out.result, body)
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
                let mut t = T { h, k: store(h, stringify!($name)), log: Vec::new() };
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
    // an `owner` on the call is no argument of the verb: refused by name, nothing written
    let out = t.call(json!({"type": "append", "owner": "bob", "rows": [{"id": 5}]}), Some("alice")).await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
    assert_eq!(out["arg"], json!("owner"), "{out}");
    let alice = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(alice["rows"], json!([]));
    let read = t.call(json!({"type": "read", "owner": "bob"}), Some("alice")).await;
    assert_eq!(read["reason"], json!("bad_arg"), "{read}");
    assert_eq!(read["arg"], json!("owner"), "{read}");
    // the row lands under the CALLER's key
    t.append("alice", json!([{"id": 5}])).await;
    let bob = t.call(json!({"type": "read"}), Some("bob")).await;
    assert_eq!(bob["rows"], json!([]));
    let alice = t.call(json!({"type": "read"}), Some("alice")).await;
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

// ─── the rest of ram_store's own scenarios, same transcripts ──────────────────

/// `keep` is a list of kinds; anything else is refused by name.
async fn s_prune_keep_is_a_list_of_kinds(t: &mut T) {
    t.append("alice", json!([{"id": 1, "kind": "fold"}, {"id": 2, "kind": "old"}, {"id": 3, "kind": "msg"}])).await;
    let out = t.call(json!({"type": "prune", "before_id": 1_000_000, "keep": ["fold", "old"]}), Some("alice")).await;
    assert_eq!(out["pruned"], json!(1), "only `msg` is not kept: {out}");
    t.append("alice", json!([{"id": 8, "content": "kindless"}])).await;
    let out = t.call(json!({"type": "prune", "before_id": 1_000_000, "keep": ["fold"]}), Some("alice")).await;
    assert_eq!(out["pruned"], json!(2), "`old` and the kind-less row go: {out}");
    for keep in [json!("fold"), json!(5), json!({"fold": 1})] {
        let out = t.call(json!({"type": "prune", "before_id": 1_000_000, "keep": keep}), Some("alice")).await;
        assert_eq!(out["reason"], json!("bad_arg"), "{out}");
        assert_eq!(out["arg"], json!("keep"), "{out}");
    }
    let left = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(ids(&left).len(), 1, "{left}");
}
conform!(prune_keep_is_a_list_of_kinds, s_prune_keep_is_a_list_of_kinds);

async fn s_a_zero_limit_is_refused(t: &mut T) {
    t.append("alice", json!([{"id": 1, "kind": "msg", "content": "x one"}, {"id": 2, "kind": "msg", "content": "x two"}])).await;
    for (payload, arg) in [
        (json!({"type": "read", "limit": 0}), "limit"),
        (json!({"type": "search", "query": "x", "limit": 0}), "limit"),
        (json!({"type": "search", "query": "x", "width": 0}), "width"),
    ] {
        let r = t.call(payload.clone(), Some("alice")).await;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some(arg)), "{payload}: {r}");
    }
    let r = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(ids(&r).len(), 2, "{r}");
    assert_eq!(r["next"], Value::Null);
}
conform!(a_zero_limit_is_refused, s_a_zero_limit_is_refused);

async fn s_digit_string_refs_are_refused_as_not_row_ids(t: &mut T) {
    let first = t.append("alice", json!([{"id": 4, "kind": "msg", "content": "a"}])).await;
    let rid = first["ids"][0].as_i64().unwrap();
    let r = t.append("alice", json!([{"id": 9, "kind": "fold", "content": "f", "refs": [rid.to_string()]}])).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(
        r["error"],
        format!("append: the arguments do not parse (invalid type: string \"{rid}\", expected i64)"),
        "{r}"
    );
    let r = t.append("alice", json!([{"id": 9, "kind": "fold", "content": "f", "refs": [rid]}])).await;
    assert_eq!(r["appended"], 1, "{r}");
}
conform!(digit_string_refs_are_refused_as_not_row_ids, s_digit_string_refs_are_refused_as_not_row_ids);

async fn s_limits_and_ids_are_typed_integers(t: &mut T) {
    let rows: Vec<Value> = (1..=5).map(|i| json!({"id": i, "content": format!("needle {i}")})).collect();
    t.append("alice", Value::Array(rows)).await;
    for (limit, want) in [(json!(2), 2), (json!(3), 3)] {
        let r = t.call(json!({"type": "search", "query": "needle", "limit": limit.clone()}), Some("alice")).await;
        assert_eq!(r["matches"].as_array().unwrap().len(), want, "limit={limit}: {r}");
        assert_eq!(r["total"], json!(5), "limit={limit}");
        assert_eq!(r["truncated"], json!(want < 5), "limit={limit}");
    }
    for limit in [json!("3"), json!(" 2 "), json!(-1), json!(2.0), json!(true)] {
        let r = t.call(json!({"type": "search", "query": "needle", "limit": limit.clone()}), Some("alice")).await;
        assert_eq!(r["reason"], json!("bad_arg"), "limit={limit}: {r}");
        assert_eq!(r["arg"], json!("limit"), "limit={limit}: {r}");
    }
    let r = t.call(json!({"type": "search", "query": "needle", "width": 3}), Some("alice")).await;
    assert_eq!(r["matches"][0]["text"], json!("nee"));
    let r = t.call(json!({"type": "read", "ids": ["1"]}), Some("alice")).await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    assert_eq!(r["arg"], json!("ids"), "{r}");
}
conform!(limits_and_ids_are_typed_integers, s_limits_and_ids_are_typed_integers);

fn wire(v: &Value) -> String {
    serde_json::to_string(v).unwrap()
}

/// `preserve_order` makes a refusal's KEY ORDER part of the wire, and `Value`
/// equality ignores it, so this compares the serialized text. Only the verbs the
/// store lists differ between the two bundles (`boot` is sqlite's own).
async fn s_unknown_type_is_coded_unhandled_with_the_raw_verb(t: &mut T) {
    let verbs = if t.h == SQL {
        "boot, append, read, search, prune, inject, dump, put_bytes, get_bytes, reflect"
    } else {
        "append, read, search, prune, inject, dump, put_bytes, get_bytes, reflect"
    };
    for (verb, shown) in [("frob", "'frob'"), ("it's", "\\\"it's\\\"")] {
        let want = format!(
            r#"{{"error":"unknown verb {shown}; mem answers: {verbs}","reason":"unhandled","hint":"docs of=mem lists each verb's arguments","verb":"{verb}"}}"#
        );
        assert_eq!(wire(&t.call(json!({"type": verb}), Some("alice")).await), want);
    }
    // `shutdown` is no verb of either store: refused like any unknown one
    let r = t.call(json!({"type": "shutdown"}), Some("alice")).await;
    assert_eq!(r["reason"], json!("unhandled"));
}

async fn s_arg_refusals_carry_error_reason_arg_in_that_order(t: &mut T) {
    t.append("alice", json!([{"id": 1, "content": "a"}])).await;
    for (payload, why) in [
        (json!({"type": "append", "rows": [{"id": "5"}]}), "invalid type: string \"5\", expected a nonzero u64"),
        (json!({"type": "append", "rows": [{"id": 0}]}), "invalid value: integer `0`, expected a nonzero u64"),
        (json!({"type": "append", "rows": [{"id": 7, "refs": ["x"]}]}), "invalid type: string \"x\", expected i64"),
    ] {
        let got = t.call(payload.clone(), Some("alice")).await;
        assert_eq!(got["reason"], "bad_arg", "{payload}: {got}");
        assert_eq!(got["error"], format!("append: the arguments do not parse ({why})"), "{payload}: {got}");
    }
    for (payload, reason) in [
        (json!({"type": "append", "rows": []}), "bad_arg"),
        (json!({"type": "append"}), "missing_arg"),
        (json!({"type": "append", "rows": "x"}), "bad_arg"),
        (json!({"type": "append", "rows": [1]}), "bad_arg"),
    ] {
        let got = t.call(payload.clone(), Some("alice")).await;
        assert_eq!(got["reason"], json!(reason), "{payload}: {got}");
        assert_eq!(got["arg"], json!("rows"), "{payload}: {got}");
    }
}

/// The refusal's wire text (key order) is asserted inside the scenario on EACH bundle;
/// the two messages name their own store, so the transcripts are not compared.
macro_rules! conform_wire {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            for h in [RAM, SQL] {
                let mut t = T { h, k: store(h, stringify!($name)), log: Vec::new() };
                $body(&mut t).await;
            }
        }
    };
}
conform_wire!(unknown_type_is_coded_unhandled_with_the_raw_verb, s_unknown_type_is_coded_unhandled_with_the_raw_verb);
conform_wire!(arg_refusals_carry_error_reason_arg_in_that_order, s_arg_refusals_carry_error_reason_arg_in_that_order);

async fn s_two_stores_hold_separate_tables(t: &mut T) {
    let handler = t.h;
    let mut meta = json!({"callable_by": {"alice": "*"}});
    if handler == SQL {
        meta["path"] = json!("stores/second.db");
    }
    t.k.register(agent("mem2", Some(handler), Some("kernel_state"), meta));
    t.k.update(&AgentId::from("alice"), json!({"can_call": {"mem": "*", "mem2": "*"}}).as_object().cloned().unwrap());
    t.append("alice", json!([{"id": 1, "content": "in mem"}])).await;
    let read2 = as_agent(AgentId::from("alice"), t.k.send(vec![Item::new(AgentId::from("mem2"), json!({"type": "read"}))]))
        .await
        .remove(0)
        .result;
    t.log.push(read2.clone());
    assert_eq!(read2["rows"], json!([]), "mem2 shares nothing with mem: {read2}");
    let gone = t
        .k
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "delete_agent", "id": "mem2"}))])
        .await
        .remove(0)
        .result;
    assert!(gone.get("error").is_none(), "{gone}");
    let read1 = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(ids(&read1), vec![1], "mem keeps its rows");
}
conform!(two_stores_hold_separate_tables, s_two_stores_hold_separate_tables);

/// alice, bob and the operator write colliding ids into ONE store: each reads only
/// its own, the dump lists three owners, nobody reaches another's rows.
async fn s_several_owners_one_file(t: &mut T) {
    t.append("alice", json!([{"id": 1, "content": "alice one"}, {"id": 2, "content": "alice two"}])).await;
    t.append("bob", json!([{"id": 1, "content": "bob one"}])).await;
    t.call(json!({"type": "append", "rows": [{"id": 1, "kind": "call", "content": "operator"}]}), None).await;
    for (who, want) in [(Some("alice"), vec![1, 2]), (Some("bob"), vec![1]), (None, vec![1])] {
        let r = t.call(json!({"type": "read"}), who).await;
        assert_eq!(ids(&r), want, "{who:?}: {r}");
    }
    let bob = t.call(json!({"type": "read"}), Some("bob")).await;
    assert_eq!(bob["rows"][0]["content"], json!("bob one"));
    let tables = t.call(json!({"type": "dump"}), None).await["tables"].clone();
    let mut owners: Vec<&str> = tables["rows"].as_array().unwrap().iter().map(|r| r["owner"].as_str().unwrap()).collect();
    owners.sort_unstable();
    owners.dedup();
    assert_eq!(owners, vec!["g1|alice", "g1|bob", "g1|kernel"], "{tables}");
    // bob cannot prune alice's rows: his ids name HIS rows
    let out = t.call(json!({"type": "prune", "ids": [2]}), Some("bob")).await;
    assert_eq!(out["pruned"], json!(0), "{out}");
    let out = t.call(json!({"type": "prune", "before_id": 99}), Some("bob")).await;
    assert_eq!(out["pruned"], json!(1), "{out}");
    let a = t.call(json!({"type": "read"}), Some("alice")).await;
    assert_eq!(ids(&a), vec![1, 2], "alice is untouched: {a}");
    let op = t.call(json!({"type": "read"}), None).await;
    assert_eq!(ids(&op), vec![1], "the operator is untouched: {op}");
}
conform!(several_owners_one_file, s_several_owners_one_file);

/// A row holding the non-JSON `Infinity` number cannot be typed into the declared
/// `rows`: BOTH stores refuse the call whole and keep answering.
#[tokio::test]
async fn a_non_json_number_row_is_refused_by_both() {
    let inf = fantastic_kernel::test_support::non_finite("Infinity");
    for h in [RAM, SQL] {
        let mut t = T { h, k: store(h, "nonjson"), log: Vec::new() };
        let mut row = serde_json::Map::new();
        row.insert("id".to_string(), Value::from(7));
        row.insert("job_id".to_string(), inf.clone());
        let mut payload = serde_json::Map::new();
        payload.insert("type".to_string(), Value::from("append"));
        payload.insert("rows".to_string(), Value::Array(vec![Value::Object(row)]));
        let out = t.call(Value::Object(payload), Some("alice")).await;
        assert_eq!(out["reason"], json!("bad_arg"), "{h}: {out}");
        // the store survives
        let ok = t.append("bob", json!([{"id": 1}])).await;
        assert_eq!(ok["appended"], json!(1), "{ok}");
    }
}

/// A ram_store dump injects into sqlite and back, identically.
#[tokio::test]
async fn a_ram_dump_injects_into_sqlite_and_back() {
    let ram = store(RAM, "x-ram");
    let sql = store(SQL, "x-sql");
    let mut r = T { h: RAM, k: ram, log: Vec::new() };
    let mut s = T { h: SQL, k: sql, log: Vec::new() };
    r.append("alice", json!([{"id": 1, "content": "a"}, {"id": 2}, {"id": 3, "kind": "clear", "refs": [1, 2]}])).await;
    r.append("bob", json!([{"id": 1, "content": "b"}])).await;
    let tables = r.call(json!({"type": "dump"}), None).await["tables"].clone();
    assert_eq!(s.call(json!({"type": "inject", "tables": tables.clone()}), None).await, json!({"rows": 4}));
    let back = s.call(json!({"type": "dump"}), None).await["tables"].clone();
    assert_eq!(back, tables, "sqlite gives back exactly the ram dump");
    let mut r2 = T { h: RAM, k: store(RAM, "x-ram2"), log: Vec::new() };
    r2.call(json!({"type": "inject", "tables": back}), None).await;
    assert_eq!(r2.call(json!({"type": "dump"}), None).await["tables"], tables);
}

#[tokio::test]
async fn reflect_names_the_same_verbs_and_says_durable() {
    for h in [RAM, SQL] {
        let mut t = T { h, k: store(h, &format!("reflect-{}", h.len())), log: Vec::new() };
        t.append("alice", json!([{"id": 1}])).await;
        let r = t.call(json!({"type": "reflect"}), None).await;
        assert_eq!(r["durable"], json!(h == SQL));
        assert_eq!(r["owners"], json!(1));
        assert_eq!(r["rows"], json!(1));
        assert_eq!(r["optional_verbs"], json!(["search"]));
        // the verbs it answers: rendered from the declarations, in declaration order
        let d = t.k.send(vec![Item::new(AgentId::from("kernel"), json!({"type": "docs", "of": "mem"}))]).await.remove(0).result;
        let verbs: Vec<&String> = d["verbs"].as_object().unwrap().keys().collect();
        let mut want = vec!["append", "read", "search", "prune", "inject", "dump", "put_bytes", "get_bytes", "reflect"];
        if h == SQL {
            want = vec!["boot", "append", "read", "search", "prune", "inject", "dump", "put_bytes", "get_bytes", "reflect"];
        }
        assert_eq!(verbs, want);
    }
}

/// Every verb but `boot` (which opens the file) is declared once, in ram_store:
/// the same names in the same order, the same argument schemas.
#[test]
fn verbs_match_ram_store() {
    let ram = fantastic_ram_store::verbs::VERBS;
    let sql: Vec<_> = fantastic_sqlite_store::verbs::VERBS
        .iter()
        .filter(|v| v.name != "boot")
        .collect();
    assert_eq!(ram.len(), sql.len());
    for (r, q) in ram.iter().zip(sql) {
        assert_eq!(r.name, q.name);
        assert_eq!(r.schema(), q.schema(), "{} drifted from ram_store", r.name);
    }
}

async fn s_bytes_round_trip_and_stay_out_of_the_rows(t: &mut T) {
    t.append("alice", json!([{"id": 1, "content": "hi"}])).await;
    let rows = t.call(json!({"type": "read"}), Some("alice")).await;
    let dump = t.call(json!({"type": "dump"}), None).await;
    let raw: Vec<u8> = (0..=255u8).collect();
    let (put, _) = t
        .call_bytes(json!({"type": "put_bytes", "ref": "b1", "media_type": "image/png"}), Some(&raw), "alice")
        .await;
    assert_eq!(put, json!({"ref": "b1", "media_type": "image/png", "bytes": 256}));
    let (got, body) = t.call_bytes(json!({"type": "get_bytes", "ref": "b1"}), None, "alice").await;
    assert_eq!(got["media_type"], json!("image/png"));
    assert_eq!(body, Some(raw));
    // another caller's space is empty; a missing ref and a missing body are refused
    let (r, _) = t.call_bytes(json!({"type": "get_bytes", "ref": "b1"}), None, "bob").await;
    assert_eq!(r["reason"], json!("not_found"));
    let (r, _) = t.call_bytes(json!({"type": "get_bytes"}), None, "alice").await;
    assert_eq!(r["reason"], json!("missing_arg"));
    let (r, _) = t
        .call_bytes(json!({"type": "put_bytes", "ref": "b2", "media_type": "image/png"}), None, "alice")
        .await;
    assert_eq!(r["reason"], json!("missing_arg"));
    // rows, search and dump do not move
    assert_eq!(t.call(json!({"type": "read"}), Some("alice")).await, rows);
    assert_eq!(t.call(json!({"type": "dump"}), None).await, dump);
    // inject replaces the whole store, bytes included
    t.call(json!({"type": "inject", "tables": dump["tables"].clone()}), None).await;
    let (r, _) = t.call_bytes(json!({"type": "get_bytes", "ref": "b1"}), None, "alice").await;
    assert_eq!(r["reason"], json!("not_found"));
}
conform!(bytes_round_trip_and_stay_out_of_the_rows, s_bytes_round_trip_and_stay_out_of_the_rows);
