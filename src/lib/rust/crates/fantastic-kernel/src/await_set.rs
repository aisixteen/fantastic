//! `await_set` — ONE wake for a SET of agents.
//!
//! A lead that waits for several children would otherwise wake on every report
//! and re-send its whole context each time. `await_set {ids, timeout_s?, label?}`
//! answers at once with a `wait_id`; every message of `type: "report"` that a
//! NAMED agent sends the caller before the set completes is HELD (not delivered
//! as its own turn); when every named agent has reported, or the timeout passes,
//! the caller gets ONE message carrying them all. `await_cancel {wait_id}` drops a
//! wait and delivers what it held, one by one.
//!
//! The signal is a plain message — a payload whose `type` is `report`, from the
//! named agent to the waiting caller — so this module depends on no `report`
//! verb. State is a table on the kernel plus three kinds of EXECUTION ROW
//! (`await_open`, `await_hold`, `await_close`) in the kernel's own memory store,
//! so a wait is rebuilt on boot ([`restore`]). Python twin: `kernel/_await.py`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::agent::AgentId;
use crate::kernel::Kernel;
use crate::outcome::Item;
use crate::rows::Row;

/// The row kinds a wait is written as, in the kernel's store.
const KINDS: [&str; 3] = ["await_open", "await_hold", "await_close"];

/// One held report.
#[derive(Clone)]
struct Held {
    state: Value,
    result: Value,
    payload: Value,
}

/// One open wait.
struct Wait {
    caller: AgentId,
    ids: Vec<String>,
    label: Value,
    deadline: Option<f64>,
    /// Held reports in the order they were first held; a second report from the
    /// same agent replaces the first IN PLACE.
    held: Vec<(String, Held)>,
    timer: Option<tokio::task::AbortHandle>,
}

impl Wait {
    fn covered(&self) -> bool {
        self.ids
            .iter()
            .all(|id| self.held.iter().any(|(h, _)| h == id))
    }

    fn hold(&mut self, from: &str, h: Held) {
        match self.held.iter_mut().find(|(id, _)| id == from) {
            Some(slot) => slot.1 = h,
            None => self.held.push((from.to_string(), h)),
        }
    }
}

/// The per-kernel table of open waits (`kernel.extension("await_set")`).
#[derive(Default)]
pub struct AwaitTable {
    waits: Mutex<BTreeMap<i64, Wait>>,
}

impl AwaitTable {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<i64, Wait>> {
        self.waits.lock().expect("await table poisoned")
    }
}

fn table(kernel: &Kernel) -> Arc<AwaitTable> {
    kernel.extension::<AwaitTable>("await_set")
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn row(kind: &str, fields: Value) -> Row {
    let mut r = Map::new();
    r.insert("kind".to_string(), Value::from(kind));
    if let Value::Object(f) = fields {
        r.extend(f);
    }
    r
}

/// `await_set` — open a wait for the CALLER. Python `_await.await_set`.
pub(crate) async fn await_set(
    kernel: &Arc<Kernel>,
    caller: Option<AgentId>,
    payload: &Value,
) -> Value {
    const VERB: &str = "await_set";
    let Some(caller) = caller else {
        return crate::refusal::bad(
            VERB,
            "caller",
            "an agent — an operator send has nobody to wake",
        );
    };
    let Some(raw) = payload.get("ids").filter(|v| !v.is_null()) else {
        return crate::refusal::missing(VERB, "ids");
    };
    let Some(items) = raw.as_array().filter(|a| !a.is_empty()) else {
        return match raw.as_array() {
            Some(_) => crate::refusal::missing(VERB, "ids"),
            None => crate::refusal::bad(VERB, "ids", "a list of agent ids"),
        };
    };
    let mut ids: Vec<String> = Vec::new();
    for it in items {
        let Some(s) = it.as_str().filter(|s| !s.is_empty()) else {
            return crate::refusal::bad(VERB, "ids", "a list of agent id strings");
        };
        let Some(agent) = kernel.resolve_target(s) else {
            return crate::refusal::unknown_agent(
                s,
                kernel.agents.iter().map(|e| e.key().0.clone()).collect(),
            );
        };
        if agent.id == caller {
            return crate::refusal::bad(VERB, "ids", "absent — an agent cannot await itself");
        }
        if !ids.contains(&agent.id.0) {
            ids.push(agent.id.0.clone());
        }
    }
    let timeout_raw = payload.get("timeout_s").filter(|v| !v.is_null()).cloned();
    let timeout = match payload.get("timeout_s").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v
            .as_f64()
            .filter(|t| v.is_number() && t.is_finite() && *t > 0.0)
        {
            Some(t) => Some(t),
            None => return crate::refusal::bad(VERB, "timeout_s", "a positive number of seconds"),
        },
    };
    let label = match payload.get("label").filter(|v| !v.is_null()) {
        None => Value::Null,
        Some(v) if v.is_string() => v.clone(),
        Some(_) => return crate::refusal::bad(VERB, "label", "a string"),
    };
    let deadline = timeout.map(|t| now_secs() + t);
    let wait_id = kernel.mint_id();
    let open = row(
        "await_open",
        json!({
            "id": wait_id, "wait_id": wait_id, "caller": caller.0,
            "ids": ids, "label": label, "deadline": deadline,
        }),
    );
    if let Err(e) = kernel.rows_put(&caller, open) {
        return crate::executions::raised("RuntimeError", &e);
    }
    let mut w = Wait {
        caller,
        ids: ids.clone(),
        label: label.clone(),
        deadline,
        held: Vec::new(),
        timer: None,
    };
    if let Some(t) = timeout {
        w.timer = Some(arm(kernel, wait_id, std::time::Duration::from_secs_f64(t)));
    }
    table(kernel).lock().insert(wait_id, w);
    // `timeout_s` is echoed AS SENT (60 stays 60, not 60.0), like the python twin
    json!({"wait_id": wait_id, "label": label, "ids": ids, "timeout_s": timeout_raw})
}

