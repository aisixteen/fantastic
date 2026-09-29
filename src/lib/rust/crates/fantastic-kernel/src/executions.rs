//! THE KERNEL'S MEMORY STORE — start, flush, read back.
//!
//! A rule-for-rule port of python `kernel/_kernel.py` (`start`, `_flusher`,
//! `_is_own_flush_row`, `flush_executions`, `prune_executions`, `_store_read`,
//! `ledger_entries`, `recall_executions`, `announce_deadletter`, `mint_id`,
//! `rows_put`), which is canonical.
//!
//! The kernel keeps NO conversation and writes NO tape. Its EXECUTION rows (`call`,
//! `settle`, `task`, `ack`) live in RAM ([`crate::rows::RowStore`]) until the
//! in-kernel FLUSHER writes them to the root's MEMORY STORE (`memory_store_id`) and
//! drops them; `ledger`, dead letters, `ack_deadletter` and named-call dedup read
//! the store back. `kernel_start` refuses to run without a store — no built-in
//! default. The kernel knows nothing about stores beyond their verbs (`read`,
//! `append`, `prune`): a store is an ordinary agent, reached by ordinary sends.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use fantastic_bundle::{py_repr, py_str, py_truthy};
use serde_json::{json, Map, Value};

use crate::agent::AgentId;
use crate::kernel::Kernel;
use crate::outcome::{Item, Outcome, Status};
use crate::rows::Row;
use crate::tasks::TaskId;

/// Seconds of quiet before a batch is written — python `FLUSH_DEBOUNCE`.
const FLUSH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// Rows per store page when the kernel reads its own key back.
const READ_PAGE: i64 = 1000;

#[derive(Default)]
struct Inner {
    /// THE MEMORY STORE the kernel writes its execution rows to — the root
    /// record's `memory_store_id`, resolved by `start`. `None` ⇒ not started:
    /// rows stay in RAM.
    store_id: Option<AgentId>,
    started: bool,
    flusher_task: Option<TaskId>,
    /// The flush rows this kernel wrote (by id), and the calls that carried
    /// them: those calls' own rows are ALREADY RECORDED — the flush row is their
    /// record.
    own_flushes: HashSet<i64>,
    own_flush_calls: HashMap<i64, i64>,
    /// How many `call` rows the store holds for the kernel (counted as flushed,
    /// re-read at every prune) — what the retention window is measured against.
    stored_calls: usize,
}

/// The kernel's flusher state. One per kernel.
pub struct ExecState {
    inner: std::sync::Mutex<Inner>,
    wake: tokio::sync::Notify,
    lock: tokio::sync::Mutex<()>,
}

impl Default for ExecState {
    fn default() -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner::default()),
            wake: tokio::sync::Notify::new(),
            lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl ExecState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("exec state poisoned")
    }
}

/// A refusal-shaped raise — python's dispatch records an exception out of a verb
/// as `{"error": "<Class>: <message>", "raised": true}` and settles it `failed`.
pub(crate) fn raised(class: &str, message: &str) -> Value {
    json!({"error": format!("{class}: {message}"), "raised": true})
}

fn status_of(row: &Row) -> Option<&str> {
    row.get("outcome")
        .and_then(Value::as_object)
        .and_then(|o| o.get("status"))
        .and_then(Value::as_str)
}

fn kind_of(row: &Row) -> Option<&str> {
    row.get("kind").and_then(Value::as_str)
}

impl Kernel {
    /// The next message id. The ONLY allocator in the kernel: a call, a forked
    /// task, a conversation row, a checkpoint all take their id here, so one
    /// number names one thing everywhere (`ps`, `cancel`, a reply tag, a store
    /// row). Reserves a new block in the root record when the old one is spent —
    /// once the kernel is STARTED (before that the bound lives in memory only).
    pub fn mint_id(&self) -> i64 {
        let (n, bound) = self.rows.mint();
        self.persist_bound(bound);
        n
    }

