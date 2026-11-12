//! ram_store: tenancy by caller, FK cascade, search, inject/dump — the
//! twin of python `ram_store/tests/test_ram_store.py`.
//!
//! Callers act through `as_agent` (the test standing in for an agent's handler —
//! a real agent send, gated both ways); the operator is a bare `kernel.send`. The root's guid is set by hand — the root is
//! not booted here, and the store keys every row by it.

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
            "callable_by": {"alice": "*", "bob": "*", "carol": "*", "leg": "*"},
        }),
    ));
    k
}

async fn call(k: &Arc<Kernel>, payload: Value, as_: Option<&str>) -> Value {
    let item = Item::new(AgentId::from("mem"), payload);
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
    // an `owner` on the call is no argument of the verb: refused by name, nothing written
    let out = call(
        &k,
        json!({"type": "append", "owner": "bob", "rows": [{"id": 5}]}),
        Some("alice"),
    )
    .await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
    assert_eq!(out["arg"], json!("owner"), "{out}");
    let alice = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(alice["rows"], json!([]));
    let read = call(&k, json!({"type": "read", "owner": "bob"}), Some("alice")).await;
    assert_eq!(read["reason"], json!("bad_arg"), "{read}");
    assert_eq!(read["arg"], json!("owner"), "{read}");
    // the row lands under the CALLER's key
    append(&k, "alice", json!([{"id": 5}])).await;
    let bob = call(&k, json!({"type": "read"}), Some("bob")).await;
    assert_eq!(bob["rows"], json!([]));
}

