//! ram_store: tenancy by caller, grants + FK cascade, search, inject/dump — the
//! twin of python `ram_store/tests/test_ram_store.py`.
//!
//! Callers act through `with_sender` (a real agent send, gated both ways); the
//! operator is a bare `kernel.send`. The root's guid is set by hand — the root is
//! not booted here, and the store keys every row by it.

use super::*;
use fantastic_kernel::agent::Agent;
use fantastic_kernel::{with_remote_caller, with_sender, Item};

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

fn store() -> Arc<Kernel> {
    let mut k = Kernel::new();
    k.bundles
        .register(HANDLER_MODULE, RamStoreBundle::default());
    let k = Arc::new(k);
    let root = agent("kernel_state", None, None, json!({"guid": "g1"}));
    k.register(Arc::clone(&root));
    k.set_root(root);
    for a in ["alice", "bob", "carol", "leg"] {
        k.register(agent(
            a,
            None,
            Some("kernel_state"),
            json!({"can_call": {"mem": "*"}}),
        ));
    }
    k.register(agent(
        "mem",
        Some(HANDLER_MODULE),
        Some("kernel_state"),
        json!({
            "operator": true,
            "admins": ["kernel"],
            "callable_by": {"alice": "*", "bob": "*", "carol": "*", "leg": "*",
                            // a relayed call is gated AS the foreign caller: named
                            "g2|scout": "*"},
        }),
    ));
    k
}

async fn call(k: &Arc<Kernel>, payload: Value, as_: Option<&str>) -> Value {
    let item = Item::new(AgentId::from("mem"), payload);
    match as_ {
        None => k.send(vec![item]).await.remove(0).result,
        Some(who) => {
            with_sender(AgentId::from(who), k.send(vec![item]))
                .await
                .remove(0)
                .result
        }
    }
}

async fn append(k: &Arc<Kernel>, who: &str, rows: Value) -> Value {
    call(k, json!({"type": "append", "rows": rows}), Some(who)).await
}

fn ids(v: &Value) -> Vec<i64> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect()
}

fn update(k: &Arc<Kernel>, id: &str, patch: Value) {
    k.update(&AgentId::from(id), patch.as_object().unwrap().clone());
}

#[tokio::test]
async fn rows_are_keyed_by_caller() {
    let k = store();
    let out = append(
        &k,
        "alice",
        json!([{"id": 101, "role": "user", "content": "hi"}]),
    )
    .await;
    assert_eq!(out, json!({"ids": [101], "appended": 1}));
    // the same id is free under another owner — keys are (owner, id)
    assert_eq!(
        append(&k, "bob", json!([{"id": 101, "content": "bob"}])).await["ids"],
        json!([101])
    );
    let a = call(&k, json!({"type": "read"}), Some("alice")).await;
    let b = call(&k, json!({"type": "read"}), Some("bob")).await;
    assert_eq!(a["rows"][0]["content"], json!("hi"));
    assert_eq!(b["rows"][0]["content"], json!("bob"));
    // a duplicate id under one owner is refused, all or none
    let dup = append(&k, "alice", json!([{"id": 102}, {"id": 101}])).await;
    assert_eq!(dup["reason"], json!("exists"));
    let a = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(a["rows"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn owner_is_never_a_writable_field() {
    let k = store();
    let out = call(
        &k,
        json!({"type": "append", "owner": "bob", "rows": [{"id": 5}]}),
        Some("alice"),
    )
    .await;
    assert_eq!(out["reason"], json!("unauthorized"));
    let out = call(&k, json!({"type": "read", "owner": "bob"}), Some("alice")).await;
    assert_eq!(out["reason"], json!("unauthorized"));
}

#[tokio::test]
async fn missing_id_takes_the_kernels_next() {
    let k = store();
    let before = k.rows.next_id();
    let out = append(&k, "alice", json!([{"content": "x"}])).await;
    assert!(out["ids"][0].as_i64().unwrap() >= before);
}

#[tokio::test]
async fn operator_needs_explicit_service() {
    let k = store();
    update(&k, "mem", json!({"operator": false}));
    let out = call(&k, json!({"type": "read"}), None).await;
    assert_eq!(out["reason"], json!("unauthorized"));
    update(&k, "mem", json!({"operator": true}));
    call(
        &k,
        json!({"type": "append", "rows": [{"id": 7, "kind": "call"}]}),
        None,
    )
    .await;
    let got = call(&k, json!({"type": "read", "kinds": ["call"]}), None).await;
    assert_eq!(ids(&got), vec![7]);
    // the operator's rows are its own key — alice sees none of them
    let a = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(a["rows"], json!([]));
}

#[tokio::test]
async fn admin_may_name_an_owner() {
    let k = store();
    append(&k, "alice", json!([{"id": 1, "content": "a"}])).await;
    let got = call(&k, json!({"type": "read", "owner": "alice"}), None).await;
    assert_eq!(ids(&got), vec![1]);
    let out = call(
        &k,
        json!({"type": "append", "owner": "alice", "rows": [{"id": 2}]}),
        None,
    )
    .await;
    assert_eq!(out["ids"], json!([2]));
}

#[tokio::test]
async fn refs_must_name_own_rows() {
    let k = store();
    append(&k, "bob", json!([{"id": 10}])).await;
    let out = append(
        &k,
        "alice",
        json!([{"id": 20, "kind": "fold", "refs": [10]}]),
    )
    .await;
    assert_eq!(out["reason"], json!("not_found"));
    // members in the same batch are fine
    let out = append(
        &k,
        "alice",
        json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}]),
    )
    .await;
    assert_eq!(out["ids"], json!([1, 2, 3]));
}

