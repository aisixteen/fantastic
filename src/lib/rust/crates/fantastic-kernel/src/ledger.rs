//! The EXECUTION LEDGER, process side — one entry per execution still in RAM: in
//! flight, or settled and not yet flushed.
//!
//! Port of python `kernel/_kernel.py`'s `executions`/`ledger_put`/`names` (canonical).
//!
//! A batch item's optional `id` NAMES the execution — ENSURE-semantics, exactly
//! what a supplied `create_agent` id already means, applied to dispatch:
//!
//! - unknown id ⇒ dispatch and record (a name, not a new mechanism)
//! - in flight  ⇒ JOIN — no second run (C10: once-only under concurrency)
//! - settled    ⇒ the recorded outcome returns, the handler never runs (C9/C11)
//!
//! A settled execution that was FLUSHED lives on as its `call` + `settle` rows in
//! the kernel's memory store; `send` looks a name up there
//! (`Kernel::recall_executions`) before launching and puts it back here as a DONE
//! entry ([`Ledger::put_settled`]), so dedup survives both the flush and a restart.
//!
//! ONE ID SPACE. The caller's id IS the pid that `ps`/`cancel`/reply tags carry;
//! an UNNAMED execution is ledgered under its minted pid.
//!
//! RETENTION IS A RING in RAM. Past `cap`, the OLDEST SETTLED entries are evicted,
//! unnamed before named. In-flight entries are never evicted. In the store, the
//! same number is the prune window (`Kernel::prune_executions`). `None` = unbounded.
//!
//! THE VIEW IS CALLER-SCOPED. Entries record who originated them (`caller`); the
//! `ledger` verb shows an agent only its own, the operator everything.

use crate::agent::AgentId;
use crate::outcome::{Outcome, Status};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Default ring size. Big enough that a normal session never evicts, bounded so
/// a long-lived kernel cannot grow without limit.
pub const DEFAULT_LEDGER_CAP: usize = 1_000_000;

/// The kernel verbs whose answer is scoped to the caller that sends them
/// (`ledger`, `ack_deadletter`: the executions THAT caller originated). A join of
/// one by any other caller is refused ([`Ledger::open`]); a new caller-scoped verb
/// joins this set. Python `_CALLER_SCOPED_VERBS`.
pub const CALLER_SCOPED_VERBS: [&str; 2] = ["ledger", "ack_deadletter"];

/// The answer to joining another caller's execution of a caller-scoped verb,
/// in python's words (`_foreign_scoped_join`): the caller's raw id as given.
fn foreign_scoped_join(caller_id: &Value, verb: &str) -> Value {
    let mut r = crate::refusal::refusal(
        crate::refusal::BAD_ARG,
        format!(
            "send: id {} names another caller's {verb} execution — {verb} answers \
             only the caller that sent it, so it cannot be joined; make your own \
             {verb} call under a new id",
            fantastic_bundle::py_repr_value(caller_id),
        ),
    );
    crate::refusal::with(&mut r, "arg", json!("id"));
    r
}

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
    /// WHO ORIGINATED it (`None` = the operator) — the `ledger` verb's scope and
    /// the `on_deadletter` event's addressee.
    caller: Option<AgentId>,
    /// `ack_deadletter` retired this dead letter (while it is still in RAM).
    acked: bool,
    /// `None` while in flight. Set ONCE, at the final outcome — a retrying
    /// execution does not settle its intermediate attempts, so a joiner never
    /// sees an answer that was later retried away.
    outcome: Option<Outcome>,
    /// The item's SECRET paths (RAM only — [`crate::secret`]): what the settle
    /// masks the recorded outcome with.
    secret: Option<Arc<crate::secret::Secret>>,
    /// The IDENTITY that opened it (RAM only): the sending agent, the qualified
    /// `<guid>|<agent>` a bridge leg relayed, or `None` for the operator — what a
    /// joiner must BE to receive the real outcome of a secret-marked execution.
    /// Python `entry["origin"]` (`_caller_identity`).
    origin: Option<String>,
    /// The outcome AS SHOWN, for a call with `secret` paths — what the settle
    /// row records and the `ledger` read-back answers. Its original caller
    /// joining in RAM still gets the real [`Row::outcome`]; any other joiner the
    /// shown one ([`Opened`]'s mask). Python `shown_out`.
    shown: Option<Outcome>,
    /// How joiners wait. `watch` (not `Notify`) because it carries the value and
    /// cannot miss an edge: a joiner that subscribes after the settle still
    /// reads it immediately.
    tx: watch::Sender<Option<Outcome>>,
}

