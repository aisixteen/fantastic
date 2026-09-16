//! The `messages` verb — conversation rows by id, gated per row against the
//! OWNER. Miniature of python's `e2e/tests/coding/E77_messages_gate`.
//!
//! Why it is gated INSIDE: `messages` is a root verb, and a root verb skips the
//! ingress check. Without re-running that check here, any agent could read any
//! other agent's conversation heads by asking the root for them — a side channel
//! around `callable_by` that no grant ever opened.

use fantastic_kernel::{Agent, AgentId, BundleRegistry, Item, Kernel, StorageMode};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Every key a digest is allowed to carry. `content` is NOT one of them.
const DIGEST_KEYS: [&str; 12] = [
    "id", "agent", "parent", "kind", "role", "head", "bytes", "title", "level", "refs", "verb",
    "name",
];

fn kernel() -> Arc<Kernel> {
    let mut k = Kernel::with_storage(StorageMode::InMemory);
    k.bundles = BundleRegistry::new();
    let k = Arc::new(k);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        std::env::temp_dir(),
        false,
    );
    k.register(Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    k
}

/// A bare agent with a `callable_by` of its own.
fn agent(k: &Arc<Kernel>, id: &str, callable_by: Value) {
    let mut meta = Map::new();
    meta.insert("callable_by".to_string(), callable_by);
    let a = Agent::new(
        AgentId::from(id),
        None,
        Some(AgentId::from("core")),
        meta,
        std::env::temp_dir(),
        false,
    );
    k.register(Arc::clone(&a));
}

async fn ask(k: &Arc<Kernel>, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result
}