    fn persist_bound(&self, bound: Option<i64>) {
        let Some(bound) = bound else { return };
        if !self.started() {
            return;
        }
        if let Some(root) = self.root() {
            let mut patch = Map::new();
            patch.insert("next_id".to_string(), Value::from(bound));
            self.update(&root.id, patch);
        }
    }

    /// Append one EXECUTION row (`agent` = the agent it is about); mint its id
    /// when absent, accept a PRE-MINTED unused one, refuse anything else — the
    /// kernel is the only allocator. Bookkeeping from above, never gated. Wakes
    /// the flusher. Returns the id.
    pub fn rows_put(&self, agent: &AgentId, row: Row) -> Result<i64, String> {
        let (rid, bound) = self.rows.put(agent, row)?;
        self.persist_bound(bound);
        self.exec.wake.notify_one();
        Ok(rid)
    }

    /// Has `kernel_start` run?
    #[must_use]
    pub fn started(&self) -> bool {
        self.exec.lock().started
    }

    /// The memory store the kernel flushes into, once started.
    #[must_use]
    pub fn store_id(&self) -> Option<AgentId> {
        self.exec.lock().store_id.clone()
    }

    /// Set `ledger_cap` — the RAM ring AND the store-side prune window. `None` =
    /// unbounded (the operator's explicit choice).
    pub fn set_ledger_cap(&self, cap: Option<usize>) {
        self.ledger.set_cap(cap);
    }