/// What [`Ledger::open`] decided for one item.
///
/// A join carries the ORIGINAL call's payload (what the joiner is authorized
/// against), its MASK: the execution's secret marks when it carried some and
/// the joiner is not the identity that opened it — the outcome is then handed
/// over AS SHOWN ([`crate::secret::shown_outcome`]), never the real values;
/// `None` for an unmarked execution, and for its original caller — and its
/// FOREIGN refusal: `Some` when the execution ran a caller-scoped verb
/// ([`CALLER_SCOPED_VERBS`]) and the joiner is not the caller that sent it. The
/// caller answers it once the gate has passed the joiner.
pub enum Opened {
    /// A new execution: dispatch it, then call [`Ledger::settle`].
    Fresh,
    /// Someone else is running this id — await their outcome, run nothing.
    Join(
        watch::Receiver<Option<Outcome>>,
        Value,
        Option<Arc<crate::secret::Secret>>,
        Option<Box<Value>>,
    ),
    /// Already settled: this IS the answer, the handler must not run again.
    Settled(
        Box<Outcome>,
        Value,
        Option<Arc<crate::secret::Secret>>,
        Option<Box<Value>>,
    ),
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

/// The kernel's execution ledger, process side. The durable half is the `call` +
/// `settle` rows the kernel flushes into its memory store.
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
    ///
    /// `identity` is WHO this call is (the sending agent, a relayed
    /// `<guid>|<agent>`, `None` for the operator): a fresh row keeps it as its
    /// origin, a join compares it with the origin. `secret` is the item's marks,
    /// kept beside a FRESH row in the same step that claims it — a concurrent
    /// joiner never sees a marked execution without its marks.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &self,
        exec_id: i64,
        caller_id: &Value,
        target: &AgentId,
        payload: &Value,
        retries: u32,
        name: Option<&str>,
        caller: Option<AgentId>,
        identity: Option<String>,
        secret: Option<crate::secret::Secret>,
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
            // A SECRET-MARKED EXECUTION'S REAL OUTCOME IS ITS CALLER'S ALONE
            // (audit 2026-10-03, F1): any other identity joins it AS SHOWN.
            let mask = row
                .secret
                .as_ref()
                .filter(|_| row.origin != identity)
                .map(Arc::clone);
            // A CALLER-SCOPED VERB'S ANSWER IS ITS CALLER'S VIEW, NOT A FACT
            // (owner-approved 2026-10-03, audit KV-1). `ledger` and
            // `ack_deadletter` answer from WHO sends them — the executions that
            // caller originated (`Row::caller`); a join never re-runs the
            // handler, so the joiner would be handed the original caller's view.
            // Measured: `B`, granted `ledger`, joined `A`'s named ledger read and
            // got A's rows (and the operator's unscoped read by its int pid:
            // every caller's). So a join by any caller other than the one that
            // sent it is REFUSED — the operator included, whose own read is
            // unscoped anyway. Any NEW caller-scoped verb joins
            // `CALLER_SCOPED_VERBS`. Cross-caller joins of every OTHER verb stay
            // open by design: one execution under one id is the once-only
            // contract (a wire frame id IS the execution id), the gate asks the
            // joiner the original call's question, and a secret-marked outcome
            // goes over as shown (`mask`). Python `_agent.py` (the join branch).
            let foreign = row
                .payload
                .get("type")
                .and_then(Value::as_str)
                .filter(|v| CALLER_SCOPED_VERBS.contains(v))
                .filter(|_| row.caller != caller)
                .map(|v| Box::new(foreign_scoped_join(caller_id, v)));
            return match &row.outcome {
                // The joiner is authorized against THIS row's verb by the
                // caller — hand it out with the decision.
                Some(o) => Opened::Settled(Box::new(o.clone()), row.payload.clone(), mask, foreign),
                None => Opened::Join(row.tx.subscribe(), row.payload.clone(), mask, foreign),
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
                caller,
                acked: false,
                outcome: None,
                secret: secret.map(Arc::new),
                origin: identity,
                shown: None,
                tx,
            },
        );
        g.order.push(exec_id);
        Self::evict(&mut g);
        Opened::Fresh
    }

    /// Did the execution held under `exec_id` carry `secret` marks? False when no
    /// row is held in RAM, or when one replayed from the store holds it (that
    /// answers a refusal, never a value). Python `exe.get("secret") is not None`.
    #[must_use]
    pub fn carried_secret(&self, exec_id: i64) -> bool {
        self.inner
            .lock()
            .expect("ledger poisoned")
            .rows
            .get(&exec_id)
            .is_some_and(|r| r.secret.is_some())
    }

    /// The outcome AS RECORDED for a call with secret paths, once settled — what
    /// the settle row keeps. `None` for any other execution.
    pub(crate) fn shown(&self, exec_id: i64) -> Option<Outcome> {
        self.inner
            .lock()
            .expect("ledger poisoned")
            .rows
            .get(&exec_id)
            .and_then(|r| r.shown.clone())
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
    ///
    /// A call with `secret` paths is masked HERE, under the same lock as the
    /// settle (python `_stamp_call` runs in the same synchronous step as the
    /// future's result): no `ledger` read can ever see the real outcome.
    pub fn settle(&self, exec_id: i64, mut outcome: Outcome) -> Outcome {
        let mut g = self.inner.lock().expect("ledger poisoned");
        if let Some(row) = g.rows.get_mut(&exec_id) {
            outcome.attempts = row.attempts.max(1);
            if let Some(s) = &row.secret {
                row.shown = Some(crate::secret::shown_outcome(&outcome, s));
            }
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

    /// The retention cap (`None` = unbounded) — also the store-side prune window.
    #[must_use]
    pub fn cap(&self) -> Option<usize> {
        self.inner.lock().expect("ledger poisoned").cap
    }

    /// Is `exec_id` held AND settled?
    #[must_use]
    pub fn is_done(&self, exec_id: i64) -> bool {
        self.inner
            .lock()
            .expect("ledger poisoned")
            .rows
            .get(&exec_id)
            .is_some_and(|r| r.outcome.is_some())
    }

    /// Every held execution that has NOT settled — what the graceful stop waits
    /// on before its final flush.
    #[must_use]
    pub fn unsettled(&self) -> Vec<i64> {
        let g = self.inner.lock().expect("ledger poisoned");
        g.order
            .iter()
            .filter(|id| g.rows.get(id).is_some_and(|r| r.outcome.is_none()))
            .copied()
            .collect()
    }

    /// `(target, caller, name, named)` of one held execution.
    #[must_use]
    pub fn info(&self, exec_id: i64) -> Option<(AgentId, Option<AgentId>, Option<String>)> {
        let g = self.inner.lock().expect("ledger poisoned");
        g.rows
            .get(&exec_id)
            .map(|r| (r.target.clone(), r.caller.clone(), r.name.clone()))
    }

    /// Mark a held dead letter acknowledged (`ack_deadletter`).
    pub fn mark_acked(&self, exec_id: i64) {
        if let Some(r) = self
            .inner
            .lock()
            .expect("ledger poisoned")
            .rows
            .get_mut(&exec_id)
        {
            r.acked = true;
        }
    }

    /// FORGET one execution — its rows landed in the store, which is its record
    /// now. The name binding goes with it, but only if it still points HERE.
    pub fn forget(&self, exec_id: i64) {
        let mut g = self.inner.lock().expect("ledger poisoned");
        if let Some(name) = g.rows.remove(&exec_id).and_then(|r| r.name) {
            if g.names.get(&name) == Some(&exec_id) {
                g.names.remove(&name);
            }
        }
        g.order.retain(|i| *i != exec_id);
    }

    /// The RAM half of the `ledger` PROJECTION, oldest first — python
    /// `ledger_entries`' overlay loop. Each entry carries its originator as
    /// `_caller` for the verb to scope by; a settled one carries what the settle
    /// row KEEPS (`stored_outcome`) — the same shape flushed or not.
    #[must_use]
    pub fn entries(&self) -> Vec<(i64, Map<String, Value>, bool)> {
        let g = self.inner.lock().expect("ledger poisoned");
        g.order
            .iter()
            .filter_map(|id| g.rows.get(id).map(|r| (*id, r)))
            .map(|(id, r)| {
                let mut e = Map::new();
                e.insert("id".to_string(), json!(id));
                e.insert("target".to_string(), json!(r.target.0));
                e.insert("attempts".to_string(), json!(r.attempts));
                e.insert("retries".to_string(), json!(r.retries));
                e.insert("named".to_string(), json!(r.name.is_some()));
                e.insert(
                    "_caller".to_string(),
                    r.caller.as_ref().map_or(Value::Null, |c| json!(c.0)),
                );
                if let Some(name) = &r.name {
                    e.insert("name".to_string(), json!(name));
                }
                match &r.outcome {
                    Some(real) => {
                        // a call with `secret` paths reads back AS SHOWN
                        let out = r.shown.as_ref().unwrap_or(real);
                        e.insert("status".to_string(), json!(out.status.as_str()));
                        // THE SAME SHAPE flushed or not: what the settle row keeps,
                        // made wire-safe (a raw body reads `<bytes:N>`).
                        let kept =
                            stored_outcome(out, r.name.is_some(), crate::jsonsafe::BodyAs::Marker);
                        let mut oc = Map::new();
                        oc.insert("pid".to_string(), json!(id));
                        if let Value::Object(k) = kept {
                            for (key, v) in k {
                                oc.insert(key, v);
                            }
                        }
                        oc.insert("attempts".to_string(), json!(out.attempts));
                        e.insert("outcome".to_string(), Value::Object(oc));
                    }
                    None => {
                        e.insert("status".to_string(), json!("in_flight"));
                    }
                }
                (id, e, r.acked)
            })
            .collect()
    }

    /// PUT BACK ONE FLUSHED, SETTLED EXECUTION — the durable half of C9, python
    /// `recall_executions`' `ledger_put`. `call` is its `call` row (target at
    /// `agent`, the payload, the retry budget, the name, the caller) and
    /// `outcome` the settle row's `outcome`: a NAMED call's settle kept its
    /// `result`; any other kept only what `stored_outcome` keeps — and the result
    /// says so (the kept fields minus `status`). It comes back DONE, so a re-send
    /// returns the recorded outcome instead of running the handler again.
    ///
    /// A LIVE ENTRY ALWAYS WINS: an id already held is left alone. An unknown
    /// status is not guessed at — nothing is put and `false` comes back.
    pub fn put_settled(
        &self,
        exec_id: i64,
        call: &crate::rows::Row,
        outcome: &Map<String, Value>,
        attempts: u32,
    ) -> bool {
        let status = match outcome.get("status").and_then(Value::as_str) {
            Some("completed") => Status::Completed,
            // A HANDLER THAT SAID NO IS A SETTLED EXECUTION TOO.
            Some("errored") => Status::Errored,
            Some("failed") => Status::Failed,
            Some("cancelled") => Status::Cancelled,
            Some("timed_out") => Status::TimedOut,
            _ => return false,
        };
        let result = match outcome.get("result") {
            Some(r) => r.clone(),
            None => Value::Object(
                outcome
                    .iter()
                    .filter(|(k, _)| k.as_str() != "status")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        };
        self.put_done(exec_id, call, status, result, attempts)
    }

    /// PUT BACK ONE FLUSHED execution that carried SECRET paths — python
    /// `recall_executions`' `call.get("secret")` branch. Its record holds
    /// placeholders, not values, so it cannot be answered from the record: it
    /// comes back DONE holding the `failed` refusal `refusal`, and a re-send gets
    /// that instead of `<secret>` passed off as the value.
    pub(crate) fn put_refused(
        &self,
        exec_id: i64,
        call: &crate::rows::Row,
        refusal: Value,
        attempts: u32,
    ) -> bool {
        self.put_done(exec_id, call, Status::Failed, refusal, attempts)
    }

    /// The one DONE insert both recalls share: `call` is the flushed `call` row.
    fn put_done(
        &self,
        exec_id: i64,
        call: &crate::rows::Row,
        status: Status,
        result: Value,
        attempts: u32,
    ) -> bool {
        let Some(target) = call.get("agent").and_then(Value::as_str) else {
            return false;
        };
        let mut g = self.inner.lock().expect("ledger poisoned");
        if g.rows.contains_key(&exec_id) {
            return false;
        }
        let out = Outcome {
            pid: Some(crate::tasks::TaskId(exec_id)),
            status,
            result,
            attempts,
            // A RECALLED outcome has no binary body: the store records raw bytes
            // as their LENGTH, never the bytes.
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
                // The ORIGINAL call's payload: a joiner of a recalled execution
                // is authorized against exactly that call.
                payload: call.get("payload").cloned().unwrap_or(Value::Null),
                retries: call.get("retries").and_then(Value::as_u64).unwrap_or(0) as u32,
                attempts,
                name,
                caller: call
                    .get("caller")
                    .and_then(Value::as_str)
                    .map(AgentId::from),
                acked: false,
                outcome: Some(out),
                secret: None,
                origin: None,
                shown: None,
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

/// The dead endings — an execution that settled one of these is a DEAD LETTER:
/// announced (`on_deadletter`), listed by `ledger`, never retried silently, and
/// retired by `ack_deadletter`. Python `DEAD_STATUSES`.
#[must_use]
pub fn is_dead(status: Status) -> bool {
    matches!(
        status,
        Status::Failed | Status::TimedOut | Status::Cancelled
    )
}

/// WHAT A SETTLE ROW KEEPS of an outcome — the one definition (python
/// `stored_outcome`, decided 2026-09-26).
///
/// A NAMED call keeps its whole result: its name is a dedup contract, and a
/// re-send returns the recorded outcome (C9). A DEAD LETTER keeps its error text:
/// someone has to read why it died. Every other settle keeps a digest —
/// `{status, bytes}`.
///
/// Why not the body everywhere: the kernel's own store reads are executions too
/// (the `ledger` projection, every AI agent's head read), so a settle holding its
/// body re-recorded the store inside the store — measured ~6x growth per `ledger`
/// call (2 KB → 14.8 MB in five). And a reply body is not persisted anyway: its
/// receiver holds it in RAM with the one-line `origin` that gets it again.
///
/// `body_as` says how a raw reply body is written where python's dict holds the
/// bytes: `{len: N}` for the store (`_store_safe`), `<bytes:N>` for the wire
/// (`_json_safe`).
#[must_use]
pub fn stored_outcome(out: &Outcome, named: bool, body_as: crate::jsonsafe::BodyAs) -> Value {
    let status = Value::from(out.status.as_str());
    let res = crate::jsonsafe::with_body(&out.result, out.body.as_ref(), body_as);
    let mut kept = Map::new();
    kept.insert("status".to_string(), status);
    if named {
        kept.insert("result".to_string(), res);
        return Value::Object(kept);
    }
    if is_dead(out.status) {
        if let Value::Object(m) = &res {
            for k in ["error", "reason", "side", "cancelled", "timed_out"] {
                if let Some(v) = m.get(k) {
                    let v = match v {
                        Value::String(s) => Value::String(s.chars().take(500).collect()),
                        other => other.clone(),
                    };
                    kept.insert(k.to_string(), v);
                }
            }
        }
        return Value::Object(kept);
    }
    // `len(json.dumps(_store_safe(res), default=str))` — python's separators and
    // ASCII escaping, measured over the STORE form (bytes as `{len}`).
    let stored =
        crate::jsonsafe::with_body(&out.result, out.body.as_ref(), crate::jsonsafe::BodyAs::Len);
    let size = fantastic_bundle::py_json_dumps(&stored, true, None).len();
    kept.insert("bytes".to_string(), json!(size));
    Value::Object(kept)
}
