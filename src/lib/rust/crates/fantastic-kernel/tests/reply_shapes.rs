//! K-D — the REPLY SHAPES a kernel verb answers with, graded against python.
//!
//! Python is the spec for every one of these. Each row here is a measured
//! divergence from the 2026-09-02 sweep: rust answered the bare record where
//! python answers `{updated, id, agent}`, a bare `{error}` where python answers
//! a coded refusal, an eighth `verb` key on a ledger row python renders with
//! seven, and its own prose on the `not_here` code. A caller that branches on
//! `reason`, or reads `updated`, or diffs a row's key set, got a different
//! answer per runtime for the same call — which is exactly the thing the one
//! refusal shape exists to prevent.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

struct Noop;

#[async_trait]
impl Bundle for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(json!({"ok": true})))
    }
}

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", Noop);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (tmp, kernel)
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn mk_child(kernel: &Arc<Kernel>, id: &str) {
    let r = call(
        kernel,
        "kernel",
        json!({
            "type": "create_agent",
            "parent_id": "kernel",
            "handler_module": "noop.tools",
            "id": id,
        }),
    )
    .await;
    assert_eq!(r["id"], id, "fixture create failed: {r}");
}

/// `{updated:true, id, agent}` — not the bare record. A caller taught by the
/// kernel_verbs doc branches on `updated`; the record has no such field, so the
/// call that SUCCEEDED read as a failure and the model retried it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_agent_answers_updated_id_and_the_record() {
    let (_t, kernel) = mk();
    mk_child(&kernel, "kid").await;
    let r = call(
        &kernel,
        "kernel",
        json!({"type": "update_agent", "id": "kid", "note": "hello"}),
    )
    .await;
    assert_eq!(r["updated"], true, "{r}");
    assert_eq!(r["id"], "kid", "{r}");
    // The record rides UNDER `agent`, whole — the grant a caller just wrote is
    // read back from there, not off the reply's top level.
    assert_eq!(r["agent"]["id"], "kid", "{r}");
    assert_eq!(r["agent"]["note"], "hello", "{r}");
    // And the three keys are the whole reply, so a key-set diff against python
    // holds.
    let keys: Vec<&String> = r.as_object().expect("object reply").keys().collect();
    assert_eq!(keys, vec!["updated", "id", "agent"], "{r}");
}

/// An unknown id is `unknown_agent` + `deleted:false` — a code to branch on and
/// the LIVE id index to correct from. The bare `{error: "no agent \"typo\""}`
/// this used to answer is the measured 34-retry case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_agent_on_an_unknown_id_is_the_unknown_agent_refusal() {
    let (_t, kernel) = mk();
    mk_child(&kernel, "kid").await;
    let r = call(
        &kernel,
        "kernel",
        json!({"type": "delete_agent", "id": "typo"}),
    )
    .await;
    assert_eq!(r["reason"], "unknown_agent", "{r}");
    assert_eq!(r["deleted"], false, "{r}");
    // The legal values, so the caller can self-correct without a second verb.
    let ids: Vec<&str> = r["agents"]
        .as_array()
        .expect("agents index")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(ids.contains(&"kid"), "{r}");
    assert!(ids.contains(&"core"), "{r}");
}

