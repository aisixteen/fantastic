//! A call's settle is a MESSAGE: the ack comes back at once, the settle is
//! delivered later to the call's `reply_to` — a continuation, else the
//! receiver's `reply` verb, else the row only — and a `delivered` row naming
//! the settle is APPENDED; the settle row itself is never touched. A handler may defer its settle; a settle that comes late wakes
//! nothing.

use crate::agent::{Agent, AgentId};
use crate::bundle::{BundleError, Handler, Reply};
use crate::kernel::Kernel;
use crate::outcome::{Item, Outcome, ReplyTo, Status};
use crate::tasks::{current_task, TaskId};
use crate::test_support::{as_agent, send_settled};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the receivers saw.
#[derive(Default)]
struct Seen {
    /// (receiving agent, the `reply` payload, who the handler ran as)
    replies: Mutex<Vec<(String, Value, Option<String>)>>,
    /// (who the continuation ran as, its outcome)
    thens: Mutex<Vec<(Option<String>, Outcome)>>,
    /// How many times `work` ran.
    runs: Mutex<usize>,
    /// The executions that deferred.
    deferred: Mutex<Vec<crate::Deferred>>,
    /// Holds `gate` until released.
    gate: tokio::sync::Notify,
    /// The settle row as it stood while its delivery ran, serialized.
    settle_at_delivery: Mutex<Vec<String>>,
}

impl Seen {
    fn replies(&self) -> Vec<(String, Value, Option<String>)> {
        self.replies.lock().unwrap().clone()
    }
    fn thens(&self) -> Vec<(Option<String>, Outcome)> {
        self.thens.lock().unwrap().clone()
    }
    fn runs(&self) -> usize {
        *self.runs.lock().unwrap()
    }
}

struct Rcv {
    seen: Arc<Seen>,
}

crate::stand_in_verbs!(rcv_verbs {
    ReplyV "reply" { re: Option<i64>, target: Option<String>, verb: Option<String>, status: Option<String>, result: Option<serde_json::Value> },
    WorkV "work" { ms: Option<i64> },
    GateV "gate" {  },
    DeferV "defer" {  },
    AskV "ask" { to: Option<String>, reply_to: Option<String> },
    CallThenV "call_then" { to: Option<String> },
});

#[async_trait]
impl Handler for Rcv {
    type Call = rcv_verbs::Call;
    fn name(&self) -> &str {
        "rcv"
    }
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn verbs(&self) -> &'static [crate::Verb] {
        rcv_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &Agent) -> Vec<crate::DynVerb> {
        rcv_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static crate::RecordDecl {
        crate::fixture_record!()
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let p = call.payload();
        let verb = p["type"].as_str().unwrap_or("").to_string();
        let reply = match verb.as_str() {
            "reply" => {
                if let Some(row) = kernel.rows.all().into_iter().find(|r| {
                    r.get("kind") == Some(&json!("settle")) && r.get("parent") == p.get("re")
                }) {
                    self.seen
                        .settle_at_delivery
                        .lock()
                        .unwrap()
                        .push(serde_json::to_string(&row).unwrap());
                }
                self.seen.replies.lock().unwrap().push((
                    agent_id.0.clone(),
                    p.clone(),
                    crate::send::current_sender().map(|s| s.0),
                ));
                Value::Null
            }
            "work" => {
                *self.seen.runs.lock().unwrap() += 1;
                if let Some(ms) = p["ms"].as_u64() {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                json!({"done": true})
            }
            "gate" => {
                self.seen.gate.notified().await;
                json!({"opened": true})
            }
            "defer" => {
                let d = kernel.defer().map_err(|e| e.to_string())?;
                self.seen.deferred.lock().unwrap().push(d);
                return Ok(None);
            }
            "ask" => {
                let to = AgentId::from(p["to"].as_str().unwrap_or(""));
                let reply_to = match p["reply_to"].as_str() {
                    None => ReplyTo::Caller,
                    Some("") => ReplyTo::Row,
                    Some(id) => ReplyTo::Agent(AgentId::from(id)),
                };
                let out = kernel
                    .send(vec![
                        Item::new(to, json!({"type": "work"})).with_reply_to(reply_to)
                    ])
                    .await
                    .remove(0);
                json!({"pid": out.pid.map(|p| p.0)})
            }
            "call_then" => {
                let to = AgentId::from(p["to"].as_str().unwrap_or(""));
                let seen = Arc::clone(&self.seen);
                let ack = kernel
                    .call(Item::new(to, json!({"type": "work"})), move |o| async move {
                        let me = crate::send::current_sender().map(|s| s.0);
                        seen.thens.lock().unwrap().push((me, o));
                    })
                    .await;
                ack.to_value()
            }
            other => json!({"error": format!("unknown verb {other}")}),
        };
        Ok(Some(reply))
    }
}

fn mk(seen: &Arc<Seen>) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(
        "rcv.tools",
        Rcv {
            seen: Arc::clone(seen),
        },
    );
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
    kernel
}