#[tokio::test]
async fn share_read_and_zero_tolerance() {
    let k = store();
    append(
        &k,
        "alice",
        json!([{"id": 1, "content": "a1"}, {"id": 2, "content": "a2"}, {"id": 3, "content": "a3"}]),
    )
    .await;
    append(&k, "bob", json!([{"id": 50, "content": "b"}])).await;
    let g = call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"head_until": 2}, "why": "context"}),
        Some("alice"),
    )
    .await;
    assert_eq!(g["created"], json!(true));
    assert_eq!(g["anchor"], json!(1));
    let got = call(
        &k,
        json!({"type": "read", "grant": g["grant"]}),
        Some("bob"),
    )
    .await;
    assert_eq!(ids(&got), vec![1, 2]);
    // carol holds no grant
    let out = call(
        &k,
        json!({"type": "read", "grant": g["grant"]}),
        Some("carol"),
    )
    .await;
    assert_eq!(out["reason"], json!("unauthorized"));
    // bob cannot share alice's rows — not under his key
    let out = call(
        &k,
        json!({"type": "share", "with": "carol", "scope": {"row": 1}}),
        Some("bob"),
    )
    .await;
    assert_eq!(out["reason"], json!("not_found"));
    // an identical share is the same grant
    let again = call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"head_until": 2}}),
        Some("alice"),
    )
    .await;
    assert_eq!(again["grant"], g["grant"]);
    assert_eq!(again["created"], json!(false));
}

#[tokio::test]
async fn frozen_stops_live_follows() {
    let k = store();
    append(&k, "alice", json!([{"id": 1}, {"id": 2}])).await;
    let frozen = call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"head_until": 2}}),
        Some("alice"),
    )
    .await;
    let live = call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"head_until": 2}, "mode": "live"}),
        Some("alice"),
    )
    .await;
    append(&k, "alice", json!([{"id": 3}])).await;
    let f = call(
        &k,
        json!({"type": "read", "grant": frozen["grant"]}),
        Some("bob"),
    )
    .await;
    let lv = call(
        &k,
        json!({"type": "read", "grant": live["grant"]}),
        Some("bob"),
    )
    .await;
    assert_eq!(ids(&f), vec![1, 2]);
    assert_eq!(ids(&lv), vec![1, 2, 3]);
}

