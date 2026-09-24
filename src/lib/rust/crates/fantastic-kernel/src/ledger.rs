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
//!   (C11 idempotency; C9 in-process, and across restart — the ROOT LOADER
//!   rebuilds these rows from the `call`/`settle` rows on `messages.xml`,
//!   the ONE tape; there is no second ledger file)
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
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::watch;

/// Default ring size. Big enough that a normal session never evicts, bounded so
/// a long-lived kernel cannot grow without limit.
pub const DEFAULT_LEDGER_CAP: usize = 1_000_000;

/// One execution and what it became.
struct Row {
    target: AgentId,
    /// The PAYLOAD of the execution this row NAMES. A joiner is handed this
    /// row's outcome verbatim, so it must be authorized for THIS call — the
    /// whole payload, because placement (`create_agent`'s `parent_id`) and
    /// conferral read fields, not just the verb — and not for
    /// whatever it happened to send. Kept here because the row outlives the
    /// dispatch that opened it.
    payload: Value,
    retries: u32,
    attempts: u32,
    /// The caller's NAME for this execution, when it chose one. The row's ID is
    /// the kernel's; a name is a handle the caller may re-send to JOIN. Only a
    /// named row is a dedup contract, and the ring evicts unnamed rows first.
    name: Option<String>,
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
    Join(watch::Receiver<Option<Outcome>>, Value),
    /// Already settled: this IS the answer, the handler must not run again.
    Settled(Box<Outcome>, Value),
    /// The id is unusable — the refusal to hand back verbatim.
    Refused(Box<Value>),
}

struct Inner {
    /// Keyed by the kernel's own id — the ONE counter's number, the same one
    /// `ps`, `cancel`, a reply tag and the message log use.
    rows: HashMap<i64, Row>,
    /// The caller's NAME → that id. The ONLY bridge between the two spaces:
    /// outside callers supply names, the kernel makes ids, and an id arriving
    /// from outside is a reference to check, never a claim.
    names: HashMap<String, i64>,
    /// Insertion order, for the ring. Mirrors python's dict ordering.
    order: Vec<i64>,
    cap: Option<usize>,
}