async fn agent(kernel: &Arc<Kernel>, id: &str, can: Value) {
    let mut payload = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "rcv.tools", "id": id});
    if !can.is_null() {
        payload["can"] = can;
    }
    let r = send_settled(kernel, vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "{r}");
}

/// The `delivered` rows of `pid`, still in RAM.
fn delivered_rows(kernel: &Arc<Kernel>, pid: i64) -> Vec<crate::rows::Row> {
    kernel
        .rows
        .all()
        .into_iter()
        .filter(|r| r.get("kind") == Some(&json!("delivered")) && r.get("parent") == Some(&json!(pid)))
        .collect()
}

/// The settle rows of `pid`, still in RAM (the kernel never started).
fn settles(kernel: &Arc<Kernel>, pid: i64) -> Vec<crate::rows::Row> {
    kernel
        .rows
        .all()
        .into_iter()
        .filter(|r| r.get("kind") == Some(&json!("settle")) && r.get("parent") == Some(&json!(pid)))
        .collect()
}

async fn until<F: Fn() -> bool>(ok: F) -> bool {
    for _ in 0..300 {
        if ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ok()
}

#[tokio::test]
async fn a_call_returns_its_ack_before_the_handler_runs() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "b", Value::Null).await;
    let got = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&got);
    let ack = crate::send::as_operator(k.call(
        Item::new(AgentId::from("b"), json!({"type": "gate"})),
        move |o| async move {
            *slot.lock().unwrap() = Some(o);
        },
    ))
    .await;
    // THE ACK: a pid and `accepted`, while the handler is held
    assert_eq!(ack.status, Status::Accepted);
    let pid = ack.pid.expect("an ack carries the pid").0;
    assert_eq!(ack.to_value()["status"], json!("accepted"));
    assert!(got.lock().unwrap().is_none(), "nothing settled yet");
    assert!(settles(&k, pid).is_empty(), "no settle row yet");
    // the call row is written at launch, and the execution is in `ps`
    assert!(
        k.rows.get_one(pid).is_some_and(|r| r["kind"] == json!("call")),
        "the call row is written with the ack"
    );
    assert!(k.tasks.contains(&TaskId(pid)) || until(|| k.tasks.contains(&TaskId(pid))).await);
    // released: the settle arrives later, as a message
    seen.gate.notify_one();
    assert!(until(|| got.lock().unwrap().is_some()).await);
    let out = got.lock().unwrap().clone().unwrap();
    assert_eq!(out.status, Status::Completed);
    assert_eq!(out.result, json!({"opened": true}));
    assert_eq!(out.pid, Some(TaskId(pid)));
}

#[tokio::test]
async fn a_settle_is_delivered_to_reply_to_once() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "a", json!({"b": {"call": ["*"]}})).await;
    agent(&k, "b", Value::Null).await;
    agent(&k, "c", Value::Null).await;
    // the default: the caller receives it
    let pid = send_settled(&k, vec![Item::new(AgentId::from("a"), json!({"type": "ask", "to": "b"}))])
        .await
        .remove(0)
        .result["pid"]
        .as_i64()
        .expect("a's call to b has a pid");
    assert!(until(|| !seen.replies().is_empty()).await);
    let replies = seen.replies();
    assert_eq!(replies.len(), 1, "delivered once: {replies:?}");
    let (to, payload, ran_as) = &replies[0];
    assert_eq!(to, "a");
    assert_eq!(ran_as.as_deref(), Some("a"), "the receiver's handler runs as itself");
    assert_eq!(payload["re"], json!(pid));
    assert_eq!(payload["target"], json!("b"));
    assert_eq!(payload["verb"], json!("work"));
    assert_eq!(payload["status"], json!("completed"));
    assert_eq!(payload["result"], json!({"done": true}));
    let rows = settles(&k, pid);
    assert_eq!(rows.len(), 1);
    assert!(until(|| delivered_rows(&k, pid).len() == 1).await);
    assert_eq!(delivered_rows(&k, pid)[0]["settle"], rows[0]["id"], "it names the settle row");
    assert!(!rows[0].contains_key("delivered"));
    assert_eq!(
        k.rows.get_one(pid).unwrap()["reply_to"],
        json!("a"),
        "the call row names its receiver"
    );

    // named: another agent receives it, the caller does not
    let pid = send_settled(
        &k,
        vec![Item::new(AgentId::from("a"), json!({"type": "ask", "to": "b", "reply_to": "c"}))],
    )
    .await
    .remove(0)
    .result["pid"]
    .as_i64()
    .unwrap();
    assert!(until(|| seen.replies().len() == 2).await);
    let (to, payload, _) = seen.replies()[1].clone();
    assert_eq!(to, "c");
    assert_eq!(payload["re"], json!(pid));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(seen.replies().len(), 2, "no second delivery");
}