#[tokio::test]
async fn fold_scope_reaches_members() {
    let k = store();
    append(
        &k,
        "alice",
        json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}, {"id": 4}]),
    )
    .await;
    let g = call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"fold": 3}}),
        Some("alice"),
    )
    .await;
    let got = call(
        &k,
        json!({"type": "read", "grant": g["grant"]}),
        Some("bob"),
    )
    .await;
    assert_eq!(ids(&got), vec![1, 2, 3]);
    assert_eq!(got["rows"][2]["refs"], json!([1, 2]));
}

#[tokio::test]
async fn prune_cascades_to_grants_and_fold_refs() {
    let k = store();
    append(
        &k,
        "alice",
        json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}]),
    )
    .await;
    let g = call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"head_until": 3}}),
        Some("alice"),
    )
    .await;
    let out = call(&k, json!({"type": "prune", "before_id": 2}), Some("alice")).await;
    assert_eq!(out, json!({"pruned": 1, "grants_removed": 1}));
    let gone = call(
        &k,
        json!({"type": "read", "grant": g["grant"]}),
        Some("bob"),
    )
    .await;
    assert_eq!(gone["reason"], json!("not_found"));
    let fold = call(&k, json!({"type": "read", "ids": [3]}), Some("alice")).await;
    assert_eq!(fold["rows"][0]["refs"], json!([2]));
    let listed = call(
        &k,
        json!({"type": "grants", "side": "given"}),
        Some("alice"),
    )
    .await;
    assert_eq!(listed["grants"], json!([]));
}

#[tokio::test]
async fn unshare_and_grants_listing() {
    let k = store();
    append(&k, "alice", json!([{"id": 1}])).await;
    let g = call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"row": 1}}),
        Some("alice"),
    )
    .await;
    let rec = call(
        &k,
        json!({"type": "grants", "side": "received"}),
        Some("bob"),
    )
    .await;
    assert_eq!(rec["grants"][0]["owner"], json!("alice"));
    assert_eq!(rec["grants"].as_array().unwrap().len(), 1);
    let out = call(
        &k,
        json!({"type": "unshare", "grant": g["grant"]}),
        Some("bob"),
    )
    .await;
    assert_eq!(out["reason"], json!("unauthorized"));
    let ok = call(
        &k,
        json!({"type": "unshare", "grant": g["grant"]}),
        Some("alice"),
    )
    .await;
    assert_eq!(ok["unshared"], json!(true));
    let out = call(
        &k,
        json!({"type": "read", "grant": g["grant"]}),
        Some("bob"),
    )
    .await;
    assert_eq!(out["reason"], json!("not_found"));
}

#[tokio::test]
async fn search_lines_and_fold_descent() {
    let k = store();
    append(
        &k,
        "alice",
        json!([
            {"id": 1, "content": "alpha\nthe audit marker is 7\nomega"},
            {"id": 2, "content": "nothing"},
            {"id": 3, "kind": "fold", "refs": [1, 2], "content": "[fold] survey"},
        ]),
    )
    .await;
    let got = call(
        &k,
        json!({"type": "search", "query": "AUDIT"}),
        Some("alice"),
    )
    .await;
    assert_eq!(
        got["matches"],
        json!([{"id": 1, "line": 2, "text": "the audit marker is 7"}])
    );
    let scoped = call(
        &k,
        json!({"type": "search", "query": "marker", "ids": [3]}),
        Some("alice"),
    )
    .await;
    assert_eq!(scoped["total"], json!(1));
}

#[tokio::test]
async fn read_paging_and_selectors() {
    let k = store();
    append(
        &k,
        "alice",
        json!([
            {"id": 1, "kind": "call", "name": "job"},
            {"id": 2, "kind": "clear"},
            {"id": 3},
            {"id": 4, "kind": "view"},
        ]),
    )
    .await;
    let head = call(
        &k,
        json!({"type": "read", "from_last_kind": "clear"}),
        Some("alice"),
    )
    .await;
    assert_eq!(ids(&head), vec![2, 3, 4]);
    let named = call(
        &k,
        json!({"type": "read", "match": {"name": "job"}}),
        Some("alice"),
    )
    .await;
    assert_eq!(ids(&named), vec![1]);
    let last_view = call(
        &k,
        json!({"type": "read", "kinds": ["view"], "order": "desc", "limit": 1}),
        Some("alice"),
    )
    .await;
    assert_eq!(ids(&last_view), vec![4]);
    let page = call(&k, json!({"type": "read", "limit": 2}), Some("alice")).await;
    assert_eq!(ids(&page), vec![1, 2]);
    assert_eq!(page["next"], json!(2));
    let rest = call(&k, json!({"type": "read", "after": 2}), Some("alice")).await;
    assert_eq!(ids(&rest), vec![3, 4]);
}