/// The kernel's execution ledger. Session-lifetime; the durable half is the
/// TAPE — the root loader rebuilds these rows at boot from the `call` + `settle`
/// rows of `messages.xml` (the kernel never touches disk).
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
                names: HashMap::new(),
                order: Vec::new(),
                cap,
            }),
        }
    }

    /// Claim `exec_id` for an execution, or discover it is already taken.
    ///
    /// `exec_id` is the KERNEL's id. `caller_id` is the CALLER's raw id as
    /// given — a string name or an int (C38) — used ONLY to render the rebind
    /// refusal in python's exact words; `name` is `Some` only for the
    /// string-name case (only a named row is a dedup contract, and the ring
    /// evicts the unnamed first).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &self,
        exec_id: i64,
        caller_id: &Value,
        target: &AgentId,
        payload: &Value,
        retries: u32,
        name: Option<&str>,
        origin: Option<AgentId>,
    ) -> Opened {
        let mut g = self.inner.lock().expect("ledger poisoned");

        if let Some(row) = g.rows.get(&exec_id) {
            // AN ID IS BOUND TO ITS FIRST TARGET. The same id aimed elsewhere is
            // a different intent wearing the same name — returning the other
            // execution's outcome would be silently wrong, so it refuses.
            //
            // PYTHON'S PROSE, verbatim (r2 correction: BOTH ledgers render the
            // CALLER's raw id, never the minted exec_id — `py_repr` for a
            // string, bare digits for an int; the target names via `py_repr`
            // too; no `bound_to`).
            if &row.target != target {
                let mut r = crate::refusal::refusal(
                    crate::refusal::BAD_ARG,
                    format!(
                        "send: id {} already names an execution on {}, not {} \
                         — an id is bound to its first execution; a retry \
                         aimed elsewhere needs a new id",
                        fantastic_bundle::py_repr_value(caller_id),
                        fantastic_bundle::py_repr(&row.target.0),
                        fantastic_bundle::py_repr(&target.0),
                    ),
                );
                crate::refusal::with(&mut r, "arg", json!("id"));
                return Opened::Refused(Box::new(r));
            }
            return match &row.outcome {
                // The joiner is authorized against THIS row's verb by the
                // caller — hand it out with the decision.
                Some(o) => Opened::Settled(Box::new(o.clone()), row.payload.clone()),
                None => Opened::Join(row.tx.subscribe(), row.payload.clone()),
            };
        }

        // THE NAME IS THE CALLER'S, THE ID IS THE KERNEL'S. Bind them here, so
        // a later send under the same name JOINS this execution instead of
        // starting a second one.
        let (tx, _rx) = watch::channel(None);
        if let Some(n) = name {
            g.names.insert(n.to_string(), exec_id);
        }
        g.rows.insert(
            exec_id,
            Row {
                target: target.clone(),
                payload: payload.clone(),
                retries,
                attempts: 0,
                name: name.map(str::to_string),
                origin,
                outcome: None,
                tx,
            },
        );
        g.order.push(exec_id);
        Self::evict(&mut g);
        Opened::Fresh
    }

    /// The execution a caller's NAME refers to, if it is still on the ring.
    #[must_use]
    pub fn resolve_name(&self, name: &str) -> Option<i64> {
        self.inner
            .lock()
            .expect("ledger poisoned")
            .names
            .get(name)
            .copied()
    }

    /// Whether a row exists under `exec_id` (named or not, settled or not).
    #[must_use]
    pub fn has(&self, exec_id: i64) -> bool {
        self.inner
            .lock()
            .expect("ledger poisoned")
            .rows
            .contains_key(&exec_id)
    }

    /// Count one dispatch attempt against `exec_id`; returns the new count.
    pub fn attempt(&self, exec_id: i64) -> u32 {
        let mut g = self.inner.lock().expect("ledger poisoned");
        match g.rows.get_mut(&exec_id) {
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
    pub fn should_retry(&self, exec_id: i64, status: Status) -> bool {
        if !status.is_retryable() {
            return false;
        }
        let g = self.inner.lock().expect("ledger poisoned");
        g.rows
            .get(&exec_id)
            .is_some_and(|r| r.attempts <= r.retries)
    }

    /// Record the FINAL outcome and wake every joiner. Stamps `attempts`, so a
    /// caller can see what its work actually cost.
    pub fn settle(&self, exec_id: i64, mut outcome: Outcome) -> Outcome {
        let mut g = self.inner.lock().expect("ledger poisoned");
        if let Some(row) = g.rows.get_mut(&exec_id) {
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
                // NO `verb` KEY. Python's row is exactly {id, target, attempts,
                // retries, named, name?, status, outcome?} — KEY ORDER (C16),
                // and `name` is OMITTED (not `null`) when the execution is
                // unnamed, exactly like python's `if row.get("name"): entry["name"]
                // = …`. The verb is not lost — the TAPE's `call` row carries it,
                // beside the payload; the read verb never needed it.
                let mut o = Map::new();
                o.insert("id".to_string(), json!(id));
                o.insert("target".to_string(), json!(r.target.0));
                o.insert("attempts".to_string(), json!(r.attempts));
                o.insert("retries".to_string(), json!(r.retries));
                o.insert("named".to_string(), json!(r.name.is_some()));
                if let Some(name) = &r.name {
                    o.insert("name".to_string(), json!(name));
                }
                o.insert(
                    "status".to_string(),
                    json!(r
                        .outcome
                        .as_ref()
                        .map_or("in_flight", |out| out.status.as_str())),
                );
                if let Some(out) = &r.outcome {
                    o.insert("outcome".to_string(), out.to_value());
                }
                Value::Object(o)
            })
            .collect()
    }

    /// REBUILD ONE SETTLED EXECUTION FROM THE TAPE — the durable half of C9.
    ///
    /// `call` is the `call` row (it carries the target at `agent`, the retry
    /// budget, the caller's name and the origin) and `outcome` is its `settle`
    /// row's body (`{status, result}`); `attempts` is the settle's final count.
    /// Together they ARE the row a live kernel held, so it comes back DONE and a
    /// re-send of the id returns the recorded outcome instead of running the
    /// handler again — across a restart, out of one file.
    ///
    /// A LIVE ROW ALWAYS WINS: an id already open is left alone. An unknown
    /// status is not guessed at — the row is skipped and `false` comes back.
    pub fn hydrate_settled(
        &self,
        exec_id: i64,
        call: &crate::rows::Row,
        outcome: &Map<String, Value>,
        attempts: u32,
    ) -> bool {
        let Some(target) = call.get("agent").and_then(Value::as_str) else {
            return false;
        };
        let status = match outcome.get("status").and_then(Value::as_str) {
            Some("completed") => Status::Completed,
            // A HANDLER THAT SAID NO IS A SETTLED ROW TOO. Dropping `errored`
            // re-arms exactly the call the ledger exists to never run twice.
            Some("errored") => Status::Errored,
            Some("failed") => Status::Failed,
            Some("cancelled") => Status::Cancelled,
            Some("timed_out") => Status::TimedOut,
            _ => return false,
        };
        let mut g = self.inner.lock().expect("ledger poisoned");
        if g.rows.contains_key(&exec_id) {
            return false;
        }
        // THE LIVE SHAPE, rebuilt: a joiner after a reboot must receive exactly
        // what a joiner before it received — {pid, status, result, attempts}.
        let out = Outcome {
            pid: Some(crate::tasks::TaskId(exec_id)),
            status,
            result: outcome.get("result").cloned().unwrap_or(Value::Null),
            attempts,
            // A REHYDRATED outcome has no binary body: the tape records raw
            // bytes as their LENGTH, never the bytes.
            body: None,
        };
        let (tx, _rx) = watch::channel(Some(out.clone()));
        let name = call.get("name").and_then(Value::as_str).map(str::to_string);
        if let Some(n) = &name {
            g.names.insert(n.clone(), exec_id);
        }
        g.rows.insert(
            exec_id,
            Row {
                target: AgentId::from(target),
                // SETTLED: nothing is left to re-run, so the row keeps no
                // payload. A joiner of a rehydrated row is gated against exactly
                // that — no verb, no fields (python stores `None`).
                payload: Value::Null,
                retries: call.get("retries").and_then(Value::as_u64).unwrap_or(0) as u32,
                attempts,
                name,
                origin: call
                    .get("origin")
                    .and_then(Value::as_str)
                    .map(AgentId::from),
                outcome: Some(out),
                tx,
            },
        );
        g.order.push(exec_id);
        Self::evict(&mut g);
        true
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
                let id = g.order[i];
                let evictable = g
                    .rows
                    .get(&id)
                    .is_some_and(|r| r.outcome.is_some() && r.name.is_some() == named_pass);
                if evictable {
                    // DROP THE NAME BINDING WITH THE ROW — but only if it still
                    // points HERE. A name rebound to a newer execution must not
                    // be orphaned by the eviction of the older one it once named.
                    if let Some(name) = g.rows.remove(&id).and_then(|r| r.name) {
                        if g.names.get(&name) == Some(&id) {
                            g.names.remove(&name);
                        }
                    }
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