/// The timer of one wait: at the deadline it completes the wait as `timed_out`.
fn arm(kernel: &Arc<Kernel>, wait_id: i64, after: std::time::Duration) -> tokio::task::AbortHandle {
    let k = Arc::clone(kernel);
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let taken = table(&k).lock().remove(&wait_id);
        if let Some(mut w) = taken {
            // the timer IS this task: never abort itself
            w.timer = None;
            finish(&k, wait_id, w, true).await;
        }
    })
    .abort_handle()
}

/// `await_cancel` — drop the caller's wait; what it held is delivered one by one.
pub(crate) async fn await_cancel(
    kernel: &Arc<Kernel>,
    caller: Option<AgentId>,
    payload: &Value,
) -> Value {
    let raw = payload.get("wait_id").cloned().unwrap_or(Value::Null);
    let wait_id = match &raw {
        Value::Null | Value::Bool(_) => return crate::refusal::missing("await_cancel", "wait_id"),
        Value::String(s) if s.is_empty() => {
            return crate::refusal::missing("await_cancel", "wait_id")
        }
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    let tbl = table(kernel);
    let taken = wait_id.and_then(|id| {
        let mut t = tbl.lock();
        match t.get(&id) {
            Some(w) if Some(&w.caller) == caller.as_ref() => t.remove(&id),
            _ => None,
        }
    });
    let (Some(wait_id), Some(w)) = (wait_id, taken) else {
        return json!({"cancelled": false, "wait_id": raw, "released": 0});
    };
    if let Some(t) = &w.timer {
        t.abort();
    }
    let _ = kernel.rows_put(
        &w.caller,
        row(
            "await_close",
            json!({"wait_id": wait_id, "how": "cancelled"}),
        ),
    );
    let released = w.held.len();
    if released > 0 {
        let k = Arc::clone(kernel);
        tokio::spawn(redeliver(k, w.caller, w.held));
    }
    json!({"cancelled": true, "wait_id": wait_id, "released": released})
}

/// Deliver each held report to the caller as ITS OWN message, from the agent
/// that sent it, in the order held. The wait is gone, so the hook lets them by.
async fn redeliver(kernel: Arc<Kernel>, caller: AgentId, held: Vec<(String, Held)>) {
    for (from, h) in held {
        let item = Item::new(caller.clone(), h.payload);
        let _ = crate::send::as_agent(&AgentId::from(from.as_str()), kernel.send(vec![item])).await;
    }
}

/// THE HOOK in `dispatch`: is this `report` held for an open wait?
///
/// `Some(reply)` = held (the reporter is answered at once, never blocked on the
/// lead's turn); `None` = dispatch it normally.
pub(crate) fn hold_report(
    kernel: &Arc<Kernel>,
    target: &AgentId,
    origin: Option<&AgentId>,
    payload: &Value,
) -> Option<Value> {
    if payload.get("type").and_then(Value::as_str) != Some("report") {
        return None;
    }
    let origin = origin?;
    let held = Held {
        state: payload.get("state").cloned().unwrap_or(Value::Null),
        result: payload.get("result").cloned().unwrap_or(Value::Null),
        payload: payload.clone(),
    };
    let tbl = table(kernel);
    let (wait_id, full) = {
        let mut t = tbl.lock();
        let (wid, w) = t
            .iter_mut()
            .find(|(_, w)| &w.caller == target && w.ids.contains(&origin.0))?;
        w.hold(&origin.0, held.clone());
        (*wid, w.covered())
    };
    let _ = kernel.rows_put(
        target,
        row(
            "await_hold",
            json!({
                "wait_id": wait_id, "from": origin.0,
                "state": held.state, "result": held.result, "payload": held.payload,
            }),
        ),
    );
    if full {
        // removal is the atomic claim: the timer and the last report cannot both win
        let taken = tbl.lock().remove(&wait_id);
        if let Some(mut w) = taken {
            if let Some(t) = w.timer.take() {
                t.abort();
            }
            let k = Arc::clone(kernel);
            tokio::spawn(async move { finish(&k, wait_id, w, false).await });
        }
    }
    Some(json!({"held": true, "wait_id": wait_id}))
}

/// Close a claimed wait and deliver its ONE combined message.
async fn finish(kernel: &Arc<Kernel>, wait_id: i64, w: Wait, timed_out: bool) {
    // A stopping kernel delivers nothing; the open row stays, so the wait is
    // rebuilt on the next boot.
    if kernel.draining() && !crate::send::in_shutdown() {
        return;
    }
    let mut done: Vec<Value> = Vec::new();
    let mut missing: Vec<Value> = Vec::new();
    for id in &w.ids {
        match w.held.iter().find(|(h, _)| h == id) {
            Some((_, h)) => done.push(json!({"id": id, "state": h.state, "result": h.result})),
            None => missing.push(json!(id)),
        }
    }
    let body = json!({
        "wait_id": wait_id, "label": w.label, "done": done,
        "missing": missing, "timed_out": timed_out,
    });
    let _ = kernel.rows_put(
        &w.caller,
        row(
            "await_close",
            json!({"wait_id": wait_id, "how": if timed_out { "timeout" } else { "done" }}),
        ),
    );
    let Some(agent) = kernel.agents.get(&w.caller).map(|e| Arc::clone(&e)) else {
        return; // the lead left the tree: nobody to wake
    };
    // `text` is what an AI lead reads; the structured fields ride beside it.
    let mut msg = body.as_object().cloned().unwrap_or_default();
    msg.insert("type".to_string(), json!("send"));
    msg.insert("text".to_string(), json!(body.to_string()));
    let _ = kernel.bookkeeping(&agent, Value::Object(msg), None).await;
}

/// Rebuild every wait the store holds an `await_open` for and no `await_close`.
/// Called by `Kernel::start` once the store is wired; returns the waits that are
/// already due (every id held, or the deadline passed) for [`resume`], which runs
/// AFTER the agents booted. A store read error is logged, never fatal.
pub(crate) async fn restore(kernel: &Arc<Kernel>) -> Vec<(i64, bool)> {
    let mut q = Map::new();
    q.insert("kinds".to_string(), json!(KINDS));
    let mut rows = match kernel.store_read(q).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  [kernel] await_set restore failed: {e}");
            Vec::new()
        }
    };
    rows.extend(kernel.rows.all().into_iter().filter(|r| {
        r.get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| KINDS.contains(&k))
    }));
    let mut by_id: BTreeMap<i64, Row> = BTreeMap::new();
    for r in rows {
        if let Some(id) = r.get("id").and_then(Value::as_i64) {
            by_id.insert(id, r);
        }
    }
    let mut waits: BTreeMap<i64, Wait> = BTreeMap::new();
    let mut closed: Vec<i64> = Vec::new();
    for r in by_id.values() {
        let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
        let Some(wid) = r.get("wait_id").and_then(Value::as_i64) else {
            continue;
        };
        match kind {
            "await_open" => {
                let ids: Vec<String> = r
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let Some(caller) = r.get("caller").and_then(Value::as_str) else {
                    continue;
                };
                waits.insert(
                    wid,
                    Wait {
                        caller: AgentId::from(caller),
                        ids,
                        label: r.get("label").cloned().unwrap_or(Value::Null),
                        deadline: r.get("deadline").and_then(Value::as_f64),
                        held: Vec::new(),
                        timer: None,
                    },
                );
            }
            "await_hold" => {
                if let (Some(w), Some(from)) =
                    (waits.get_mut(&wid), r.get("from").and_then(Value::as_str))
                {
                    w.hold(
                        from,
                        Held {
                            state: r.get("state").cloned().unwrap_or(Value::Null),
                            result: r.get("result").cloned().unwrap_or(Value::Null),
                            payload: r.get("payload").cloned().unwrap_or(Value::Null),
                        },
                    );
                }
            }
            "await_close" => closed.push(wid),
            _ => {}
        }
    }
    for wid in closed {
        waits.remove(&wid);
    }
    let mut due: Vec<(i64, bool)> = Vec::new();
    let tbl = table(kernel);
    let now = now_secs();
    for (wid, mut w) in waits {
        if w.covered() {
            due.push((wid, false));
        } else if w.deadline.is_some_and(|d| d <= now) {
            due.push((wid, true));
        } else if let Some(d) = w.deadline {
            w.timer = Some(arm(
                kernel,
                wid,
                std::time::Duration::from_secs_f64(d - now),
            ));
        }
        tbl.lock().insert(wid, w);
    }
    due
}

/// Complete the waits [`restore`] found due; spawned, so the start is not held.
pub(crate) fn resume(kernel: &Arc<Kernel>, due: Vec<(i64, bool)>) {
    for (wid, timed_out) in due {
        let k = Arc::clone(kernel);
        tokio::spawn(async move {
            let taken = table(&k).lock().remove(&wid);
            if let Some(w) = taken {
                finish(&k, wid, w, timed_out).await;
            }
        });
    }
}

#[cfg(test)]
#[path = "await_set/tests.rs"]
mod tests;