fn plant(k: &Arc<Kernel>, owner: &str, content: &str) -> i64 {
    k.rows
        .put(
            &AgentId::from(owner),
            json!({"kind": "turn", "role": "user", "content": content})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap()
}

#[tokio::test]
async fn the_operator_sees_every_row_at_digest_resolution() {
    let k = kernel();
    let secret = "the passphrase is PRJ-1BCA4F, and here is a great deal more text";
    let id = plant(&k, "ai", secret);
    let out = ask(&k, json!({"type": "messages", "ids": [id]})).await;
    let rows = out["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert!(out["refused"].as_array().unwrap().is_empty());
    // A DIGEST, NOT A BODY: the head is capped and `content` never appears.
    let row = rows[0].as_object().unwrap();
    assert_eq!(row["bytes"], secret.len());
    assert!(row["head"].as_str().unwrap().chars().count() <= 120);
    assert!(!row.contains_key("content"), "a body leaked: {row:?}");
    for key in row.keys() {
        assert!(DIGEST_KEYS.contains(&key.as_str()), "unknown key {key}");
    }
}

#[tokio::test]
async fn an_agent_reads_its_own_rows_and_is_refused_anothers() {
    let k = kernel();
    agent(&k, "peer", json!({}));
    agent(&k, "ai", json!({}));
    let mine = plant(&k, "peer", "my own turn");
    let theirs = plant(&k, "ai", "somebody else's turn");

    let out = fantastic_kernel::send::with_sender(AgentId::from("peer"), async {
        ask(&k, json!({"type": "messages", "ids": [mine, theirs]})).await
    })
    .await;
    let ids: Vec<i64> = out["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![mine], "an agent read a row it was never granted");
    // LISTED, never silently dropped: "not yours" must be tellable from
    // "not there".
    assert_eq!(out["refused"], json!([theirs]));
}

#[tokio::test]
async fn a_grant_for_messages_opens_exactly_that() {
    let k = kernel();
    agent(&k, "peer", json!({}));
    agent(&k, "ai", json!({"peer": ["messages"]}));
    let theirs = plant(&k, "ai", "now readable");

    let out = fantastic_kernel::send::with_sender(AgentId::from("peer"), async {
        ask(&k, json!({"type": "messages", "ids": [theirs]})).await
    })
    .await;
    assert_eq!(out["rows"].as_array().unwrap().len(), 1);
    assert!(out["refused"].as_array().unwrap().is_empty());

    // …and a grant for something ELSE does not open it.
    let k2 = kernel();
    agent(&k2, "peer", json!({}));
    agent(&k2, "ai", json!({"peer": ["reflect"]}));
    let other = plant(&k2, "ai", "still private");
    let out = fantastic_kernel::send::with_sender(AgentId::from("peer"), async {
        ask(&k2, json!({"type": "messages", "ids": [other]})).await
    })
    .await;
    assert!(out["rows"].as_array().unwrap().is_empty());
    assert_eq!(out["refused"], json!([other]));
}

#[tokio::test]
async fn an_unknown_id_is_refused_and_bad_args_are_named() {
    let k = kernel();
    let out = ask(&k, json!({"type": "messages", "ids": [404]})).await;
    assert!(out["rows"].as_array().unwrap().is_empty());
    assert_eq!(out["refused"], json!([404]));

    for bad in [
        json!({"type": "messages"}),
        json!({"type": "messages", "ids": []}),
    ] {
        let out = ask(&k, bad).await;
        assert_eq!(out["reason"], "bad_arg", "{out}");
        assert_eq!(out["arg"], "ids");
    }
}

#[tokio::test]
async fn the_digest_pages_by_limit_and_after() {
    let k = kernel();
    let ids: Vec<i64> = (0..5)
        .map(|i| plant(&k, "ai", &format!("turn {i}")))
        .collect();
    let page = ask(&k, json!({"type": "messages", "ids": ids, "limit": 2})).await;
    assert_eq!(page["rows"].as_array().unwrap().len(), 2);
    let next = page["next"].as_i64().expect("a cursor");
    let rest = ask(&k, json!({"type": "messages", "ids": ids, "after": next})).await;
    let first = rest["rows"].as_array().unwrap()[0]["id"].as_i64().unwrap();
    assert!(first > next, "the second page must resume past the cursor");
}

#[tokio::test]
async fn a_view_row_says_how_much_it_holds_never_the_array() {
    let k = kernel();
    let id = k
        .rows
        .view_put(
            &AgentId::from("ai"),
            json!({"budget": 12000, "why": "after the fold", "ids": [1, 2, 3]})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
    let out = ask(&k, json!({"type": "messages", "ids": [id]})).await;
    let row = &out["rows"].as_array().unwrap()[0];
    assert_eq!(
        row["ids"],
        json!(3),
        "the array itself must not be handed out"
    );
    assert_eq!(row["budget"], json!(12000));
    assert_eq!(row["title"], json!("after the fold"));
}

// ── rows_read / rows_write: hydration's pair, operator-only ────────────────

#[tokio::test]
async fn rows_read_returns_whole_rows_oldest_first_and_pages() {
    let k = kernel();
    let ids: Vec<i64> = (0..4)
        .map(|i| plant(&k, "ai", &format!("turn {i}")))
        .collect();
    let got = ask(&k, json!({"type": "rows_read", "agent": "ai"})).await;
    let rows = got["rows"].as_array().expect("rows");
    assert_eq!(
        rows.iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        ids
    );
    // WHOLE, not a digest: `messages` hides bodies on purpose, this one does not.
    assert_eq!(rows[0]["content"], "turn 0");
    assert!(got["next"].is_null());

    let page = ask(&k, json!({"type": "rows_read", "agent": "ai", "limit": 2})).await;
    assert_eq!(page["rows"].as_array().unwrap().len(), 2);
    assert_eq!(page["next"], json!(ids[1]));
    let rest = ask(
        &k,
        json!({"type": "rows_read", "agent": "ai", "after": page["next"]}),
    )
    .await;
    assert_eq!(
        rest["rows"].as_array().unwrap()[0]["id"].as_i64().unwrap(),
        ids[2]
    );
    // An agent with no conversation is empty, not an error.
    let none = ask(&k, json!({"type": "rows_read", "agent": "nobody"})).await;
    assert!(none["rows"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn rows_write_appends_under_fresh_ids_and_leaves_the_source_alone() {
    let k = kernel();
    let src = plant(&k, "ai", "the original");
    let whole = ask(&k, json!({"type": "rows_read", "agent": "ai"})).await;
    // A COPY strips what the store owns; the caller remaps its own references.
    let rows: Vec<Value> = whole["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let mut m = r.as_object().unwrap().clone();
            for own in ["id", "agent", "ts"] {
                m.remove(own);
            }
            Value::Object(m)
        })
        .collect();
    let out = ask(
        &k,
        json!({"type": "rows_write", "agent": "ai_fork", "rows": rows}),
    )
    .await;
    let new_ids = out["ids"].as_array().expect("ids");
    assert_eq!(new_ids.len(), 1);
    assert_ne!(new_ids[0].as_i64().unwrap(), src, "a copy is a NEW row");

    let copied = k.rows.for_agent(&AgentId::from("ai_fork"));
    assert_eq!(copied[0]["content"], "the original");
    assert_eq!(copied[0]["agent"], "ai_fork");
    // A fork READS; it does not move anything.
    assert_eq!(k.rows.for_agent(&AgentId::from("ai")).len(), 1);
}

#[tokio::test]
async fn rows_write_refuses_a_row_that_carries_its_own_id() {
    let k = kernel();
    let out = ask(
        &k,
        json!({"type": "rows_write", "agent": "ai", "rows": [{"id": 7, "content": "x"}]}),
    )
    .await;
    assert_eq!(out["reason"], "bad_arg");
    assert_eq!(out["arg"], "rows");
    assert!(out["error"]
        .as_str()
        .unwrap()
        .contains("may not carry an `id`"));
}

#[tokio::test]
async fn the_pairs_bad_args_are_named() {
    let k = kernel();
    for (payload, arg) in [
        (json!({"type": "rows_read"}), "agent"),
        (json!({"type": "rows_write", "agent": "ai"}), "rows"),
        (
            json!({"type": "rows_write", "agent": "ai", "rows": []}),
            "rows",
        ),
        (
            json!({"type": "rows_write", "agent": "ai", "rows": ["nope"]}),
            "rows",
        ),
    ] {
        let out = ask(&k, payload.clone()).await;
        assert_eq!(out["reason"], "bad_arg", "{payload} -> {out}");
        assert_eq!(out["arg"], arg, "{payload} -> {out}");
    }
}

/// Reading a conversation WHOLE, and writing one into an agent, is what a LOADER
/// does from above. `messages` is the grantable digest read; these two are
/// PRIVILEGED, so a `*` never carries them — saying yes takes naming them.
#[tokio::test]
async fn a_wildcard_never_carries_the_pair_but_a_named_grant_does() {
    let k = kernel();
    plant(&k, "ai", "private");
    // Wide open BOTH ways, and it still refuses: `*` is not a yes to these.
    grant_root(&k, json!({"peer": "*"}));
    peer(&k, json!({"core": "*"}));
    for payload in [
        json!({"type": "rows_read", "agent": "ai"}),
        json!({"type": "rows_write", "agent": "ai", "rows": [{"content": "x"}]}),
    ] {
        let out = fantastic_kernel::send::with_sender(AgentId::from("peer"), async {
            ask(&k, payload.clone()).await
        })
        .await;
        assert_eq!(out["reason"], "unauthorized", "{payload} -> {out}");
    }
    assert_eq!(
        k.rows.for_agent(&AgentId::from("ai")).len(),
        1,
        "nothing was written"
    );

    // NAMED, and it opens — on both sides of the gate.
    grant_root(&k, json!({"peer": ["rows_read"]}));
    peer(&k, json!({"core": ["rows_read"]}));
    let out = fantastic_kernel::send::with_sender(AgentId::from("peer"), async {
        ask(&k, json!({"type": "rows_read", "agent": "ai"})).await
    })
    .await;
    assert_eq!(out["rows"].as_array().unwrap()[0]["content"], "private");
}

/// Set the ROOT's ingress — the target side of the gate.
fn grant_root(k: &Arc<Kernel>, callable_by: Value) {
    let root = k.root().expect("a root");
    root.meta
        .write()
        .expect("meta lock poisoned")
        .insert("callable_by".to_string(), callable_by);
}

/// Create (or re-grant) `peer` with an egress map — the caller side.
fn peer(k: &Arc<Kernel>, can_call: Value) {
    if let Some(existing) = k.agents.get(&AgentId::from("peer")) {
        existing
            .meta
            .write()
            .expect("meta lock poisoned")
            .insert("can_call".to_string(), can_call);
        return;
    }
    let mut meta = Map::new();
    meta.insert("can_call".to_string(), can_call);
    let a = Agent::new(
        AgentId::from("peer"),
        None,
        Some(AgentId::from("core")),
        meta,
        std::env::temp_dir(),
        false,
    );
    k.register(Arc::clone(&a));
}

// ── C63–C69: the digest IS python's, value for value ───────────────────

/// Plant an arbitrary row and return its id.
fn plant_row(k: &Arc<Kernel>, owner: &str, row: Value) -> i64 {
    k.rows
        .put(&AgentId::from(owner), row.as_object().unwrap().clone())
        .unwrap()
}

async fn digest_of(k: &Arc<Kernel>, id: i64) -> Value {
    let out = ask(k, json!({"type": "messages", "ids": [id]})).await;
    out["rows"]
        .as_array()
        .expect("rows")
        .first()
        .cloned()
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn digest_head_and_bytes_count_code_points() {
    // C63. `bytes` used to be `content.len()` — UTF-8 bytes — so a cyrillic or
    // CJK turn read as two or three times its length, and `head` cut on chars
    // while `bytes` counted bytes.
    let k = kernel();
    let jp = plant_row(
        &k,
        "ai",
        json!({"kind": "turn", "content": "日本語".repeat(4) + "x"}),
    );
    assert_eq!(digest_of(&k, jp).await["bytes"], json!(13));

    let crlf = plant_row(&k, "ai", json!({"kind": "turn", "content": "a\r\nb"}));
    let d = digest_of(&k, crlf).await;
    assert_eq!(d["bytes"], json!(4), "\\r survives, \\n becomes a space");
    assert_eq!(d["head"], json!("a\r b"));

    // 120 CODE POINTS, sliced BEFORE the newline replacement.
    let long = plant_row(
        &k,
        "ai",
        json!({"kind": "turn", "content": "\r\n".repeat(100)}),
    );
    let d = digest_of(&k, long).await;
    assert_eq!(d["head"].as_str().unwrap().chars().count(), 120);
    assert_eq!(d["bytes"], json!(200));
}

#[tokio::test]
async fn non_string_content_digests_like_python_str() {
    // C64: `str(row.get("content", ""))`. Rust used to read `as_str` and fall to
    // "" for everything else, so every non-string body digested as empty.
    let k = kernel();
    let cases: Vec<(Value, &str, usize)> = vec![
        (json!(5), "5", 1),
        (Value::Null, "None", 4),
        (json!(["a", 1, true]), "['a', 1, True]", 14),
        (json!(1.5), "1.5", 3),
        (json!({"k": "v", "n": null}), "{'k': 'v', 'n': None}", 21),
    ];
    for (content, head, bytes) in cases {
        let id = plant_row(
            &k,
            "ai",
            json!({"kind": "turn", "content": content.clone()}),
        );
        let d = digest_of(&k, id).await;
        assert_eq!(d["head"], json!(head), "content {content}");
        assert_eq!(d["bytes"], json!(bytes), "content {content}");
    }
    // An ABSENT key is "" — not "None".
    let id = plant_row(&k, "ai", json!({"kind": "turn"}));
    let d = digest_of(&k, id).await;
    assert_eq!(d["head"], json!(""));
    assert_eq!(d["bytes"], json!(0));
}

#[tokio::test]
async fn digest_optional_keys_follow_python_truthiness() {
    // C65. `if row.get(k):` — an empty string, a 0 and an empty list are all
    // absent, and what is inserted is the RAW value, whatever its type.
    let k = kernel();
    let falsy = plant_row(
        &k,
        "ai",
        json!({"kind": "", "role": "user", "title": "", "level": 0, "refs": [], "content": "x"}),
    );
    let d = digest_of(&k, falsy).await;
    let m = d.as_object().unwrap();
    assert_eq!(m["kind"], json!("user"), "a falsy kind falls back to role");
    assert!(!m.contains_key("title"), "an empty title is not a title");
    assert!(!m.contains_key("level"), "level 0 is not a level");
    assert!(!m.contains_key("refs"), "an empty refs is not refs");

    // Truthy values of ANY type ride raw; `refs` becomes a LENGTH by python's
    // `len()` — items, code points, keys.
    let truthy = plant_row(
        &k,
        "ai",
        json!({"kind": "fold", "title": 7, "level": [1], "refs": "日本語", "content": "x"}),
    );
    let d = digest_of(&k, truthy).await;
    assert_eq!(d["title"], json!(7));
    assert_eq!(d["level"], json!([1]));
    assert_eq!(d["refs"], json!(3), "a string refs counts code points");

    assert_eq!(
        d.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["id", "agent", "parent", "kind", "role", "head", "bytes", "title", "level", "refs"],
        "python's key order"
    );

    // A VIEW: `ids` is a count, `title` is `why` and is assigned LAST — it
    // overwrites the row's own title while keeping that key's position.
    let view = plant_row(
        &k,
        "ai",
        json!({"kind": "view", "title": "kept", "ids": [1, 2, 3], "budget": 900, "why": "fit"}),
    );
    let d = digest_of(&k, view).await;
    assert_eq!(d["ids"], json!(3));
    assert_eq!(d["budget"], json!(900));
    assert_eq!(d["title"], json!("fit"));
    assert_eq!(
        d.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["id", "agent", "parent", "kind", "role", "head", "bytes", "title", "ids", "budget"],
        "`title` keeps the position it already had"
    );
    // A view with no `why` and no ids: `title` lands last, as null.
    let bare = plant_row(&k, "ai", json!({"kind": "view", "budget": 1}));
    let d = digest_of(&k, bare).await;
    assert_eq!(d["ids"], json!(0));
    assert_eq!(d["title"], Value::Null);
    assert_eq!(
        d.as_object()
            .unwrap()
            .keys()
            .next_back()
            .map(String::as_str),
        Some("title")
    );
}

#[tokio::test]
async fn call_row_digest_status_comes_from_outcome() {
    // C66. A top-level `status` is not the call's status — the settle stamps a
    // nested `outcome`, and that is the only place the answer lives.
    let k = kernel();
    let id = plant_row(
        &k,
        "ai",
        json!({"kind": "call", "verb": "work", "status": "completed",
               "outcome": {"status": "failed"}}),
    );
    assert_eq!(digest_of(&k, id).await["status"], json!("failed"));

    // An outcome WITHOUT a status is a null status, not an absent key.
    let id = plant_row(
        &k,
        "ai",
        json!({"kind": "call", "verb": "work", "outcome": {"x": 1}}),
    );
    let d = digest_of(&k, id).await;
    assert!(d.as_object().unwrap().contains_key("status"));
    assert_eq!(d["status"], Value::Null);

    // An EMPTY outcome is falsy — python inserts nothing at all.
    let id = plant_row(
        &k,
        "ai",
        json!({"kind": "call", "verb": "work", "outcome": {}}),
    );
    let d = digest_of(&k, id).await;
    assert!(!d.as_object().unwrap().contains_key("status"), "{d}");
    // A task row reads the same way, and a falsy `name` is no name.
    let id = plant_row(
        &k,
        "ai",
        json!({"kind": "task", "verb": "job", "name": "", "outcome": {"status": "cancelled"}}),
    );
    let d = digest_of(&k, id).await;
    assert!(!d.as_object().unwrap().contains_key("name"));
    assert_eq!(d["status"], json!("cancelled"));
}

#[tokio::test]
async fn limit_coerces_like_python_int() {
    // C67: `int(x or 0)`, TypeError/ValueError swallowed to 0.
    let k = kernel();
    let ids: Vec<i64> = (0..5)
        .map(|i| plant(&k, "ai", &format!("row {i}")))
        .collect();
    let all = json!(ids);

    let two = ask(&k, json!({"type": "messages", "ids": all, "limit": "2"})).await;
    assert_eq!(
        two["rows"].as_array().unwrap().len(),
        2,
        "a digit STRING is a limit"
    );
    assert_eq!(two["next"], json!(ids[1]));

    for spelling in [
        json!(" 2"),
        json!("+2"),
        json!("٢"),
        json!(2.5),
        json!(true),
    ] {
        let n = if spelling == json!(true) { 1 } else { 2 };
        let r = ask(
            &k,
            json!({"type": "messages", "ids": all, "limit": spelling.clone()}),
        )
        .await;
        assert_eq!(
            r["rows"].as_array().unwrap().len(),
            n,
            "limit {spelling} must coerce like python"
        );
    }
    for junk in [json!([1]), json!({"a": 1}), json!("two"), Value::Null] {
        let r = ask(
            &k,
            json!({"type": "messages", "ids": all, "limit": junk.clone()}),
        )
        .await;
        assert_eq!(
            r["rows"].as_array().unwrap().len(),
            5,
            "limit {junk} is no cap"
        );
    }
    // A NEGATIVE limit breaks on the first row: an empty page, cursor null.
    let neg = ask(&k, json!({"type": "messages", "ids": all, "limit": -1})).await;
    assert!(neg["rows"].as_array().unwrap().is_empty());
    assert_eq!(neg["next"], Value::Null);

    // `rows_read` slices with the same number — python's slice, so a negative
    // bound drops the TAIL.
    let r = ask(
        &k,
        json!({"type": "rows_read", "agent": "ai", "limit": "2"}),
    )
    .await;
    assert_eq!(r["rows"].as_array().unwrap().len(), 2);
    assert_eq!(r["next"], json!(ids[1]));
    let r = ask(&k, json!({"type": "rows_read", "agent": "ai", "limit": -2})).await;
    assert_eq!(r["rows"].as_array().unwrap().len(), 3, "rows[:-2]");
    assert_eq!(r["next"], json!(ids[2]));
    // Beyond i64 saturates WITH ITS SIGN: +big is no cap, −big is an empty page.
    let r = ask(
        &k,
        json!({"type": "rows_read", "agent": "ai", "limit": "99999999999999999999"}),
    )
    .await;
    assert_eq!(r["rows"].as_array().unwrap().len(), 5);
    let r = ask(
        &k,
        json!({"type": "rows_read", "agent": "ai", "limit": "-99999999999999999999"}),
    )
    .await;
    assert!(r["rows"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn digest_cap_counts_python_json_dumps_length() {
    // C68: `len(json.dumps(d))` — ensure_ascii escapes and `", "`/`": "`
    // separators. A compact UTF-8 measurement keeps far more rows than python.
    let k = kernel();
    let ids: Vec<i64> = (0..300)
        .map(|_| {
            plant_row(
                &k,
                "ai",
                json!({"kind": "turn", "role": "user", "content": "日本語".repeat(100)}),
            )
        })
        .collect();
    let r = ask(&k, json!({"type": "messages", "ids": json!(ids)})).await;
    let kept = r["rows"].as_array().unwrap();
    // Every kept digest, measured python's way, fits under the cap; one more
    // would not. That is the assertion the cap makes, independent of id width.
    let dumps: Vec<usize> = kept.iter().map(py_dumps_len).collect();
    let used: usize = dumps.iter().sum();
    assert!(used <= 32 * 1024, "the page overflowed the cap: {used}");
    let next = dumps.last().copied().unwrap_or(0);
    assert!(
        used + next > 32 * 1024,
        "the page stopped early: {used} + {next} still fits"
    );
    // An ensure_ascii digest of a CJK head is ~4x its compact form: measured
    // COMPACT, this whole page is nowhere near the cap, so the old
    // `d.to_string().len()` would have gone on keeping rows.
    let compact: usize = kept.iter().map(|d| d.to_string().len()).sum();
    let one_more = compact / kept.len();
    assert!(
        compact + one_more <= 32 * 1024,
        "compact bytes {compact} — measured that way the page would have stopped \
         here too, so this case proves nothing"
    );
    eprintln!(
        "kept {} rows, {used} python bytes, {compact} compact",
        kept.len()
    );
    assert_eq!(r["next"], kept.last().unwrap()["id"]);
}

/// `len(json.dumps(d))` — the reference measurement, spelled out here so the
/// test does not lean on the implementation it checks.
fn py_dumps_len(d: &Value) -> usize {
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c if c.is_ascii() => out.push(c),
                        c => {
                            let mut buf = [0u16; 2];
                            for unit in c.encode_utf16(&mut buf) {
                                out.push_str(&format!("\\u{unit:04x}"));
                            }
                        }
                    }
                }
                out.push('"');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write(x, out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, (kk, vv)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write(&Value::from(kk.clone()), out);
                    out.push_str(": ");
                    write(vv, out);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    write(d, &mut out);
    out.len()
}

#[tokio::test]
async fn digest_raises_like_python_on_unsized_refs_and_non_object_outcome() {
    // C69. Python calls `len()` on a truthy `refs` and `.get()` on a truthy
    // `outcome`; a scalar in either place kills the whole verb, and the batch
    // reports it as a RAISED handler. Rust used to drop the key and answer.
    let k = kernel();
    let refs = plant_row(&k, "ai", json!({"kind": "fold", "refs": 5}));
    let r = ask(&k, json!({"type": "messages", "ids": [refs]})).await;
    assert_eq!(
        r["error"],
        json!("TypeError: object of type 'int' has no len()")
    );
    assert_eq!(r["raised"], json!(true));

    let ids = plant_row(&k, "ai", json!({"kind": "view", "ids": 5}));
    let r = ask(&k, json!({"type": "messages", "ids": [ids]})).await;
    assert_eq!(
        r["error"],
        json!("TypeError: object of type 'int' has no len()")
    );

    let out = plant_row(
        &k,
        "ai",
        json!({"kind": "call", "verb": "w", "outcome": "done"}),
    );
    let r = ask(&k, json!({"type": "messages", "ids": [out]})).await;
    assert_eq!(
        r["error"],
        json!("AttributeError: 'str' object has no attribute 'get'")
    );
    assert_eq!(r["raised"], json!(true));

    // A DIGIT THAT IS NOT A DECIMAL: `'²'.isdigit()` is True, `int('²')` is not.
    let ok = plant(&k, "ai", "plain");
    let r = ask(&k, json!({"type": "messages", "ids": [ok, "²"]})).await;
    assert_eq!(
        r["error"],
        json!("ValueError: invalid literal for int() with base 10: '²'")
    );
    // …and the same id as a CURSOR raises too.
    let r = ask(&k, json!({"type": "messages", "ids": [ok], "after": "²"})).await;
    assert_eq!(r["raised"], json!(true));
    let r = ask(
        &k,
        json!({"type": "rows_read", "agent": "ai", "after": "²"}),
    )
    .await;
    assert_eq!(r["raised"], json!(true));

    // A FALSY refs/outcome never reaches the raise — nothing is inserted.
    let fine = plant_row(
        &k,
        "ai",
        json!({"kind": "call", "verb": "w", "refs": 0, "outcome": 0}),
    );
    let d = digest_of(&k, fine).await;
    assert!(!d.as_object().unwrap().contains_key("refs"));
    assert!(!d.as_object().unwrap().contains_key("status"));
}