#[tokio::test]
async fn reply_to_null_writes_only_the_row() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "a", json!({"b": {"call": ["*"]}})).await;
    agent(&k, "b", Value::Null).await;
    let pid = send_settled(
        &k,
        vec![Item::new(AgentId::from("a"), json!({"type": "ask", "to": "b", "reply_to": ""}))],
    )
    .await
    .remove(0)
    .result["pid"]
    .as_i64()
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(seen.replies().is_empty(), "nobody is told: {:?}", seen.replies());
    let rows = settles(&k, pid);
    assert_eq!(rows.len(), 1, "the settle row is written");
    assert!(delivered_rows(&k, pid).is_empty(), "nothing is owed, nothing is appended");
    assert_eq!(k.rows.get_one(pid).unwrap()["reply_to"], Value::Null);
    // the operator, too, gets the row only
    let out = send_settled(&k, vec![Item::new(AgentId::from("b"), json!({"type": "work"}))])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Completed);
    assert!(seen.replies().is_empty());
}

#[tokio::test]
async fn a_timed_out_call_delivers_its_dead_letter() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "a", Value::Null).await;
    agent(&k, "b", Value::Null).await;
    let out = crate::send::as_operator(k.send(vec![Item::new(
        AgentId::from("b"),
        json!({"type": "work", "ms": 5000}),
    )
    .with_timeout(0.05)
    .with_reply_to(ReplyTo::Agent(AgentId::from("a")))]))
    .await
    .remove(0);
    assert_eq!(out.status, Status::TimedOut);
    assert!(until(|| !seen.replies().is_empty()).await);
    let replies = seen.replies();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].0, "a");
    assert_eq!(replies[0].1["status"], json!("timed_out"));
    assert_eq!(replies[0].1["re"], json!(out.pid.unwrap().0));
    assert!(
        replies[0].1["result"]["error"].as_str().unwrap_or("").contains("timeout"),
        "{:?}",
        replies[0].1
    );
}

#[tokio::test]
async fn a_joiner_gets_the_same_pid_and_the_settle() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "b", Value::Null).await;
    let outs: Arc<Mutex<Vec<Outcome>>> = Arc::default();
    let mut acks = Vec::new();
    for _ in 0..2 {
        let o = Arc::clone(&outs);
        acks.push(
            crate::send::as_operator(k.call(
                Item::new(AgentId::from("b"), json!({"type": "work", "ms": 100})).with_id("job-1"),
                move |out| async move { o.lock().unwrap().push(out) },
            ))
            .await,
        );
    }
    assert!(acks.iter().all(|a| a.status == Status::Accepted));
    assert_eq!(acks[0].pid, acks[1].pid, "the joiner acks the same pid");
    assert!(until(|| outs.lock().unwrap().len() == 2).await);
    let outs = outs.lock().unwrap().clone();
    assert!(outs.iter().all(|o| o.status == Status::Completed && o.pid == acks[0].pid));
    assert_eq!(outs[0].result, outs[1].result);
    assert_eq!(seen.runs(), 1, "one execution");
    // a third call of the settled name: the recorded outcome, nothing runs
    let o3 = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&o3);
    let ack = crate::send::as_operator(k.call(
        Item::new(AgentId::from("b"), json!({"type": "work"})).with_id("job-1"),
        move |out| async move { *slot.lock().unwrap() = Some(out) },
    ))
    .await;
    assert_eq!(ack.pid, acks[0].pid);
    assert!(until(|| o3.lock().unwrap().is_some()).await);
    assert_eq!(seen.runs(), 1);
}