    /// One send as the OPERATOR — the kernel doing its own bookkeeping.
    ///
    /// BOXED: `send` reaches the store through here (named-call recall reads it
    /// before launching), and the store reply comes back through `send`, so the
    /// future is recursive in terms of itself.
    fn operator_send(
        self: &Arc<Self>,
        target: &AgentId,
        payload: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Outcome> + Send>> {
        let k = Arc::clone(self);
        let item = Item::new(target.clone(), payload);
        Box::pin(crate::send::with_operator(async move {
            k.send(vec![item]).await.remove(0)
        }))
    }

    /// THE START SEQUENCE — the `kernel_start` verb, and what the daemon runs.
    ///
    /// 1. The root record must name `memory_store_id`. None ⇒ FAIL LOUD: the
    ///    kernel keeps its execution rows (the ledger, dead letters, named-call
    ///    dedup) in a memory store and does not start without one. No built-in
    ///    default.
    /// 2. Boot the root (it mints the guid every store key carries), then THAT
    ///    store — before anyone who might call it.
    /// 3. The store must serve the operator (the kernel writes as `kernel`) —
    ///    probed now, refused now, never discovered on the first flush.
    /// 4. Start flushing: every execution row minted so far and from now on is
    ///    written into the store and leaves RAM.
    /// 5. `boot_all`: boot every other agent in registration order (AI agents
    ///    boot lazily — they read their own store on their first turn).
    ///
    /// Idempotent: a second call reports the store it is already writing to.
    pub async fn start(self: &Arc<Self>, boot_all: bool) -> Value {
        {
            let st = self.exec.lock();
            if st.started {
                return json!({"started": true, "already": true, "store": st.store_id.as_ref().map(|s| s.0.clone())});
            }
        }
        let Some(root) = self.root() else {
            return json!({"error": "kernel: no root agent registered"});
        };
        let sid = root
            .meta
            .read()
            .expect("meta poisoned")
            .get("memory_store_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let Some(sid) = sid else {
            let mut r = crate::refusal::refusal(
                crate::refusal::MISSING_ARG,
                "kernel_start: the root record names no `memory_store_id` — the kernel \
                 keeps its execution rows (ledger, dead letters, named-call dedup) in a \
                 memory store and will not start without one",
            );
            crate::refusal::with(
                &mut r,
                "hint",
                Value::String(format!(
                    "a memory store is needed — create a ram_store and name it in \
                     memory_store_id if you have none: `create_agent parent_id={} \
                     handler_module=ram_store.tools id=memory operator=true`, then \
                     `update_agent id={} memory_store_id=memory`",
                    root.id, root.id
                )),
            );
            crate::refusal::with(&mut r, "arg", json!("memory_store_id"));
            return r;
        };
        let Some(store) = self
            .agents
            .get(&AgentId::from(sid.as_str()))
            .map(|e| Arc::clone(&e))
        else {
            let mut r = crate::refusal::unknown_agent(
                &sid,
                self.agents.iter().map(|e| e.key().0.clone()).collect(),
            );
            crate::refusal::with(
                &mut r,
                "hint",
                json!("the root's `memory_store_id` names no agent in this tree"),
            );
            return r;
        };
        let mut booted: Vec<String> = Vec::new();
        for a in [&root, &store] {
            let out = self.operator_send(&a.id, json!({"type": "boot"})).await;
            if out.status != Status::Completed {
                return crate::refusal::refusal(
                    crate::refusal::UNHANDLED,
                    format!(
                        "kernel_start: boot of {} did not complete ({}): {}",
                        py_repr(&a.id.0),
                        out.status.as_str(),
                        py_str(&out.result)
                    ),
                );
            }
            booted.push(a.id.0.clone());
        }
        let probe = self
            .operator_send(&store.id, json!({"type": "read", "limit": 1}))
            .await
            .result;
        if let Some(err) = probe.get("error").filter(|e| py_truthy(e)) {
            let hint = probe
                .get("hint")
                .filter(|h| py_truthy(h))
                .map(py_str)
                .unwrap_or_else(|| {
                    format!(
                        "the store must serve the operator: update_agent id={} operator=true",
                        store.id
                    )
                });
            return crate::refusal::refusal_hint(
                crate::refusal::UNAUTHORIZED,
                format!(
                    "kernel_start: the memory store {} refuses the kernel: {}",
                    py_repr(&store.id.0),
                    py_str(err)
                ),
                hint,
            );
        }
        // The reserved id bound goes to the loader now (the root is booted, so its
        // record writer is listening): a crash from here on skips ids, never reuses.
        {
            let mut patch = Map::new();
            patch.insert("next_id".to_string(), Value::from(self.rows.id_reserved()));
            self.update(&root.id, patch);
        }
        {
            let mut st = self.exec.lock();
            st.store_id = Some(store.id.clone());
            st.started = true;
        }
        let k = Arc::clone(self);
        let tid = self.spawn_keepalive_task(&root.id, "flush_executions", async move {
            k.flusher().await;
        });
        self.exec.lock().flusher_task = Some(tid);
        self.exec.wake.notify_one();
        if boot_all {
            for id in self.agent_ids() {
                if id == root.id || id == store.id {
                    continue;
                }
                let out = self.operator_send(&id, json!({"type": "boot"})).await;
                let refused = out.result.get("error").is_some_and(py_truthy);
                if out.status == Status::Completed && !refused {
                    booted.push(id.0.clone());
                } else {
                    eprintln!(
                        "  [kernel] boot {}: {} {}",
                        py_repr(&id.0),
                        out.status.as_str(),
                        py_str(&out.result)
                    );
                }
            }
        }
        json!({"started": true, "store": store.id.0, "booted": booted})
    }

    /// The flush loop: wake on a new row, let a burst settle, write a batch.
    async fn flusher(self: Arc<Self>) {
        loop {
            self.exec.wake.notified().await;
            tokio::time::sleep(FLUSH_DEBOUNCE).await;
            let _ = self.flush_executions().await;
        }
    }

    /// A row the kernel's own flush produced — ALREADY RECORDED by its flush row.
    ///
    /// A flush is a `send` to the store, so it mints a `call` row and a `settle`
    /// row like every send; writing those would mint two more on the next flush,
    /// forever. The flush therefore describes ITSELF: it mints one id and appends
    /// a `flush` row inside the very batch it writes. The call that carried that
    /// batch names the flush row in its payload — the store already holds the
    /// record of this execution — so its call and settle rows are not written
    /// again. This is dedup by execution (the store proves it), not a blind spot:
    /// every OTHER call to the store, the kernel's own reads included, is recorded
    /// like any call.
    fn is_own_flush_row(&self, st: &mut Inner, row: &Row) -> bool {
        let kind = kind_of(row);
        let store = st.store_id.as_ref().map(|s| s.0.as_str());
        if kind == Some("call") && row.get("agent").and_then(Value::as_str) == store {
            let payload = row.get("payload");
            if payload.and_then(|p| p.get("type")).and_then(Value::as_str) == Some("append") {
                if let Some(rows) = payload
                    .and_then(|p| p.get("rows"))
                    .and_then(Value::as_array)
                {
                    for r in rows {
                        if r.get("kind").and_then(Value::as_str) != Some("flush") {
                            continue;
                        }
                        let Some(fid) = r.get("id").and_then(Value::as_i64) else {
                            continue;
                        };
                        if st.own_flushes.contains(&fid) {
                            if let Some(cid) = row.get("id").and_then(Value::as_i64) {
                                st.own_flush_calls.insert(cid, fid);
                            }
                            return true;
                        }
                    }
                }
            }
        }
        if kind == Some("settle") {
            if let Some(p) = row.get("parent").and_then(Value::as_i64) {
                return st.own_flush_calls.contains_key(&p);
            }
        }
        false
    }

    /// Write every execution row still in RAM to the memory store, as ONE batch
    /// closed by a self-describing `flush` row; on success the rows leave RAM and a
    /// settled execution whose settle row landed leaves the ledger (the store is
    /// its record now). All or nothing: a refused batch stays in RAM for the next
    /// flush, and the refused flush is itself a dead letter.
    ///
    /// EDGE (recorded, not resolved): the write can LAND while its reply is lost
    /// (a timeout). The store then holds the flush row as `done` while RAM saw the
    /// flush call fail — that call is a dead letter in RAM (the ledger shows it,
    /// `on_deadletter` announced it), and its own rows are not re-flushed because
    /// the store already recorded them. RAM and store disagree about THAT flush
    /// only; the admin reconciles.
    pub async fn flush_executions(self: &Arc<Self>) -> Value {
        let Some(store_id) = self.store_id() else {
            return json!({"flushed": 0});
        };
        let guard = self.exec.lock.lock().await;
        // The PREVIOUS flush's own call + settle: already recorded by its flush row.
        {
            let mut st = self.exec.lock();
            for rid in self.rows.ids() {
                let Some(row) = self.rows.get_one(rid) else {
                    continue;
                };
                if !self.is_own_flush_row(&mut st, &row) {
                    continue;
                }
                self.rows.remove(rid);
                if kind_of(&row) == Some("settle") {
                    // The flush call ended: its bookkeeping goes. A flush that did
                    // not complete stays in the ledger — a dead letter in RAM.
                    if let Some(parent) = row.get("parent").and_then(Value::as_i64) {
                        if let Some(fid) = st.own_flush_calls.remove(&parent) {
                            st.own_flushes.remove(&fid);
                        }
                        if status_of(&row) == Some("completed") {
                            self.ledger.forget(parent);
                        }
                    }
                }
            }
        }
        let ids = self.rows.ids();
        if ids.is_empty() {
            return json!({"flushed": 0});
        }
        let fid = self.mint_id();
        self.exec.lock().own_flushes.insert(fid);
        let mut kinds: HashMap<i64, Option<String>> = HashMap::new();
        let mut batch: Vec<Value> = Vec::with_capacity(ids.len() + 1);
        for i in &ids {
            if let Some(r) = self.rows.get_one(*i) {
                kinds.insert(*i, kind_of(&r).map(str::to_string));
                batch.push(Value::Object(r));
            }
        }
        let mut flush = Map::new();
        flush.insert("id".to_string(), Value::from(fid));
        flush.insert("kind".to_string(), Value::from("flush"));
        flush.insert(
            "covers".to_string(),
            Value::Array(ids.iter().map(|i| Value::from(*i)).collect()),
        );
        flush.insert("status".to_string(), Value::from("done"));
        flush.insert("ts".to_string(), Value::from(now_secs()));
        batch.push(Value::Object(flush));
        let mut payload = Map::new();
        payload.insert("type".to_string(), Value::from("append"));
        payload.insert("rows".to_string(), Value::Array(batch));
        let out = self.operator_send(&store_id, Value::Object(payload)).await;
        let refused = out.result.get("error").is_some_and(py_truthy);
        if out.status != Status::Completed || refused {
            eprintln!(
                "  [kernel] flush of {} execution row(s) to {} failed: {} {}",
                ids.len(),
                py_repr(&store_id.0),
                out.status.as_str(),
                py_str(&out.result)
            );
            return json!({"flushed": 0, "error": out.result});
        }
        for i in &ids {
            let Some(r) = self.rows.remove(*i) else {
                continue;
            };
            if kind_of(&r) == Some("settle") {
                if let Some(parent) = r.get("parent").and_then(Value::as_i64) {
                    if self.ledger.is_done(parent) {
                        self.ledger.forget(parent);
                    }
                }
            }
        }
        let calls = ids
            .iter()
            .filter(|i| kinds.get(i).and_then(Option::as_deref) == Some("call"))
            .count();
        let over = {
            let mut st = self.exec.lock();
            st.stored_calls += calls;
            self.ledger.cap().is_some_and(|cap| st.stored_calls > cap)
        };
        drop(guard);
        let mut pruned = 0;
        if over {
            pruned = self.prune_executions().await;
        }
        json!({"flushed": ids.len(), "flush": fid, "pruned": pruned})
    }

    /// THE STORE-SIDE RETENTION WINDOW (`ledger_cap`). Over the cap, the OLDEST
    /// settled executions leave the kernel's store — unnamed first, then named (a
    /// named one carries a dedup contract, so it goes last) — each with its settle
    /// and ack rows, and the task/flush rows that only covered what was pruned. A
    /// DEAD LETTER that nobody acked stays: it is waiting for a person. In-flight
    /// work is never pruned. Returns how many executions went.
    pub async fn prune_executions(self: &Arc<Self>) -> usize {
        let Some(store_id) = self.store_id() else {
            return 0;
        };
        let rows = match self.store_read(Map::new()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  [kernel] prune of the kernel's store failed: {e}");
                return 0;
            }
        };
        let calls: Vec<&Row> = rows
            .iter()
            .filter(|r| kind_of(r) == Some("call") && !r.get("internal").is_some_and(py_truthy))
            .collect();
        let mut settles: HashMap<i64, &Row> = HashMap::new();
        let mut acked: HashSet<i64> = HashSet::new();
        for r in &rows {
            let parent = r.get("parent").and_then(Value::as_i64);
            match (kind_of(r), parent) {
                (Some("settle"), Some(p)) => {
                    settles.insert(p, r);
                }
                (Some("ack"), Some(p)) => {
                    acked.insert(p);
                }
                _ => {}
            }
        }
        self.exec.lock().stored_calls = calls.len();
        let cap = self.ledger.cap().unwrap_or(0);
        if calls.len() <= cap {
            return 0;
        }
        // HYSTERESIS: down to 90% of the window, so a full store is not re-read and
        // re-pruned on every flush once it sits at the cap.
        let floor_to = if cap >= 10 { cap * 9 / 10 } else { cap };
        let over = calls.len() - floor_to;
        let prunable = |c: &Row| -> bool {
            let Some(cid) = c.get("id").and_then(Value::as_i64) else {
                return false;
            };
            let Some(st) = settles.get(&cid) else {
                return false; // in flight
            };
            if self.ledger.info(cid).is_some() {
                return false; // still live in RAM
            }
            let dead = matches!(status_of(st), Some("failed" | "timed_out" | "cancelled"));
            !dead || acked.contains(&cid)
        };
        let mut sorted: Vec<&Row> = calls.clone();
        sorted.sort_by_key(|r| r.get("id").and_then(Value::as_i64).unwrap_or(0));
        let mut doomed_calls: Vec<i64> = Vec::new();
        for named_pass in [false, true] {
            for c in &sorted {
                if doomed_calls.len() >= over {
                    break;
                }
                if c.get("named").is_some_and(py_truthy) == named_pass && prunable(c) {
                    if let Some(id) = c.get("id").and_then(Value::as_i64) {
                        doomed_calls.push(id);
                    }
                }
            }
        }
        if doomed_calls.is_empty() {
            return 0;
        }
        let gone: HashSet<i64> = doomed_calls.iter().copied().collect();
        let mut doomed: HashSet<i64> = gone.clone();
        for r in &rows {
            if matches!(kind_of(r), Some("settle" | "ack"))
                && r.get("parent")
                    .and_then(Value::as_i64)
                    .is_some_and(|p| gone.contains(&p))
            {
                if let Some(id) = r.get("id").and_then(Value::as_i64) {
                    doomed.insert(id);
                }
            }
        }
        let floor = calls
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_i64))
            .filter(|id| !gone.contains(id))
            .min();
        for r in &rows {
            let Some(rid) = r.get("id").and_then(Value::as_i64) else {
                continue;
            };
            match kind_of(r) {
                Some("flush") => {
                    let covers: Vec<i64> = r
                        .get("covers")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_i64).collect())
                        .unwrap_or_default();
                    let all_gone = covers
                        .iter()
                        .all(|x| doomed.contains(x) || floor.is_some_and(|f| *x < f));
                    if all_gone {
                        doomed.insert(rid);
                    }
                }
                Some("task") if floor.is_some_and(|f| rid < f) => {
                    doomed.insert(rid);
                }
                _ => {}
            }
        }
        let mut ids: Vec<i64> = doomed.into_iter().collect();
        ids.sort_unstable();
        let out = self
            .operator_send(&store_id, json!({"type": "prune", "ids": ids}))
            .await
            .result;
        if out.get("error").is_some_and(py_truthy) {
            eprintln!(
                "  [kernel] prune of the kernel's store failed: {}",
                py_str(&out)
            );
            return 0;
        }
        let mut st = self.exec.lock();
        st.stored_calls = st.stored_calls.saturating_sub(gone.len());
        gone.len()
    }

    /// Every row of the kernel's own key matching `payload`, all pages.
    pub(crate) async fn store_read(
        self: &Arc<Self>,
        payload: Map<String, Value>,
    ) -> Result<Vec<Row>, String> {
        let Some(store_id) = self.store_id() else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Row> = Vec::new();
        let mut after: Option<Value> = None;
        loop {
            let mut q = Map::new();
            q.insert("type".to_string(), Value::from("read"));
            for (k, v) in &payload {
                q.insert(k.clone(), v.clone());
            }
            q.insert("limit".to_string(), Value::from(READ_PAGE));
            if let Some(a) = &after {
                q.insert("after".to_string(), a.clone());
            }
            let res = self.operator_send(&store_id, Value::Object(q)).await.result;
            let ok = res.is_object() && !res.get("error").is_some_and(py_truthy);
            if !ok {
                return Err(format!(
                    "kernel: memory store read failed: {}",
                    py_str(&res)
                ));
            }
            if let Some(rows) = res.get("rows").and_then(Value::as_array) {
                out.extend(rows.iter().filter_map(|r| r.as_object().cloned()));
            }
            match res.get("next") {
                Some(n) if !n.is_null() => after = Some(n.clone()),
                _ => return Ok(out),
            }
        }
    }

    /// The LEDGER PROJECTION: flushed `call`/`settle`/`ack` rows from the store,
    /// overlaid by what is still in RAM (live executions, unflushed rows). Each
    /// entry carries its originator as `_caller` for the verb to scope by.
    pub(crate) async fn ledger_entries(
        self: &Arc<Self>,
    ) -> Result<Vec<Map<String, Value>>, String> {
        let mut entries: BTreeMap<i64, Map<String, Value>> = BTreeMap::new();
        let mut acked: HashSet<i64> = HashSet::new();
        let mut rows: Vec<Row> = Vec::new();
        if self.store_id().is_some() {
            let mut q = Map::new();
            q.insert("kinds".to_string(), json!(["call", "settle", "ack"]));
            rows = self.store_read(q).await?;
        }
        rows.extend(
            self.rows
                .all()
                .into_iter()
                .filter(|r| matches!(kind_of(r), Some("call" | "settle" | "ack"))),
        );
        let mut settles: HashMap<i64, Row> = HashMap::new();
        for r in &rows {
            let parent = r.get("parent").and_then(Value::as_i64);
            match (kind_of(r), parent) {
                (Some("ack"), Some(p)) => {
                    acked.insert(p);
                }
                (Some("settle"), Some(p)) => {
                    settles.insert(p, r.clone());
                }
                _ => {}
            }
        }
        for r in &rows {
            if kind_of(r) != Some("call") || r.get("internal").is_some_and(py_truthy) {
                continue;
            }
            let Some(eid) = r.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let mut e = Map::new();
            e.insert("id".to_string(), Value::from(eid));
            e.insert(
                "target".to_string(),
                r.get("agent").cloned().unwrap_or(Value::Null),
            );
            let attempts = r.get("attempts").cloned().unwrap_or(Value::from(1));
            e.insert("attempts".to_string(), attempts.clone());
            e.insert(
                "retries".to_string(),
                r.get("retries").cloned().unwrap_or(Value::from(0)),
            );
            e.insert(
                "named".to_string(),
                Value::from(r.get("named").is_some_and(py_truthy)),
            );
            e.insert(
                "_caller".to_string(),
                r.get("caller").cloned().unwrap_or(Value::Null),
            );
            if let Some(n) = r.get("name").filter(|n| py_truthy(n)) {
                e.insert("name".to_string(), n.clone());
            }
            match settles.get(&eid) {
                Some(st) => {
                    let oc = st
                        .get("outcome")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    e.insert(
                        "status".to_string(),
                        oc.get("status").cloned().unwrap_or(Value::Null),
                    );
                    let att = st
                        .get("attempts")
                        .filter(|a| py_truthy(a))
                        .cloned()
                        .unwrap_or(attempts);
                    e.insert("attempts".to_string(), att.clone());
                    let mut outcome = Map::new();
                    outcome.insert("pid".to_string(), Value::from(eid));
                    for (k, v) in oc {
                        outcome.insert(k, v);
                    }
                    outcome.insert("attempts".to_string(), att);
                    e.insert("outcome".to_string(), Value::Object(outcome));
                }
                None => {
                    e.insert("status".to_string(), Value::from("in_flight"));
                }
            }
            entries.insert(eid, e);
        }
        for (eid, e, was_acked) in self.ledger.entries() {
            if was_acked {
                acked.insert(eid);
            }
            entries.insert(eid, e);
        }
        for eid in acked {
            if let Some(e) = entries.get_mut(&eid) {
                e.insert("acked".to_string(), Value::Bool(true));
            }
        }
        Ok(entries.into_values().collect())
    }

    /// Bring a flushed execution back into RAM before `send` launches an item that
    /// names it — a caller NAME not held in RAM, or an int id not in the ledger. A
    /// settled one comes back as a DONE entry holding its recorded outcome (dedup
    /// survives the flush and a restart); one with no settle row was in flight when
    /// the process stopped and rebuilds nothing — re-sending runs it.
    pub(crate) async fn recall_executions(self: &Arc<Self>, items: &[Item]) -> Result<(), String> {
        enum Want {
            Name(String),
            Id(i64),
        }
        let mut wanted: Vec<Want> = Vec::new();
        for it in items {
            let raw = it
                .raw_id
                .clone()
                .or_else(|| it.id.clone().map(Value::String));
            match raw {
                Some(Value::String(s)) if !s.is_empty() => {
                    if self.ledger.resolve_name(&s).is_none() {
                        wanted.push(Want::Name(s));
                    }
                }
                Some(Value::Number(n)) => {
                    if let Some(x) = n.as_i64() {
                        if !self.ledger.has(x) {
                            wanted.push(Want::Id(x));
                        }
                    }
                }
                _ => {}
            }
        }
        for w in wanted {
            let mut q = Map::new();
            q.insert("kinds".to_string(), json!(["call"]));
            match &w {
                Want::Name(n) => {
                    q.insert("match".to_string(), json!({"name": n}));
                }
                Want::Id(i) => {
                    q.insert("ids".to_string(), json!([i]));
                }
            }
            let calls = self.store_read(q).await?;
            let Some(call) = calls.last() else { continue };
            let Some(cid) = call.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let mut q = Map::new();
            q.insert("kinds".to_string(), json!(["settle"]));
            q.insert("match".to_string(), json!({"parent": cid}));
            let settles = self.store_read(q).await?;
            let Some(st) = settles.last() else { continue };
            let oc = st
                .get("outcome")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let attempts = st
                .get("attempts")
                .filter(|a| py_truthy(a))
                .or_else(|| call.get("attempts").filter(|a| py_truthy(a)))
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32;
            self.ledger.put_settled(cid, call, &oc, attempts);
        }
        Ok(())
    }

    /// `on_deadletter` — an execution settled failed / timed_out / cancelled.
    /// Emitted on the ORIGINATING caller's own inbox (and so to whoever watches
    /// it, a UI included), or on the root for an operator send. Nothing retries
    /// it; the event is how a person learns there is something to decide.
    pub(crate) async fn announce_deadletter(
        self: &Arc<Self>,
        pid: i64,
        out: &Outcome,
        attempts: u32,
    ) {
        let (target, caller, name) = match self.ledger.info(pid) {
            Some((t, c, n)) => (Some(t), c, n),
            None => (None, None, None),
        };
        let mut entry = Map::new();
        entry.insert("type".to_string(), Value::from("on_deadletter"));
        entry.insert("id".to_string(), Value::from(pid));
        entry.insert(
            "target".to_string(),
            target.map_or(Value::Null, |t| Value::from(t.0)),
        );
        entry.insert(
            "caller".to_string(),
            caller
                .as_ref()
                .map_or(Value::Null, |c| Value::from(c.0.clone())),
        );
        entry.insert("status".to_string(), Value::from(out.status.as_str()));
        entry.insert("attempts".to_string(), Value::from(attempts));
        if let Some(n) = name {
            entry.insert("name".to_string(), Value::from(n));
        }
        let src = caller
            .filter(|c| self.agents.contains_key(c))
            .or_else(|| self.root().map(|r| r.id.clone()));
        if let Some(src) = src {
            self.emit(&src, Value::Object(entry)).await;
        }
    }

    /// Stop the flusher (the graceful stop's last act on the store).
    pub(crate) async fn stop_flusher(self: &Arc<Self>) {
        let tid = self.exec.lock().flusher_task.take();
        if let Some(tid) = tid {
            let _ = self.cancel_task(&tid).await;
        }
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
