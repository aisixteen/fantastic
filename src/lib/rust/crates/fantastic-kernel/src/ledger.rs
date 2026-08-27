//! The EXECUTION LEDGER — every dispatch is a row, named or not.
//!
//! Port of python `kernel/_agent.py`'s `_ledger_open`/`_record` + `_kernel.py`'s
//! `executions`/`ledger_put` (canonical). `tmp/pychanges.md` #34–#37.
//!
//! A batch item's optional `id` NAMES the execution — ENSURE-semantics, exactly
//! what a supplied `create_agent` id already means, applied to dispatch:
//!
//! - unknown id ⇒ dispatch and record (a name, not a new mechanism)
//! - in flight  ⇒ JOIN — no second run (C10: once-only under concurrency)
//! - settled    ⇒ the recorded outcome returns, the handler never runs
//!   (C11 idempotency; C9 in-process, and across restart once persisted)
//!
//! ONE ID SPACE. The caller's id IS the pid that `ps`/`cancel`/reply tags carry;
//! an UNNAMED execution is ledgered under its minted pid. So any reply is
//! re-fetchable by pid, and there is no second id type to reconcile — a minted
//! pid can never dedup (the caller learns it only after dispatch), a
//! caller-chosen id can.
//!
//! RETENTION IS A RING. Past `cap`, the OLDEST SETTLED rows are evicted,
//! unnamed before named — a named row carries a dedup contract, and evicting it
//! silently re-arms a side effect. In-flight rows are never evicted, so the cap
//! may overshoot while work is live. `None` = unbounded, the operator's explicit
//! choice.
//!
//! THE VIEW IS ORIGIN-SCOPED. Rows record who originated them; an agent sees
//! only its own, the operator sees all. Unscoped, `ledger` would be a
//! side-channel around `callable_by` — reading other agents' result bodies
//! without ever being granted an edge to them.

use crate::agent::AgentId;
use crate::outcome::{Outcome, Status};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::watch;

/// Default ring size. Big enough that a normal session never evicts, bounded so
/// a long-lived kernel cannot grow without limit.
pub const DEFAULT_LEDGER_CAP: usize = 1_000_000;

/// One execution and what it became.
struct Row {
    target: AgentId,
    retries: u32,
    attempts: u32,
    named: bool,
    origin: Option<AgentId>,
    /// `None` while in flight. Set ONCE, at the final outcome — a retrying
    /// execution does not settle its intermediate attempts, so a joiner never
    /// sees an answer that was later retried away.
    outcome: Option<Outcome>,
    /// How joiners wait. `watch` (not `Notify`) because it carries the value and
    /// cannot miss an edge: a joiner that subscribes after the settle still
    /// reads it immediately.
    tx: watch::Sender<Option<Outcome>>,
}

/// What [`Ledger::open`] decided for one item.
pub enum Opened {
    /// A new execution: dispatch it, then call [`Ledger::settle`].
    Fresh,
    /// Someone else is running this id — await their outcome, run nothing.
    Join(watch::Receiver<Option<Outcome>>),
    /// Already settled: this IS the answer, the handler must not run again.
    Settled(Box<Outcome>),
    /// The id is unusable — the refusal to hand back verbatim.
    Refused(Box<Value>),
}

struct Inner {
    rows: HashMap<String, Row>,
    /// Insertion order, for the ring. Mirrors python's dict ordering.
    order: Vec<String>,
    cap: Option<usize>,
}

/// The kernel's execution ledger. Session-lifetime; the durable half is the
/// root loader's job (the kernel never touches disk).
pub struct Ledger {
    inner: Mutex<Inner>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new(Some(DEFAULT_LEDGER_CAP))
    }
}