#[tokio::test]
async fn a_deferred_settle_settles_later() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "b", Value::Null).await;
    let got = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&got);
    let ack = crate::send::as_operator(k.call(
        Item::new(AgentId::from("b"), json!({"type": "defer"})),
        move |o| async move { *slot.lock().unwrap() = Some(o) },
    ))
    .await;
    let pid = ack.pid.unwrap();
    assert!(until(|| !seen.deferred.lock().unwrap().is_empty()).await);
    let d = seen.deferred.lock().unwrap()[0];
    assert_eq!(d.pid(), pid);
    // the handler returned; the execution is still open — in `ps`, not settled
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(got.lock().unwrap().is_none(), "a deferred call does not settle with its handler");
    assert!(k.tasks.contains(&pid), "the open execution stays in ps");
    assert!(settles(&k, pid.0).is_empty());
    // the later answer settles it
    assert!(k.settle_deferred(pid, Outcome::new(None, Status::Completed, json!({"later": 1}))));
    assert!(until(|| got.lock().unwrap().is_some()).await);
    let out = got.lock().unwrap().clone().unwrap();
    assert_eq!(out.status, Status::Completed);
    assert_eq!(out.result, json!({"later": 1}));
    assert_eq!(out.pid, Some(pid));
    assert!(until(|| settles(&k, pid.0).len() == 1).await);
    assert!(!k.tasks.contains(&pid));
    // `defer` outside a dispatch is refused
    assert!(k.defer().is_err());
    assert!(current_task().is_none());
}

#[tokio::test]
async fn a_late_settle_wakes_nothing() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "b", Value::Null).await;
    let outs: Arc<Mutex<Vec<Outcome>>> = Arc::default();
    let o = Arc::clone(&outs);
    let ack = crate::send::as_operator(k.call(
        Item::new(AgentId::from("b"), json!({"type": "defer"})).with_timeout(0.05),
        move |out| async move { o.lock().unwrap().push(out) },
    ))
    .await;
    let pid = ack.pid.unwrap();
    assert!(until(|| outs.lock().unwrap().len() == 1).await);
    assert_eq!(outs.lock().unwrap()[0].status, Status::TimedOut, "the deadline ends a deferred call");
    // the answer comes after: nothing waits for it
    assert!(!k.settle_deferred(pid, Outcome::new(None, Status::Completed, json!({"late": true}))));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(outs.lock().unwrap().len(), 1, "no second delivery");
    assert_eq!(settles(&k, pid.0).len(), 1, "one settle row");
    // and a pid nobody deferred
    assert!(!k.settle_deferred(TaskId(999_999), Outcome::new(None, Status::Completed, json!(null))));
}

#[tokio::test]
async fn the_continuation_runs_as_the_caller_in_place_of_its_reply_verb() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "a", json!({"b": {"call": ["*"]}})).await;
    agent(&k, "b", Value::Null).await;
    let ack = send_settled(&k, vec![Item::new(AgentId::from("a"), json!({"type": "call_then", "to": "b"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(ack["status"], json!("accepted"), "{ack}");
    let pid = ack["pid"].as_i64().unwrap();
    assert!(until(|| !seen.thens().is_empty()).await);
    let thens = seen.thens();
    assert_eq!(thens.len(), 1);
    assert_eq!(thens[0].0.as_deref(), Some("a"), "the continuation is a's");
    assert_eq!(thens[0].1.status, Status::Completed);
    assert_eq!(thens[0].1.result, json!({"done": true}));
    assert_eq!(thens[0].1.pid, Some(TaskId(pid)));
    // a declares `reply`, and the continuation took the settle in its place
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(seen.replies().is_empty(), "{:?}", seen.replies());
    assert!(until(|| delivered_rows(&k, pid).len() == 1).await);
    // a refused call acks the refusal, and its continuation never runs
    let refused = as_agent(
        AgentId::from("b"),
        k.call(Item::new(AgentId::from("a"), json!({"type": "work"})), |_| async {
            panic!("a refused call has no settle")
        }),
    )
    .await;
    assert_eq!(refused.status, Status::Failed);
    assert!(refused.pid.is_none());
}

#[tokio::test]
async fn a_delivery_leaves_the_settle_row_byte_for_byte() {
    let seen = Arc::new(Seen::default());
    let k = mk(&seen);
    agent(&k, "a", json!({"b": {"call": ["*"]}})).await;
    agent(&k, "b", Value::Null).await;
    let pid = send_settled(&k, vec![Item::new(AgentId::from("a"), json!({"type": "ask", "to": "b"}))])
        .await
        .remove(0)
        .result["pid"]
        .as_i64()
        .unwrap();
    assert!(until(|| delivered_rows(&k, pid).len() == 1).await);
    // the row as the receiver saw it while its delivery ran, and after
    let during = seen.settle_at_delivery.lock().unwrap().clone();
    assert_eq!(during.len(), 1);
    let after = serde_json::to_string(&settles(&k, pid)[0]).unwrap();
    assert_eq!(during[0], after, "the settle row is never touched");
}
