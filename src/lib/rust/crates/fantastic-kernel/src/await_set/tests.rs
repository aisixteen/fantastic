//! `await_set` — see the module docs. (The restart case lives with the real
//! memory store, in `fantastic-ram-store/tests/await_restart.rs`.)
use super::*;
use crate::agent::Agent;
use crate::bundle::{Bundle, BundleError, Reply};
use crate::test_support::wire;
use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;

/// A handler that RECORDS every payload it is dispatched.
#[derive(Default)]
struct Rec {
    seen: Mutex<Vec<(String, Value)>>,
}

impl Rec {
    fn payloads(&self, agent: &str) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(a, _)| a == agent)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

struct RecBundle(Arc<Rec>);

#[async_trait]
impl Bundle for RecBundle {
    fn name(&self) -> &str {
        "rec"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        if payload["type"] == json!("boot") {
            return Ok(None);
        }
        self.0
            .seen
            .lock()
            .unwrap()
            .push((id.0.clone(), payload.clone()));
        Ok(Some(json!({"ok": true})))
    }
}

fn mk() -> (Arc<Kernel>, Arc<Rec>) {
    let rec = Arc::new(Rec::default());
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register("rec.tools", RecBundle(Arc::clone(&rec)));
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    (kernel, rec)
}

async fn mk_agent(kernel: &Arc<Kernel>, id: &str) {
    let r = kernel
        .send(vec![Item::new(
            "kernel_state",
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "rec.tools", "id": id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "{r}");
}

/// A lead plus workers, wired so each worker may `report` to the lead and the
/// lead may `await_set` on the root.
async fn fleet(workers: &[&str]) -> (Arc<Kernel>, Arc<Rec>) {
    let (kernel, rec) = mk();
    mk_agent(&kernel, "lead").await;
    wire(
        &kernel,
        "lead",
        "kernel_state",
        json!(["await_set", "await_cancel"]),
    );
    for w in workers {
        mk_agent(&kernel, w).await;
        wire(&kernel, w, "lead", json!(["report"]));
        // the lead also talks to them, and they to a stranger below
    }
    (kernel, rec)
}

async fn as_lead(kernel: &Arc<Kernel>, payload: Value) -> Value {
    crate::send::as_agent(
        &AgentId::from("lead"),
        kernel.send(vec![Item::new("kernel_state", payload)]),
    )
    .await
    .remove(0)
    .result
}

async fn report(kernel: &Arc<Kernel>, from: &str, to: &str, extra: Value) -> Value {
    let mut p = json!({"type": "report"});
    p.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    crate::send::as_agent(&AgentId::from(from), kernel.send(vec![Item::new(to, p)]))
        .await
        .remove(0)
        .result
}

/// Poll until `n` payloads reached `agent`, or fail after ~3 s.
async fn wait_for(rec: &Rec, agent: &str, n: usize) -> Vec<Value> {
    for _ in 0..300 {
        let got = rec.payloads(agent);
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{agent} got {:?}, wanted {n}", rec.payloads(agent));
}

fn set_args(ids: Value) -> Value {
    json!({"type": "await_set", "ids": ids})
}

#[tokio::test]
async fn await_set_wakes_the_caller_once_for_n_reports() {
    let (k, rec) = fleet(&["w1", "w2", "w3"]).await;
    let mut a = set_args(json!(["w1", "w2", "w3"]));
    a["label"] = json!("phase-1");
    let r = as_lead(&k, a).await;
    let wid = r["wait_id"].as_i64().expect("a wait_id at once");
    assert_eq!(r["label"], json!("phase-1"));
    assert_eq!(r["ids"], json!(["w1", "w2", "w3"]));
    // out of order on purpose: `done` follows `ids`
    for (w, res) in [("w3", "r3"), ("w1", "r1"), ("w2", "r2")] {
        let held = report(&k, w, "lead", json!({"state": "done", "result": res})).await;
        assert_eq!(held, json!({"held": true, "wait_id": wid}), "{w}");
    }
    let got = wait_for(&rec, "lead", 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let got2 = rec.payloads("lead");
    assert_eq!(got2.len(), 1, "ONE message, no per-report turns: {got2:?}");
    let m = &got[0];
    assert_eq!(m["type"], json!("send"));
    assert_eq!(m["wait_id"], json!(wid));
    assert_eq!(m["label"], json!("phase-1"));
    assert_eq!(m["timed_out"], json!(false));
    assert_eq!(m["missing"], json!([]));
    assert_eq!(
        m["done"],
        json!([
            {"id": "w1", "state": "done", "result": "r1"},
            {"id": "w2", "state": "done", "result": "r2"},
            {"id": "w3", "state": "done", "result": "r3"},
        ])
    );
    let text: Value = serde_json::from_str(m["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["done"], m["done"], "`text` carries the same body");
    assert!(got2.iter().all(|p| p["type"] != json!("report")));
}

#[tokio::test]
async fn await_set_timeout_names_the_missing() {
    let (k, rec) = fleet(&["w1", "w2"]).await;
    let mut a = set_args(json!(["w1", "w2"]));
    a["timeout_s"] = json!(0.2);
    let r = as_lead(&k, a).await;
    assert_eq!(r["timeout_s"], json!(0.2));
    report(&k, "w1", "lead", json!({"state": "done", "result": "a"})).await;
    assert!(
        rec.payloads("lead").is_empty(),
        "nothing before the deadline"
    );
    let got = wait_for(&rec, "lead", 1).await;
    assert_eq!(got[0]["timed_out"], json!(true));
    assert_eq!(
        got[0]["done"],
        json!([{"id": "w1", "state": "done", "result": "a"}])
    );
    assert_eq!(got[0]["missing"], json!(["w2"]));
    // a late report is an ordinary message now
    let late = report(&k, "w2", "lead", json!({"state": "done", "result": "b"})).await;
    assert_eq!(late, json!({"ok": true}));
    let all = rec.payloads("lead");
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!(all[1]["type"], json!("report"));
}

#[tokio::test]
async fn await_set_holds_a_report_instead_of_delivering_it() {
    let (k, rec) = fleet(&["w1", "w2"]).await;
    mk_agent(&k, "stranger").await;
    wire(&k, "stranger", "lead", json!(["report", "note"]));
    wire(&k, "w1", "lead", json!(["report", "note"]));
    let r = as_lead(&k, set_args(json!(["w1", "w2"]))).await;
    let wid = r["wait_id"].as_i64().unwrap();
    let held = report(&k, "w1", "lead", json!({"state": "done", "result": "x"})).await;
    assert_eq!(held, json!({"held": true, "wait_id": wid}));
    assert!(rec.payloads("lead").is_empty(), "held, not delivered");
    // a report from outside the set, and a non-report from inside it: normal
    let a = report(&k, "stranger", "lead", json!({"state": "done"})).await;
    assert_eq!(a, json!({"ok": true}));
    let b = crate::send::as_agent(
        &AgentId::from("w1"),
        k.send(vec![Item::new("lead", json!({"type": "note"}))]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(b, json!({"ok": true}));
    assert_eq!(rec.payloads("lead").len(), 2);
}

#[tokio::test]
async fn await_cancel_releases_held_reports_in_order() {
    let (k, rec) = fleet(&["w1", "w2", "w3"]).await;
    let wid = as_lead(&k, set_args(json!(["w1", "w2", "w3"]))).await["wait_id"]
        .as_i64()
        .unwrap();
    report(&k, "w2", "lead", json!({"state": "done", "result": "two"})).await;
    report(
        &k,
        "w1",
        "lead",
        json!({"state": "failed", "result": "one"}),
    )
    .await;
    assert!(rec.payloads("lead").is_empty());
    let c = as_lead(&k, json!({"type": "await_cancel", "wait_id": wid})).await;
    assert_eq!(c, json!({"cancelled": true, "wait_id": wid, "released": 2}));
    let got = wait_for(&rec, "lead", 2).await;
    assert_eq!(got[0]["type"], json!("report"));
    assert_eq!(got[0]["result"], json!("two"), "in the order held");
    assert_eq!(got[1]["result"], json!("one"));
    // the third reporting later makes no combined message
    let late = report(
        &k,
        "w3",
        "lead",
        json!({"state": "done", "result": "three"}),
    )
    .await;
    assert_eq!(late, json!({"ok": true}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let all = rec.payloads("lead");
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(all.iter().all(|p| p["type"] == json!("report")));
    // unknown and foreign waits are an honest false; no wait_id is refused
    let none = as_lead(&k, json!({"type": "await_cancel", "wait_id": wid})).await;
    assert_eq!(none["cancelled"], json!(false));
    let w2 = as_lead(&k, set_args(json!(["w1"]))).await["wait_id"]
        .as_i64()
        .unwrap();
    wire(&k, "w1", "kernel_state", json!(["await_cancel"]));
    let foreign = crate::send::as_agent(
        &AgentId::from("w1"),
        k.send(vec![Item::new(
            "kernel_state",
            json!({"type": "await_cancel", "wait_id": w2}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(foreign["cancelled"], json!(false), "{foreign}");
    let miss = as_lead(&k, json!({"type": "await_cancel"})).await;
    assert_eq!(miss["reason"], json!("missing_arg"), "{miss}");
}

#[tokio::test]
async fn await_set_refuses_bad_args() {
    let (k, rec) = fleet(&["w1"]).await;
    let cases: Vec<(Value, &str, &str)> = vec![
        (json!({"type": "await_set"}), "missing_arg", "ids"),
        (
            json!({"type": "await_set", "ids": []}),
            "missing_arg",
            "ids",
        ),
        (json!({"type": "await_set", "ids": "w1"}), "bad_arg", "ids"),
        (json!({"type": "await_set", "ids": [3]}), "bad_arg", "ids"),
        (
            json!({"type": "await_set", "ids": ["nobody"]}),
            "unknown_agent",
            "",
        ),
        (
            json!({"type": "await_set", "ids": ["lead"]}),
            "bad_arg",
            "ids",
        ),
        (
            json!({"type": "await_set", "ids": ["w1"], "timeout_s": 0}),
            "bad_arg",
            "timeout_s",
        ),
        (
            json!({"type": "await_set", "ids": ["w1"], "timeout_s": -2}),
            "bad_arg",
            "timeout_s",
        ),
        (
            json!({"type": "await_set", "ids": ["w1"], "timeout_s": "5"}),
            "bad_arg",
            "timeout_s",
        ),
        (
            json!({"type": "await_set", "ids": ["w1"], "label": 7}),
            "bad_arg",
            "label",
        ),
    ];
    for (p, reason, arg) in cases {
        let r = as_lead(&k, p.clone()).await;
        assert_eq!(r["reason"], json!(reason), "{p}: {r}");
        if !arg.is_empty() {
            assert_eq!(r["arg"], json!(arg), "{p}: {r}");
        }
    }
    // an operator send has nobody to wake
    let op = k
        .send(vec![Item::new("kernel_state", set_args(json!(["w1"])))])
        .await
        .remove(0)
        .result;
    assert_eq!(op["reason"], json!("bad_arg"), "{op}");
    // nothing was registered: a report is delivered, not held
    let r = report(&k, "w1", "lead", json!({"state": "done"})).await;
    assert_eq!(r, json!({"ok": true}));
    assert_eq!(rec.payloads("lead").len(), 1);
}

#[tokio::test]
async fn await_set_is_root_only_and_gated() {
    let (k, _rec) = fleet(&["w1"]).await;
    // answered by the root alone
    wire(&k, "lead", "w1", json!(["await_set"]));
    let r = as_lead_to(&k, "w1", set_args(json!(["w1"]))).await;
    assert_eq!(r["reason"], json!("not_here"), "{r}");
    // a caller with no grant of the verb on the root is refused
    let r = crate::send::as_agent(
        &AgentId::from("w1"),
        k.send(vec![Item::new("kernel_state", set_args(json!(["lead"])))]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(r["reason"], json!("unauthorized"), "{r}");
}

async fn as_lead_to(kernel: &Arc<Kernel>, to: &str, payload: Value) -> Value {
    crate::send::as_agent(
        &AgentId::from("lead"),
        kernel.send(vec![Item::new(to, payload)]),
    )
    .await
    .remove(0)
    .result
}

#[tokio::test]
async fn a_duplicate_report_from_one_agent_replaces_the_first() {
    let (k, rec) = fleet(&["w1", "w2"]).await;
    as_lead(&k, set_args(json!(["w1", "w2"]))).await;
    report(
        &k,
        "w1",
        "lead",
        json!({"state": "working", "result": "first"}),
    )
    .await;
    report(
        &k,
        "w1",
        "lead",
        json!({"state": "done", "result": "second"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        rec.payloads("lead").is_empty(),
        "a duplicate does not complete the set"
    );
    report(&k, "w2", "lead", json!({"state": "done", "result": "w"})).await;
    let got = wait_for(&rec, "lead", 1).await;
    assert_eq!(
        got[0]["done"],
        json!([
            {"id": "w1", "state": "done", "result": "second"},
            {"id": "w2", "state": "done", "result": "w"},
        ])
    );
    assert_eq!(got[0]["missing"], json!([]));
}

#[tokio::test]
async fn await_set_completion_race_delivers_once() {
    let (k, rec) = fleet(&["w1"]).await;
    let mut a = set_args(json!(["w1"]));
    a["timeout_s"] = json!(0.05);
    as_lead(&k, a).await;
    tokio::time::sleep(Duration::from_millis(48)).await;
    let _ = report(&k, "w1", "lead", json!({"state": "done", "result": "x"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let all = rec.payloads("lead");
    let combined: Vec<_> = all.iter().filter(|p| p.get("wait_id").is_some()).collect();
    assert_eq!(combined.len(), 1, "exactly one combined message: {all:?}");
}