impl Ledger {
    /// A ledger with `cap` rows of retention (`None` = unbounded).
    #[must_use]
    pub fn new(cap: Option<usize>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                rows: HashMap::new(),
                order: Vec::new(),
                cap,
            }),
        }
    }

    /// Claim `exec_id` for an execution, or discover it is already taken.
    ///
    /// `named` distinguishes a caller-chosen id from a minted pid: only the
    /// former is a dedup contract, and the ring evicts the latter first.
    pub fn open(
        &self,
        exec_id: &str,
        target: &AgentId,
        retries: u32,
        named: bool,
        origin: Option<AgentId>,
    ) -> Opened {
        let mut g = self.inner.lock().expect("ledger poisoned");

        if let Some(row) = g.rows.get(exec_id) {
            // AN ID IS BOUND TO ITS FIRST TARGET. The same id aimed elsewhere is
            // a different intent wearing the same name — returning the other
            // execution's outcome would be silently wrong, so it refuses.
            if &row.target != target {
                let mut r = crate::refusal::refusal(
                    crate::refusal::BAD_ARG,
                    format!(
                        "send: id {exec_id:?} is bound to {} — a named execution \
                         cannot be re-aimed",
                        row.target
                    ),
                );
                crate::refusal::with(&mut r, "arg", json!("id"));
                crate::refusal::with(&mut r, "bound_to", json!(row.target.0));
                return Opened::Refused(Box::new(r));
            }
            return match &row.outcome {
                Some(o) => Opened::Settled(Box::new(o.clone())),
                None => Opened::Join(row.tx.subscribe()),
            };
        }

        // A COLON ID REFERENCES an existing minted execution; it never creates
        // one. The caller cannot mint into the kernel's namespace, so an unknown
        // colon id is a dangling reference, not a new job.
        if exec_id.contains(':') && named {
            let mut r = crate::refusal::refusal(
                crate::refusal::BAD_ARG,
                format!("send: no execution {exec_id:?} — a pid REFERENCES, it never creates"),
            );
            crate::refusal::with(&mut r, "arg", json!("id"));
            return Opened::Refused(Box::new(r));
        }

        let (tx, _rx) = watch::channel(None);
        g.rows.insert(
            exec_id.to_string(),
            Row {
                target: target.clone(),
                retries,
                attempts: 0,
                named,
                origin,
                outcome: None,
                tx,
            },
        );
        g.order.push(exec_id.to_string());
        Self::evict(&mut g);
        Opened::Fresh
    }

    /// Count one dispatch attempt against `exec_id`; returns the new count.
    pub fn attempt(&self, exec_id: &str) -> u32 {
        let mut g = self.inner.lock().expect("ledger poisoned");
        match g.rows.get_mut(exec_id) {
            Some(r) => {
                r.attempts += 1;
                r.attempts
            }
            None => 1,
        }
    }

    /// May this execution be dispatched again? Only a terminal `failed` or
    /// `timed_out` with budget left. `cancelled` is ABSORBING — an operator kill
    /// is a decision, not a transient — and `completed` never retries.
    pub fn should_retry(&self, exec_id: &str, status: Status) -> bool {
        if !status.is_retryable() {
            return false;
        }
        let g = self.inner.lock().expect("ledger poisoned");
        g.rows.get(exec_id).is_some_and(|r| r.attempts <= r.retries)
    }

    /// Record the FINAL outcome and wake every joiner. Stamps `attempts`, so a
    /// caller can see what its work actually cost.
    pub fn settle(&self, exec_id: &str, mut outcome: Outcome) -> Outcome {
        let mut g = self.inner.lock().expect("ledger poisoned");
        if let Some(row) = g.rows.get_mut(exec_id) {
            outcome.attempts = row.attempts.max(1);
            row.outcome = Some(outcome.clone());
            // Ignore the send error: no subscribers is the normal case.
            let _ = row.tx.send(Some(outcome.clone()));
        }
        outcome
    }

    /// Set the retention ring. `None` = unbounded.
    pub fn set_cap(&self, cap: Option<usize>) {
        let mut g = self.inner.lock().expect("ledger poisoned");
        g.cap = cap;
        Self::evict(&mut g);
    }

    /// Rows as the `ledger` verb renders them, oldest first, scoped to `viewer`.
    ///
    /// `viewer` is the ORIGIN of the reading dispatch: `None` = operator, who
    /// sees everything; `Some(id)` = an agent, who sees only what it originated.
    pub fn rows_for(&self, viewer: Option<&AgentId>) -> Vec<Value> {
        let g = self.inner.lock().expect("ledger poisoned");
        g.order
            .iter()
            .filter_map(|id| g.rows.get(id).map(|r| (id, r)))
            .filter(|(_, r)| match viewer {
                None => true,
                Some(v) => r.origin.as_ref() == Some(v),
            })
            .map(|(id, r)| {
                let mut o = json!({
                    "id": id,
                    "target": r.target.0,
                    "status": r.outcome.as_ref().map_or("in_flight", |o| o.status.as_str()),
                    "attempts": r.attempts,
                    "retries": r.retries,
                    "named": r.named,
                });
                if let Some(out) = &r.outcome {
                    o["outcome"] = out.to_value();
                }
                o
            })
            .collect()
    }

    /// Every SETTLED row, as the durable sidecar stores them. In-flight rows are
    /// deliberately absent: work that never completed did not complete, and a
    /// post-reboot re-send re-running it is the truth, not a loss.
    pub fn settled_snapshot(&self) -> Value {
        let g = self.inner.lock().expect("ledger poisoned");
        let mut rows = serde_json::Map::new();
        for id in &g.order {
            let Some(r) = g.rows.get(id) else { continue };
            let Some(out) = &r.outcome else { continue };
            let row = json!({
                "target": r.target.0,
                "retries": r.retries,
                "attempts": r.attempts,
                "named": r.named,
                "origin": r.origin.as_ref().map(|o| o.0.clone()),
                "outcome": out.to_value(),
            });
            // STRICT-JSON FIDELITY: a row that does not survive a round-trip is
            // SKIPPED, not silently mangled. Rehydrating a lie is worse than
            // rehydrating less.
            if serde_json::from_str::<Value>(&row.to_string())
                .ok()
                .as_ref()
                == Some(&row)
            {
                rows.insert(id.clone(), row);
            }
        }
        json!({ "v": 1, "rows": rows })
    }

    /// Restore SETTLED rows from a sidecar, DONE, so dedup holds across a
    /// reboot: a completed step is never re-executed by a fresh process and a
    /// parked dead letter stays dead.
    pub fn rehydrate(&self, doc: &Value) -> usize {
        let Some(rows) = doc.get("rows").and_then(Value::as_object) else {
            return 0;
        };
        let mut g = self.inner.lock().expect("ledger poisoned");
        let mut n = 0;
        for (id, r) in rows {
            let Some(target) = r.get("target").and_then(Value::as_str) else {
                continue;
            };
            let Some(out) = r.get("outcome") else {
                continue;
            };
            let status = match out.get("status").and_then(Value::as_str) {
                Some("completed") => Status::Completed,
                Some("failed") => Status::Failed,
                Some("cancelled") => Status::Cancelled,
                Some("timed_out") => Status::TimedOut,
                _ => continue,
            };
            let outcome = Outcome {
                pid: out
                    .get("pid")
                    .and_then(Value::as_str)
                    .map(crate::tasks::TaskId::from),
                status,
                result: out.get("result").cloned().unwrap_or(Value::Null),
                attempts: out.get("attempts").and_then(Value::as_u64).unwrap_or(1) as u32,
            };
            let (tx, _rx) = watch::channel(Some(outcome.clone()));
            g.rows.insert(
                id.clone(),
                Row {
                    target: AgentId::from(target),
                    retries: r.get("retries").and_then(Value::as_u64).unwrap_or(0) as u32,
                    attempts: r.get("attempts").and_then(Value::as_u64).unwrap_or(1) as u32,
                    named: r.get("named").and_then(Value::as_bool).unwrap_or(true),
                    origin: r.get("origin").and_then(Value::as_str).map(AgentId::from),
                    outcome: Some(outcome),
                    tx,
                },
            );
            g.order.push(id.clone());
            n += 1;
        }
        Self::evict(&mut g);
        n
    }

    /// The ring: evict oldest SETTLED rows, unnamed before named, never
    /// in-flight.
    fn evict(g: &mut Inner) {
        let Some(cap) = g.cap else { return };
        if g.rows.len() <= cap {
            return;
        }
        for named_pass in [false, true] {
            let mut i = 0;
            while g.rows.len() > cap && i < g.order.len() {
                let id = g.order[i].clone();
                let evictable = g
                    .rows
                    .get(&id)
                    .is_some_and(|r| r.outcome.is_some() && r.named == named_pass);
                if evictable {
                    g.rows.remove(&id);
                    g.order.remove(i);
                } else {
                    i += 1;
                }
            }
            if g.rows.len() <= cap {
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "ledger/tests.rs"]
mod tests;
