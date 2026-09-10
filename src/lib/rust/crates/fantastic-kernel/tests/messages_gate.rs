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
    let _rx = k.register(Arc::clone(&root));
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
    let _rx = k.register(Arc::clone(&a));
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
    let _rx = k.register(Arc::clone(&a));
}