#[tokio::test]
async fn dump_inject_roundtrip() {
    let k = store();
    append(
        &k,
        "alice",
        json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}]),
    )
    .await;
    call(
        &k,
        json!({"type": "share", "with": "bob", "scope": {"fold": 3}}),
        Some("alice"),
    )
    .await;
    // only admins
    let refused = call(&k, json!({"type": "dump"}), Some("alice")).await;
    assert_eq!(refused["reason"], json!("unauthorized"));
    let tables = call(&k, json!({"type": "dump"}), None).await["tables"].clone();
    call(&k, json!({"type": "prune", "before_id": 99}), Some("alice")).await;
    let empty = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(empty["rows"], json!([]));
    let out = call(&k, json!({"type": "inject", "tables": tables}), None).await;
    assert_eq!(out, json!({"rows": 3, "grants": 1}));
    assert_eq!(
        call(&k, json!({"type": "dump"}), None).await["tables"],
        tables
    );
    // a broken FK is refused and nothing changes
    let mut bad = tables.clone();
    bad["grants"][0]["anchor"] = json!(999);
    let refused = call(&k, json!({"type": "inject", "tables": bad}), None).await;
    assert_eq!(refused["reason"], json!("bad_arg"));
    assert_eq!(
        call(&k, json!({"type": "dump"}), None).await["tables"],
        tables
    );
}

