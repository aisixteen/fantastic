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
    let _rx = kernel.register(Arc::clone(&root));
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