#[tokio::test]
async fn missing_id_takes_the_kernels_next() {
    let k = store();
    let before = k.rows.next_id();
    let out = append(&k, "alice", json!([{"content": "x"}])).await;
    assert!(out["ids"][0].as_i64().unwrap() >= before);
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
async fn prune_cascades_to_fold_refs() {
    let k = store();
    append(
        &k,
        "alice",
        json!([{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}]),
    )
    .await;
    let out = call(&k, json!({"type": "prune", "before_id": 2}), Some("alice")).await;
    assert_eq!(out, json!({"pruned": 1}));
    let fold = call(&k, json!({"type": "read", "ids": [3]}), Some("alice")).await;
    assert_eq!(fold["rows"][0]["refs"], json!([2]));
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
async fn a_name_match_reads_the_rows_carrying_it() {
    let k = store();
    append(
        &k,
        "alice",
        json!([
            {"id": 1, "kind": "call", "name": "job"},
            {"id": 2, "kind": "settle", "name": "job"},
            {"id": 3, "kind": "call", "name": "other"},
            {"id": 4, "kind": "call", "name": "job", "verb": "x"},
            {"id": 5, "kind": "clear"},
            {"id": 6, "kind": "call", "name": 7},
        ]),
    )
    .await;
    append(&k, "bob", json!([{"id": 1, "kind": "call", "name": "job"}])).await;
    let read = |q: Value| {
        let k = &k;
        async move {
            let mut q = q;
            q["type"] = json!("read");
            ids(&call(k, q, Some("alice")).await)
        }
    };
    assert_eq!(read(json!({"match": {"name": "job"}})).await, vec![1, 2, 4]);
    assert_eq!(
        read(json!({"kinds": ["call"], "match": {"name": "job"}})).await,
        vec![1, 4]
    );
    assert_eq!(
        read(json!({"match": {"name": "job", "verb": "x"}})).await,
        vec![4]
    );
    assert_eq!(
        read(json!({"match": {"name": "job"}, "order": "desc", "limit": 1})).await,
        vec![4]
    );
    assert_eq!(
        read(json!({"match": {"name": "job"}, "from_last_kind": "clear"})).await,
        Vec::<i64>::new()
    );
    assert_eq!(
        read(json!({"match": {"name": "nobody"}})).await,
        Vec::<i64>::new()
    );
    assert_eq!(read(json!({"match": {"name": 7}})).await, vec![6]);
    call(&k, json!({"type": "prune", "before_id": 3}), Some("alice")).await;
    assert_eq!(read(json!({"match": {"name": "job"}})).await, vec![4]);
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
    // a plain verb — the gate decides who reaches it
    let by_alice = call(&k, json!({"type": "dump"}), Some("alice")).await;
    assert!(by_alice["tables"].is_object(), "{by_alice}");
    let tables = call(&k, json!({"type": "dump"}), None).await["tables"].clone();
    call(&k, json!({"type": "prune", "before_id": 99}), Some("alice")).await;
    let empty = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(empty["rows"], json!([]));
    let out = call(&k, json!({"type": "inject", "tables": tables}), None).await;
    assert_eq!(out, json!({"rows": 3}));
    assert_eq!(
        call(&k, json!({"type": "dump"}), None).await["tables"],
        tables
    );
    // a broken FK is refused and nothing changes
    let mut bad = tables.clone();
    bad["fold_refs"][0]["members"] = json!([999]);
    let refused = call(&k, json!({"type": "inject", "tables": bad}), None).await;
    assert_eq!(refused["reason"], json!("bad_arg"));
    assert_eq!(
        call(&k, json!({"type": "dump"}), None).await["tables"],
        tables
    );
}

#[tokio::test]
async fn reflect_names_the_posture_and_the_verbs() {
    let k = store();
    append(&k, "alice", json!([{"id": 1}])).await;
    let r = call(&k, json!({"type": "reflect"}), None).await;
    assert_eq!(r["durable"], json!(false));
    assert_eq!(r["owners"], json!(1));
    assert_eq!(r["rows"], json!(1));
    assert_eq!(r["optional_verbs"], json!(["search"]));
    // `reflect` renders the declared verbs, builtin and common, in declaration order
    let names: Vec<&str> = r["verbs"]
        .as_object()
        .expect("reflect carries the verbs block")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        names,
        vec![
            "append",
            "read",
            "search",
            "prune",
            "inject",
            "dump",
            "put_bytes",
            "get_bytes",
            "reflect",
            "boot",
            "shutdown"
        ]
    );
}

/// A number whose text is not JSON (`Infinity`) cannot be typed into the declared
/// `rows`: the entry refuses the call by name, and the store — never reached,
/// never poisoned — answers the next call of every owner.
#[tokio::test]
async fn a_row_holding_a_non_json_number_is_refused_and_the_store_survives() {
    let k = store();
    let inf = fantastic_kernel::test_support::non_finite("Infinity");
    assert_eq!(inf.to_string(), "Infinity", "the token under test");
    // Assembled by hand: `json!` over this value is exactly the panic.
    let mut row = Map::new();
    row.insert("id".to_string(), Value::from(7));
    row.insert("job_id".to_string(), inf);
    let mut payload = Map::new();
    payload.insert("type".to_string(), Value::from("append"));
    payload.insert("rows".to_string(), Value::Array(vec![Value::Object(row)]));
    let out = call(&k, Value::Object(payload), Some("alice")).await;
    assert_eq!(out["reason"], json!("bad_arg"), "{out}");
    let read = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(read["rows"], json!([]), "{read}");
    // the lock is not poisoned: another owner's call still answers
    let other = append(&k, "bob", json!([{"id": 1, "content": "after"}])).await;
    assert_eq!(other["appended"], json!(1), "{other}");
}

#[tokio::test]
async fn digit_string_refs_are_refused_as_not_row_ids() {
    let k = store();
    let first = append(&k, "alice", json!([{"kind": "msg", "content": "a"}])).await;
    let rid = first["ids"][0].as_i64().unwrap();
    let r = append(
        &k,
        "alice",
        json!([{"kind": "fold", "content": "f", "refs": [rid.to_string()]}]),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(
        r["error"],
        format!("append: the arguments do not parse (invalid type: string \"{rid}\", expected i64)"),
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
async fn prune_keep_is_a_list_of_kinds() {
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
        json!({"type": "prune", "before_id": 1_000_000, "keep": ["fold", "old"]}),
        Some("alice"),
    )
    .await;
    assert_eq!(out["pruned"], 1, "only `msg` is not kept: {out}");
    // a kind-less row is not in `keep`
    append(&k, "alice", json!([{"content": "kindless"}])).await;
    let out = call(
        &k,
        json!({"type": "prune", "before_id": 1_000_000, "keep": ["fold"]}),
        Some("alice"),
    )
    .await;
    assert_eq!(out["pruned"], 2, "`old` and the kind-less row go: {out}");
    // a `keep` that is not a list is refused by name, nothing deleted
    for keep in [json!("fold"), json!(5)] {
        let out = call(
            &k,
            json!({"type": "prune", "before_id": 1_000_000, "keep": keep}),
            Some("alice"),
        )
        .await;
        assert_eq!(out["reason"], json!("bad_arg"), "{out}");
        assert_eq!(out["arg"], json!("keep"), "{out}");
    }
    let left = call(&k, json!({"type": "read"}), Some("alice")).await;
    assert_eq!(ids(&left).len(), 1, "{left}");
}

#[tokio::test]
async fn a_zero_limit_is_no_limit() {
    // `read` with limit 0 is no limit; `search` falls back to its defaults
    let k = store();
    append(
        &k,
        "alice",
        json!([{"kind": "msg", "content": "x one"}, {"kind": "msg", "content": "x two"}]),
    )
    .await;
    let r = call(&k, json!({"type": "read", "limit": 0}), Some("alice")).await;
    assert_eq!(ids(&r).len(), 2, "{r}");
    assert_eq!(r["next"], Value::Null);
    let r = call(
        &k,
        json!({"type": "search", "query": "x", "limit": 0, "width": 0}),
        Some("alice"),
    )
    .await;
    assert_eq!(r["matches"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(
        r["matches"][0]["text"], "x one",
        "width 0 falls back to 200"
    );
}

// ─── refusal wire shapes + the id parser ────────────────────────────────────
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
            json!("frob"),
            r#"{"error":"unknown verb 'frob'; mem answers: append, read, search, prune, inject, dump, put_bytes, get_bytes, reflect, boot, shutdown","reason":"unhandled","hint":"docs of=mem lists each verb's arguments","verb":"frob"}"#,
        ),
        (
            json!("it's"),
            r#"{"error":"unknown verb \"it's\"; mem answers: append, read, search, prune, inject, dump, put_bytes, get_bytes, reflect, boot, shutdown","reason":"unhandled","hint":"docs of=mem lists each verb's arguments","verb":"it's"}"#,
        ),
    ] {
        assert_eq!(
            wire(&call(&k, json!({"type": verb}), Some("alice")).await),
            want
        );
    }
    // a verb the store declares neither of: the shared entry answers `shutdown` itself
    assert_eq!(
        call(&k, json!({"type": "shutdown"}), Some("alice")).await,
        Value::Null
    );
}

#[tokio::test]
async fn arg_refusals_carry_error_reason_arg_in_that_order() {
    let k = store();
    append(&k, "alice", json!([{"id": 1, "content": "a"}])).await;
    // a row id that is not a kernel int and refs that are not row ids are the engine's refusals
    for (payload, why) in [
        (json!({"type": "append", "rows": [{"id": "5"}]}), "invalid type: string \"5\", expected a nonzero u64"),
        (json!({"type": "append", "rows": [{"id": 0}]}), "invalid value: integer `0`, expected a nonzero u64"),
        (json!({"type": "append", "rows": [{"id": 7, "refs": ["x"]}]}), "invalid type: string \"x\", expected i64"),
    ] {
        let got = call(&k, payload.clone(), Some("alice")).await;
        assert_eq!(got["reason"], "bad_arg", "{payload}: {got}");
        assert_eq!(got["error"], format!("append: the arguments do not parse ({why})"), "{payload}: {got}");
    }
    // an empty list, a missing list and a wrongly typed one are the entry's refusals
    for (payload, reason) in [
        (json!({"type": "append", "rows": []}), "missing_arg"),
        (json!({"type": "append"}), "missing_arg"),
        (json!({"type": "append", "rows": "x"}), "bad_arg"),
        (json!({"type": "append", "rows": [1]}), "bad_arg"),
    ] {
        let got = call(&k, payload.clone(), Some("alice")).await;
        assert_eq!(got["reason"], json!(reason), "{payload}: {got}");
        assert_eq!(got["arg"], json!("rows"), "{payload}: {got}");
    }
}

#[tokio::test]
async fn limits_and_ids_are_typed_integers() {
    // `limit`/`width` are integers: 0 falls back to the default; text, a float, a
    // bool and a negative number are refused by name.
    let k = store();
    let rows: Vec<Value> = (1..=5)
        .map(|i| json!({"id": i, "content": format!("needle {i}")}))
        .collect();
    append(&k, "alice", Value::Array(rows)).await;
    let hits = |r: &Value| r["matches"].as_array().unwrap().len();
    for (limit, want) in [(json!(2), 2), (json!(3), 3), (json!(0), 5)] {
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
    for limit in [json!("3"), json!(" 2 "), json!(-1), json!(2.0), json!(true)] {
        let r = call(
            &k,
            json!({"type": "search", "query": "needle", "limit": limit.clone()}),
            Some("alice"),
        )
        .await;
        assert_eq!(r["reason"], json!("bad_arg"), "limit={limit}: {r}");
        assert_eq!(r["arg"], json!("limit"), "limit={limit}: {r}");
    }
    let r = call(
        &k,
        json!({"type": "search", "query": "needle", "width": 3}),
        Some("alice"),
    )
    .await;
    assert_eq!(r["matches"][0]["text"], json!("nee"));
    // a row-id list takes integers only
    let r = call(&k, json!({"type": "read", "ids": ["1"]}), Some("alice")).await;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    assert_eq!(r["arg"], json!("ids"), "{r}");
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
        json!({"callable_by": {"alice": "*"}}),
    ));
    update(&k, "alice", json!({"can_call": {"mem": "*", "mem2": "*"}}));
    append(&k, "alice", json!([{"id": 1, "content": "in mem"}])).await;
    let read2 = as_agent(
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

#[tokio::test]
async fn append_emits_one_appended_event_per_new_row() {
    let k = store();
    let state: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let tap = state.clone();
    k.add_state_subscriber(std::sync::Arc::new(move |ev: &Value, _body| {
        tap.lock().unwrap().push(ev.clone());
    }));
    let mut rx = k.ensure_inbox(&AgentId::from("mem"));
    let out = append(&k, "alice", json!([{"content": "a"}, {"content": "b"}])).await;
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
    let refused = append(&k, "alice", json!([])).await;
    assert_eq!(refused["reason"], json!("missing_arg"), "{refused}");
    while let Some(item) = rx.try_recv() {
        assert_ne!(item.payload["type"], json!("appended"), "{refused}");
    }
}

// ─── put_bytes / get_bytes (#622) ───────────────────────────────────

/// A call carrying `bytes` as the request body at `data`: the reply and its body.
async fn call_bytes(
    k: &Arc<Kernel>,
    payload: Value,
    bytes: Option<&[u8]>,
    as_: &str,
) -> (Value, Option<Vec<u8>>) {
    let mut item = Item::new(AgentId::from("mem"), payload);
    if let Some(b) = bytes {
        item = item.with_binary_key("data", b.to_vec());
    }
    let out = as_agent(AgentId::from(as_), k.send(vec![item]))
        .await
        .remove(0);
    let body = out.body_bytes().map(<[u8]>::to_vec);
    (out.result, body)
}

#[tokio::test]
async fn bytes_round_trip_under_the_callers_key() {
    let k = store();
    let raw: Vec<u8> = vec![0, 255, 7, 0, 128, 10, 13];
    let (put, none) = call_bytes(
        &k,
        json!({"type": "put_bytes", "ref": "img-1", "media_type": "image/png"}),
        Some(&raw),
        "alice",
    )
    .await;
    assert_eq!(
        put,
        json!({"ref": "img-1", "media_type": "image/png", "bytes": 7})
    );
    assert!(none.is_none());
    let (got, body) = call_bytes(
        &k,
        json!({"type": "get_bytes", "ref": "img-1"}),
        None,
        "alice",
    )
    .await;
    assert_eq!(
        got,
        json!({"ref": "img-1", "media_type": "image/png", "bytes": 7})
    );
    assert_eq!(body, Some(raw.clone()));
    // another caller has its own space: the same ref is not there
    let (other, body) = call_bytes(
        &k,
        json!({"type": "get_bytes", "ref": "img-1"}),
        None,
        "bob",
    )
    .await;
    assert_eq!(other["reason"], json!("not_found"), "{other}");
    assert!(body.is_none());
    // a second put replaces; an empty body is a legal blob
    call_bytes(
        &k,
        json!({"type": "put_bytes", "ref": "img-1", "media_type": "image/jpeg"}),
        Some(&[]),
        "alice",
    )
    .await;
    let (got, body) = call_bytes(
        &k,
        json!({"type": "get_bytes", "ref": "img-1"}),
        None,
        "alice",
    )
    .await;
    assert_eq!(got["media_type"], json!("image/jpeg"));
    assert_eq!(body, Some(Vec::new()));
}

#[tokio::test]
async fn bytes_verbs_refuse_a_missing_ref_or_body() {
    let k = store();
    let (r, _) = call_bytes(
        &k,
        json!({"type": "get_bytes", "ref": "nope"}),
        None,
        "alice",
    )
    .await;
    assert_eq!(r["reason"], json!("not_found"), "{r}");
    let (r, _) = call_bytes(&k, json!({"type": "get_bytes"}), None, "alice").await;
    assert_eq!(r["reason"], json!("missing_arg"), "{r}");
    let (r, _) = call_bytes(
        &k,
        json!({"type": "put_bytes", "media_type": "image/png"}),
        Some(&[1]),
        "alice",
    )
    .await;
    assert_eq!(r["arg"], json!("ref"), "{r}");
    let (r, _) = call_bytes(
        &k,
        json!({"type": "put_bytes", "ref": "a"}),
        Some(&[1]),
        "alice",
    )
    .await;
    assert_eq!(r["arg"], json!("media_type"), "{r}");
    let (r, _) = call_bytes(
        &k,
        json!({"type": "put_bytes", "ref": "a", "media_type": "image/png"}),
        None,
        "alice",
    )
    .await;
    assert_eq!(r["reason"], json!("missing_arg"), "{r}");
    let (r, _) = call_bytes(&k, json!({"type": "get_bytes", "ref": "a"}), None, "alice").await;
    assert_eq!(r["reason"], json!("not_found"), "nothing was stored: {r}");
}

#[tokio::test]
async fn stored_bytes_never_show_in_rows_or_the_dump() {
    let k = store();
    append(&k, "alice", json!([{"id": 1, "content": "hi"}])).await;
    let rows = call(&k, json!({"type": "read"}), Some("alice")).await;
    let dump = call(&k, json!({"type": "dump"}), None).await;
    let hits = call(&k, json!({"type": "search", "query": "x"}), Some("alice")).await;
    call_bytes(
        &k,
        json!({"type": "put_bytes", "ref": "1", "media_type": "image/png"}),
        Some(b"xxxx"),
        "alice",
    )
    .await;
    assert_eq!(call(&k, json!({"type": "read"}), Some("alice")).await, rows);
    assert_eq!(call(&k, json!({"type": "dump"}), None).await, dump);
    assert_eq!(
        call(&k, json!({"type": "search", "query": "x"}), Some("alice")).await,
        hits
    );
    let r = call(&k, json!({"type": "reflect"}), None).await;
    assert_eq!(r["rows"], json!(1));
}