#[tokio::test]
async fn remote_caller_is_keyed_by_its_qualified_address() {
    let k = store();
    let item = Item::new(
        AgentId::from("mem"),
        json!({"type": "append", "rows": [{"id": 1}]}),
    );
    let out = with_remote_caller(
        AgentId::from("leg"),
        "g2|scout".to_string(),
        with_sender(AgentId::from("leg"), k.send(vec![item])),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(out["ids"], json!([1]));
    let tables = call(&k, json!({"type": "dump"}), None).await["tables"].clone();
    let owners: Vec<&str> = tables["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["owner"].as_str().unwrap())
        .collect();
    assert_eq!(owners, vec!["g2|scout"]);
    // the leg's own writes stay the leg's
    append(&k, "leg", json!([{"id": 2}])).await;
    let tables = call(&k, json!({"type": "dump"}), None).await["tables"].clone();
    let mut owners: Vec<String> = tables["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["owner"].as_str().unwrap().to_string())
        .collect();
    owners.sort();
    assert_eq!(owners, vec!["g1|leg".to_string(), "g2|scout".to_string()]);
}

#[tokio::test]
async fn reflect_names_the_posture_and_the_verbs() {
    let k = store();
    append(&k, "alice", json!([{"id": 1}])).await;
    let r = call(&k, json!({"type": "reflect"}), None).await;
    assert_eq!(r["durable"], json!(false));
    assert_eq!(r["owners"], json!(1));
    assert_eq!(r["rows"], json!(1));
    assert_eq!(r["operator"], json!(true));
    assert_eq!(r["admins"], json!(["kernel"]));
    assert_eq!(r["optional_verbs"], json!(["search"]));
    let verbs: Vec<&String> = r["verbs"].as_object().unwrap().keys().collect();
    assert_eq!(
        verbs,
        vec![
            "reflect", "boot", "append", "read", "search", "share", "unshare", "grants", "prune",
            "inject", "dump"
        ]
    );
}

#[tokio::test]
async fn verb_docs_are_pythons_first_docstring_lines() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../python/bundled_agents/memory/ram_store/src/ram_store/tools.py");
    let src = std::fs::read_to_string(&py).expect("python ram_store");
    for (name, doc) in VERB_DOCS {
        let head = format!("async def _{name}(id, payload, agent):\n    \"\"\"");
        let at = src
            .find(&head)
            .unwrap_or_else(|| panic!("no python _{name}"));
        let rest = &src[at + head.len()..];
        let line = rest
            .split('\n')
            .next()
            .unwrap()
            .trim_end_matches("\"\"\"")
            .trim();
        assert_eq!(doc, line, "{name}");
    }
}

#[test]
fn readme_is_pythons_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../python/bundled_agents/memory/ram_store/src/ram_store/readme.md");
    assert_eq!(README, std::fs::read_to_string(py).expect("python readme"));
}

#[tokio::test]
async fn validate_meta_refuses_bad_posture_fields() {
    let b = RamStoreBundle::default();
    let m = |v: Value| v.as_object().unwrap().clone();
    assert!(b
        .validate_meta(&m(json!({"operator": "yes"})), &Map::new())
        .is_some());
    assert!(b
        .validate_meta(&m(json!({"admins": [1]})), &Map::new())
        .is_some());
    assert!(b
        .validate_meta(&m(json!({"admins": "kernel"})), &Map::new())
        .is_none());
    assert!(b
        .validate_meta(
            &m(json!({"operator": true, "admins": ["kernel"]})),
            &Map::new()
        )
        .is_none());
}

/// A row may carry the one number the kernel keeps whose text is not JSON — the
/// `Infinity` a model's `3e500123` (a hex job id `float()` reads as infinity)
/// coerces to. Reading it back must answer, not panic: a panic here fires under
/// the store's lock and poisons it, so every later call on EVERY owner failed
/// (E30 subai on rust, 2026-09-29).
#[tokio::test]
async fn a_row_holding_a_non_json_number_reads_back_and_the_store_survives() {
    let k = store();
    let inf = fantastic_kernel::coerce_scalar("3e500123");
    assert_eq!(inf.to_string(), "Infinity", "the token under test");
    // Assembled by hand: `json!` over this value is exactly the panic.
    let mut row = Map::new();
    row.insert("id".to_string(), Value::from(7));
    row.insert("job_id".to_string(), inf);
    let mut payload = Map::new();
    payload.insert("type".to_string(), Value::from("append"));
    payload.insert("rows".to_string(), Value::Array(vec![Value::Object(row)]));
    let out = call(&k, Value::Object(payload), Some("alice")).await;
    assert_eq!(out["appended"], json!(1), "{out}");
    let read = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(read["rows"][0]["job_id"].to_string(), "Infinity", "{read}");
    // the lock is not poisoned: another owner's call still answers
    let other = append(&k, "bob", json!([{"id": 1, "content": "after"}])).await;
    assert_eq!(other["appended"], json!(1), "{other}");
}

#[tokio::test]
async fn digit_string_refs_are_not_found_like_python() {
    // `_as_id('3')` passes the type check, but `t.has(owner, '3')` asks a str of
    // int-keyed tables: never found — NOT_FOUND naming the raw ref.
    let k = store();
    let first = append(&k, "alice", json!([{"kind": "msg", "content": "a"}])).await;
    let rid = first["ids"][0].as_i64().unwrap();
    let r = append(
        &k,
        "alice",
        json!([{"kind": "fold", "content": "f", "refs": [rid.to_string()]}]),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .ends_with(&format!("this owner: ['{rid}']")),
        "{r}"
    );
    // The int ref is found.
    let r = append(
        &k,
        "alice",
        json!([{"kind": "fold", "content": "f", "refs": [rid]}]),
    )
    .await;
    assert_eq!(r["appended"], 1, "{r}");
}

#[tokio::test]
async fn prune_keep_is_python_in() {
    // `row.get("kind") in keep`: a STRING keep is a substring test; a row whose
    // kind is not a str makes that `in` raise TypeError; an int keep is not
    // iterable.
    let k = store();
    append(
        &k,
        "alice",
        json!([{"kind": "fold", "content": "a"}, {"kind": "old", "content": "b"},
               {"kind": "msg", "content": "c"}]),
    )
    .await;
    let out = call(
        &k,
        json!({"type": "prune", "before_id": 1_000_000, "keep": "folder"}),
        Some("alice"),
    )
    .await;
    assert_eq!(
        out["pruned"], 1,
        "'fold' and 'old' are substrings of 'folder': {out}"
    );
    append(&k, "alice", json!([{"content": "kindless"}])).await;
    let out = with_sender(
        AgentId::from("alice"),
        k.send(vec![Item::new(
            AgentId::from("mem"),
            json!({"type": "prune", "before_id": 1_000_000, "keep": "fold"}),
        )]),
    )
    .await
    .remove(0);
    assert_eq!(out.status, fantastic_kernel::Status::Failed, "{out:?}");
    assert_eq!(
        out.result["error"],
        "TypeError: 'in <string>' requires string as left operand, not NoneType"
    );
    let out = with_sender(
        AgentId::from("alice"),
        k.send(vec![Item::new(
            AgentId::from("mem"),
            json!({"type": "prune", "before_id": 1_000_000, "keep": 5}),
        )]),
    )
    .await
    .remove(0);
    assert_eq!(
        out.result["error"],
        "TypeError: argument of type 'int' is not iterable"
    );
}

#[tokio::test]
async fn a_zero_limit_is_no_limit_like_python() {
    // `ids[:limit] if limit else ids` and `_as_id(limit) or 100`: the string
    // "0" passes `_as_id` as 0, which is falsy.
    let k = store();
    append(
        &k,
        "alice",
        json!([{"kind": "msg", "content": "x one"}, {"kind": "msg", "content": "x two"}]),
    )
    .await;
    let r = call(&k, json!({"type": "read", "limit": "0"}), Some("alice")).await;
    assert_eq!(ids(&r).len(), 2, "{r}");
    assert_eq!(r["next"], Value::Null);
    let r = call(
        &k,
        json!({"type": "search", "query": "x", "limit": "0", "width": "0"}),
        Some("alice"),
    )
    .await;
    assert_eq!(r["matches"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(
        r["matches"][0]["text"], "x one",
        "width 0 falls back to 200"
    );
}

// ─── STEP-4 PINS (2026-10-01): refusal wire shapes + the id parser ──────────
//
// `preserve_order` makes a refusal's KEY ORDER part of the wire, and `Value`
// equality ignores it — so these compare the serialized text.

fn wire(v: &Value) -> String {
    serde_json::to_string(v).unwrap()
}

#[tokio::test]
async fn unknown_type_is_coded_unhandled_with_the_raw_verb_in_order() {
    let k = store();
    for (verb, want) in [
        (
            "frob",
            r#"{"error":"ram_store: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#,
        ),
        (
            "it's",
            r#"{"error":"ram_store: unknown type \"it's\"","reason":"unhandled","verb":"it's"}"#,
        ),
        (
            "shutdown",
            r#"{"error":"ram_store: unknown type 'shutdown'","reason":"unhandled","verb":"shutdown"}"#,
        ),
    ] {
        assert_eq!(
            wire(&call(&k, json!({"type": verb}), Some("alice")).await),
            want
        );
    }
    // a typeless payload never reaches a bundle through `send`; the bundle's
    // own rendering of it is python's `None`, the verb the raw null
    let r = RamStoreBundle::default()
        .handle(&AgentId::from("mem"), &json!({}), &k)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        wire(&r),
        r#"{"error":"ram_store: unknown type None","reason":"unhandled","verb":null}"#
    );
}

#[tokio::test]
async fn arg_refusals_carry_error_reason_arg_in_that_order() {
    let k = store();
    append(&k, "alice", json!([{"id": 1, "content": "a"}])).await;
    for (payload, want) in [
        (
            json!({"type": "append", "rows": []}),
            r#"{"error":"append: `rows` must be a non-empty list of rows","reason":"bad_arg","arg":"rows"}"#,
        ),
        (
            json!({"type": "append", "rows": [{"id": "5"}]}),
            r#"{"error":"append: a row id is a kernel int, got '5'","reason":"bad_arg","arg":"rows"}"#,
        ),
        (
            json!({"type": "append", "rows": [{"id": 7, "refs": ["x"]}]}),
            r#"{"error":"append: row 7 refs must be row ids","reason":"bad_arg","arg":"rows"}"#,
        ),
        (
            json!({"type": "share", "with": "alice"}),
            r#"{"error":"share: you already read your own rows","reason":"bad_arg","arg":"with"}"#,
        ),
        (
            json!({"type": "share", "with": "bob", "mode": "weird"}),
            r#"{"error":"share: mode must be one of ('frozen', 'live')","reason":"bad_arg","arg":"mode"}"#,
        ),
        (
            json!({"type": "unshare", "grant": 999}),
            r#"{"error":"unshare: no grant 999","reason":"not_found","arg":"grant"}"#,
        ),
        (
            json!({"type": "grants", "side": "sideways"}),
            r#"{"error":"grants: side must be 'given' or 'received'","reason":"bad_arg","arg":"side"}"#,
        ),
    ] {
        let got = call(&k, payload.clone(), Some("alice")).await;
        assert_eq!(wire(&got), want, "{payload}");
    }
}

#[tokio::test]
async fn row_ids_and_limits_parse_as_python_ascii_digit_ids() {
    // python `_as_id`: an int > 0, or a STRIPPED ASCII-digit string — never a
    // Unicode digit, never a sign. `limit`/`width` fall back on a falsy id.
    let k = store();
    let rows: Vec<Value> = (1..=5)
        .map(|i| json!({"id": i, "content": format!("needle {i}")}))
        .collect();
    append(&k, "alice", Value::Array(rows)).await;
    let hits = |r: &Value| r["matches"].as_array().unwrap().len();
    for (limit, want) in [
        (json!(2), 2),
        (json!(" 2 "), 2),
        (json!("3"), 3),
        (json!("٢"), 5),  // a Unicode digit is not an id → the default 100
        (json!("+2"), 5), // a sign is not a digit → the default
        (json!(0), 5),    // falsy → the default
        (json!("0"), 5),  // `_as_id("0")` is 0 → `or 100`
        (json!(-1), 5),   // not > 0 → the default
        (json!(2.0), 5),  // a float is not an int
        (json!(true), 5), // a bool is not an id
    ] {
        let r = call(
            &k,
            json!({"type": "search", "query": "needle", "limit": limit.clone()}),
            Some("alice"),
        )
        .await;
        assert_eq!(hits(&r), want, "limit={limit}: {r}");
        assert_eq!(r["total"], json!(5), "limit={limit}");
        assert_eq!(r["truncated"], json!(want < 5), "limit={limit}");
    }
    // `width` takes the same parse
    let r = call(
        &k,
        json!({"type": "search", "query": "needle", "width": " 3 "}),
        Some("alice"),
    )
    .await;
    assert_eq!(r["matches"][0]["text"], json!("nee"));
}

#[tokio::test]
async fn two_stores_hold_separate_tables_and_delete_frees_one() {
    // per-agent state: a second store of the same kind starts empty, and its
    // own delete does not touch the first's rows.
    let k = store();
    k.register(agent(
        "mem2",
        Some(HANDLER_MODULE),
        Some("kernel_state"),
        json!({"operator": true, "admins": ["kernel"], "callable_by": {"alice": "*"}}),
    ));
    update(&k, "alice", json!({"can_call": {"mem": "*", "mem2": "*"}}));
    append(&k, "alice", json!([{"id": 1, "content": "in mem"}])).await;
    let read2 = with_sender(
        AgentId::from("alice"),
        k.send(vec![Item::new(
            AgentId::from("mem2"),
            json!({"type": "read"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(
        read2["rows"],
        json!([]),
        "mem2 shares nothing with mem: {read2}"
    );
    let gone = k
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "mem2"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(gone.get("error").is_none(), "{gone}");
    let read1 = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(ids(&read1), vec![1], "mem keeps its rows");
}