/// No `id` at all is `missing_arg`, carrying the ARG — the machine field is the
/// whole point of the code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_agent_without_an_id_is_missing_arg() {
    let (_t, kernel) = mk();
    let r = call(&kernel, "kernel", json!({"type": "delete_agent"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(r["arg"], "id", "{r}");
    assert_eq!(r["error"], "delete_agent: id required", "{r}");
}

/// Same for `cancel` — `{cancelled:false}` would claim the kernel looked; it
/// never did, because nothing was named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_without_a_task_id_is_missing_arg() {
    let (_t, kernel) = mk();
    let r = call(&kernel, "kernel", json!({"type": "cancel"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(r["arg"], "task_id", "{r}");
    assert_eq!(r["error"], "cancel: task_id required", "{r}");
}

/// A verb the ROOT cannot answer is `unhandled` + the `verb` that was asked —
/// never a reason-less `{error}`, which `apply_reflect_flags` would decorate
/// into something that reads like a healthy reflect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswerable_verb_at_the_root_carries_reason_and_verb() {
    let (_t, kernel) = mk();
    let r = call(&kernel, "kernel", json!({"type": "no_such_system_verb"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "no_such_system_verb", "{r}");
}

/// `not_here` says PYTHON'S sentence and names the verb in a machine field. One
/// code must not mean two prose answers across runtimes, and a caller should not
/// have to parse the sentence to learn which verb it misaddressed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kernel_verb_at_a_child_says_pythons_words_and_names_the_verb() {
    let (_t, kernel) = mk();
    mk_child(&kernel, "kid").await;
    let r = call(&kernel, "kid", json!({"type": "ps"})).await;
    assert_eq!(r["reason"], "not_here", "{r}");
    assert_eq!(r["verb"], "ps", "{r}");
    assert_eq!(
        r["error"], "ps is a kernel verb — kid does not answer it",
        "{r}"
    );
    assert_eq!(
        r["hint"],
        "address the kernel root (alias 'kernel'); name the agent you mean in \
         `parent_id` / `id`",
        "{r}"
    );
}

/// A `ledger` row is python's SEVEN keys. `verb` was an eighth that only the
/// ports carried; it still rides the durable sidecar (where a joiner is
/// authorized against it after a reboot), just not the read verb.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ledger_rows_carry_no_verb_key() {
    let (_t, kernel) = mk();
    mk_child(&kernel, "kid").await;
    // One NAMED execution, so the row is settled and carries an outcome.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kid"),
            json!({"type": "poke"}),
        )
        .with_id("job-1")])
        .await;
    let r = call(&kernel, "kernel", json!({"type": "ledger"})).await;
    let rows = r["executions"].as_array().expect("executions");
    assert!(!rows.is_empty(), "{r}");
    for row in rows {
        let keys: Vec<&str> = row
            .as_object()
            .expect("row object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(!keys.contains(&"verb"), "row carries `verb`: {row}");
        for want in ["id", "target", "status", "attempts", "retries", "named"] {
            assert!(keys.contains(&want), "row is missing {want}: {row}");
        }
    }
}

/// `get` is NOT a kernel verb. Rust and swift answered it (returning the record,
/// or `null` for an unknown id) and advertised it in `kernel_verbs`; python has
/// no such verb and answers the root bundle's UNHANDLED refusal. A model reading
/// a rust reflect therefore learned a verb the reference runtime does not have
/// (e2e `agentic.build:rust`, 2026-09-03). A record is read with `list_agents`
/// or a `reflect`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_is_unhandled_not_a_kernel_verb() {
    let (_t, kernel) = mk();
    mk_child(&kernel, "g1").await;
    let r = call(&kernel, "kernel", json!({"type": "get", "id": "g1"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "get", "the refusal names the verb");
    assert!(r["id"].is_null(), "it must not answer with a record: {r}");
}

/// C6 — `list_agents` answers the registry in REGISTRATION ORDER.
///
/// Python iterates `self.ctx.agents`, a plain dict, so the root leads and every
/// agent follows in the order it was registered. Rust's registry is a `DashMap`
/// with no order, and the port sorted by id — which put `alpha` ahead of the
/// root and made the walk in `PT/kernel/test_pagination.py` reassemble a
/// different registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_agents_is_registry_order() {
    let (_t, kernel) = mk();
    // Deliberately NOT alphabetical: sorting would put `alpha` first.
    for id in ["zulu", "mike", "alpha"] {
        mk_child(&kernel, id).await;
    }
    let r = call(&kernel, "kernel", json!({"type": "list_agents"})).await;
    let ids: Vec<&str> = r["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .map(|a| a["id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["core", "zulu", "mike", "alpha"], "{r}");

    // A delete removes it from the order and leaves the rest as they were.
    let d = call(
        &kernel,
        "kernel",
        json!({"type": "delete_agent", "id": "mike"}),
    )
    .await;
    assert_eq!(d["deleted"], true, "{d}");
    let r = call(&kernel, "kernel", json!({"type": "list_agents"})).await;
    let ids: Vec<&str> = r["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .map(|a| a["id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["core", "zulu", "alpha"], "{r}");

    // …and the cursor pages through THAT order, not a sorted one.
    let p = call(
        &kernel,
        "kernel",
        json!({"type": "list_agents", "limit": 2}),
    )
    .await;
    let ids: Vec<&str> = p["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .map(|a| a["id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["core", "zulu"], "{p}");
}

/// C15 — a record serializes `id, handler_module?, <meta in order>, parent_id`.
///
/// Python writes `parent_id` LAST because structure wins: a `parent_id` that
/// leaked into meta must not shadow where the agent actually sits. Rust
/// declared it third, so every record on the wire and in `agent.json` carried a
/// key order python never produces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_parent_id_is_last() {
    let (_t, kernel) = mk();
    let created = call(
        &kernel,
        "kernel",
        json!({
            "type": "create_agent",
            "parent_id": "kernel",
            "handler_module": "noop.tools",
            "id": "kid",
            "display_name": "Kid",
            "note": "second",
        }),
    )
    .await;
    // `can_call`/`callable_by` are the stamped structural baseline; they land in
    // meta ahead of the authored keys.
    let keys: Vec<&str> = created
        .as_object()
        .expect("object record")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.first(), Some(&"id"), "{created}");
    assert_eq!(keys.get(1), Some(&"handler_module"), "{created}");
    assert_eq!(keys.last(), Some(&"parent_id"), "{created}");
    // Meta keeps its own order between the two.
    let dn = keys.iter().position(|k| *k == "display_name").unwrap();
    let note = keys.iter().position(|k| *k == "note").unwrap();
    assert!(dn < note, "meta order was not preserved: {keys:?}");

    // Same order off `list_agents` — one record shape, one spelling.
    let r = call(&kernel, "kernel", json!({"type": "list_agents"})).await;
    let kid = r["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "kid")
        .expect("kid is registered");
    let keys: Vec<&str> = kid
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.last(), Some(&"parent_id"), "{kid}");
    // The ROOT carries no `parent_id` at all.
    let root = r["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "core")
        .expect("root is registered");
    assert!(root.get("parent_id").is_none(), "{root}");
}
