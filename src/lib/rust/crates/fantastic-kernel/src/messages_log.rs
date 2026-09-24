//! The message store on disk — cold hydration and the append pump.
//!
//! A port of python's `kernel_state/tools.py` (`read_messages`,
//! `hydrate_messages`, `_MessagePump`), which is canonical. The format is
//! [`crate::messages_xml`].
//!
//! THE KERNEL NEVER KNOWS DISK, here as there. It starts empty, always: this
//! module reads the log FROM ABOVE and fills the store with `rows.put`, and the
//! pump tails `rows.subscribe` and appends. Both are optional — no store wired
//! means the log stays in RAM, which is the same law records live under, not a
//! fallback.
//!
//! ORDER, BY CONTRACT: hydrate (cold, before any agent exists) → subscribe at
//! the highest id present → only then create children. Subscribing before
//! hydrating would re-append what was just read; creating children before
//! subscribing would lose their first messages. Rust gets the second half for
//! free — `subscribe(after)` REPLAYS everything above the cursor, so capturing
//! `after` at hydration time covers anything minted before the pump starts.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::messages_xml::{parse_messages, row_to_xml, HEADER};
use crate::persistence::find_store;
use crate::rows::Row;
use crate::{AgentId, Kernel};

/// The log, under the workdir's `.fantastic/`.
pub const MESSAGES_FILE: &str = "messages.xml";

/// Debounce before a flush, matching python's pump: a burst of rows from one
/// message becomes one append, not one per row.
const DEBOUNCE_MS: u64 = 150;

/// The verbs the pump's OWN kernel tasks run under. A `task` row on the pump's
/// agent carrying one of these is the pump describing itself; persisting it
/// would be the same recursion its `call` rows are skipped for. Python's
/// `_MessagePump.VERBS`, which names two because python splits the read and the
/// flush into two tasks.
const PUMP_VERBS: [&str; 2] = ["message_pump", "message_pump_flush"];

/// COLD HYDRATION of the message store — `(rows, next_id)`.
///
/// Read before any agent exists, like the record tree. A file of another
/// version, or a cut row, is a LOUD error — nothing is skipped or repaired.
pub fn read_messages(root_dir: &Path) -> Result<(Vec<Row>, i64), String> {
    let path = root_dir.join(MESSAGES_FILE);
    if !path.exists() {
        return Ok((Vec::new(), 1));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{MESSAGES_FILE}: {e}"))?;
    let head = text.lines().next().unwrap_or("").trim();
    if head != HEADER.trim() {
        // C55: python `f"{head!r}"` — python `repr()` quoting, not rust's
        // Debug `{:?}` (which escapes differently and prefers double quotes).
        return Err(format!(
            "{MESSAGES_FILE}: unknown log version — first line is {}, this \
             loader writes {}",
            fantastic_bundle::py_repr(head),
            fantastic_bundle::py_repr(HEADER.trim())
        ));
    }
    let rows: Vec<Row> = parse_messages(&text)?
        .into_iter()
        .filter(|r| r.get("id").and_then(Value::as_i64).is_some())
        .collect();
    let top = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .max()
        .unwrap_or(0);
    Ok((rows, top + 1))
}

/// Apply [`read_messages`] to a kernel FROM ABOVE, before its first mint.
///
/// Sets the counter FIRST, so every hydrated id is a pre-minted one. Returns the
/// row count. A duplicate id is the caller ingesting the same file twice and is
/// said loudly, exactly as `rows.put` says it.
pub fn hydrate_messages(kernel: &Kernel, root_dir: &Path) -> Result<usize, String> {
    let (rows, next_id) = read_messages(root_dir)?;
    kernel.rows.set_next_id(next_id);
    let mut n = 0;
    let mut settles: Vec<(i64, Value)> = Vec::new();
    for row in rows {
        let agent = row
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{MESSAGES_FILE}: a row carries no `agent`"))?;
        // Remember the endings; the calls they settle may not be in yet.
        if row.get("kind").and_then(Value::as_str) == Some("settle") {
            if let (Some(parent), Some(out)) = (
                row.get("parent").and_then(Value::as_i64),
                row.get("outcome").cloned(),
            ) {
                settles.push((parent, out));
            }
        }
        kernel.rows.put(&AgentId::from(agent), row)?;
        n += 1;
    }
    // REPLAY THE ENDINGS onto the calls they ended. A call comes back off disk
    // as it was WRITTEN — at launch, with no outcome — because a row is
    // append-only and the settle is its own append. Stamping here rebuilds the
    // state a live kernel had, and it is the only reason a cold read can answer
    // "how did this end".
    for (parent, out) in settles {
        let mut stamp = serde_json::Map::new();
        stamp.insert("outcome".to_string(), out);
        kernel.rows.stamp(parent, &stamp);
    }
    Ok(n)
}

/// REBUILD THE EXECUTION LEDGER FROM THE TAPE — the durable half of C9.
///
/// `ledger.json` is gone. A settled call is a `call` row (payload, retries, name,
/// origin) plus its `settle` row (the full outcome, the final attempts), so the
/// process side is a projection of the two: a DONE row holding the recorded
/// outcome, exactly what a live kernel had. Dedup ("a completed step is never
/// re-executed") therefore holds across reboots with no second file.
///
/// A call with NO settle row was in flight when the process died: it rebuilds
/// NOTHING, and a re-send re-executes it — that work never completed, so running
/// it again is the truth, not a loss.
///
/// Runs at the root loader's `boot`, BEFORE the pump subscribes: rebuilding mints
/// nothing, and nothing rebuilt is re-appended. Returns the count. The COUNTER is
/// restored by [`hydrate_messages`] (cold, before the first mint); nothing here
/// may mint, so nothing here may lower it.
pub fn hydrate_executions(kernel: &Arc<Kernel>) -> usize {
    let mut n = 0;
    for row in kernel.rows.all() {
        if row.get("kind").and_then(Value::as_str) != Some("settle") {
            continue;
        }
        let Some(parent) = row.get("parent").and_then(Value::as_i64) else {
            continue;
        };
        let Some(call) = kernel.rows.get_one(parent) else {
            continue;
        };
        if call.get("kind").and_then(Value::as_str) != Some("call") {
            continue;
        }
        let Some(out) = row.get("outcome").and_then(Value::as_object) else {
            continue;
        };
        let attempts = row
            .get("attempts")
            .and_then(Value::as_u64)
            .or_else(|| call.get("attempts").and_then(Value::as_u64))
            .unwrap_or(1) as u32;
        if kernel.ledger.hydrate_settled(parent, &call, out, attempts) {
            n += 1;
        }
    }
    n
}

/// What the pump carries between its two tasks — python `_MessagePump`'s
/// instance fields.
#[derive(Default)]
struct PumpState {
    /// Rows the tail accepted and the flusher has not landed yet. A REFUSED —
    /// or CANCELLED — append is prepended back here; rust used to drop it.
    pending: Vec<i64>,
    /// The highest id the TAIL has looked at, skipped rows included. `catch_up`
    /// converges when the tail has seen everything the counter minted and
    /// nothing pends (python `_MessagePump.seen`).
    seen: i64,
    /// Unknown until the FIRST flush asks the store whether the log exists: a
    /// NEW log opens with the root element, an existing one is appended to.
    /// Remembered, not re-probed per append (python `_header_needed`).
    header_needed: Option<bool>,
    said_no_store: bool,
}

/// A running message pump — the tail task, the flush task, and the state they
/// share. Python's `_MessagePump` object, which the root loader hangs on itself.
pub struct MessagePump {
    state: Arc<std::sync::Mutex<PumpState>>,
    wake: Arc<tokio::sync::Notify>,
    /// ONE WRITE AT A TIME. `catch_up` takes the same lock for its CHECK, so a
    /// shutdown waits out an in-flight append instead of racing past it and
    /// dropping the writer mid-write (the row was lost: measured 2026-09-24).
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Stops the FLUSH LOOP — fired first at `stop`, because it is the task that
    /// can be holding a half-written append. Dropping it there re-queues the
    /// batch (see [`BatchGuard`]).
    halt_flush: Arc<tokio::sync::Notify>,
    /// Stops the TAIL — fired LAST, so a row minted during the catch-up is still
    /// seen.
    halt_read: Arc<tokio::sync::Notify>,
    tasks: Vec<crate::tasks::TaskId>,
}

/// THE BATCH GOES BACK IF THE WRITE DOES NOT LAND.
///
/// A flush takes its rows OUT of `pending` before awaiting the store, so a stop
/// that drops the flush future between the take and the ack used to eat them.
/// The guard is armed with the batch and disarmed only by a LANDED write; a
/// cancel, a panic or a refused append all return the rows to the head of the
/// queue, where the stop's own catch-up lands them.
struct BatchGuard {
    state: Arc<std::sync::Mutex<PumpState>>,
    batch: Option<Vec<i64>>,
}

impl BatchGuard {
    /// The write landed — nothing to put back.
    fn landed(&mut self) {
        self.batch = None;
    }
}

impl Drop for BatchGuard {
    fn drop(&mut self) {
        let Some(batch) = self.batch.take() else {
            return;
        };
        let mut st = self.state.lock().expect("pump poisoned");
        let rest = std::mem::take(&mut st.pending);
        st.pending = batch;
        st.pending.extend(rest);
    }
}

impl MessagePump {
    /// One append of everything pending, UNDER THE LOCK. The flush loop's body
    /// AND the seam a shutdown uses to land the last window.
    pub async fn flush(&self, kernel: &Arc<Kernel>) {
        let _g = self.lock.lock().await;
        self.flush_once(kernel).await;
    }

    /// One append. CANCELLATION PUTS THE BATCH BACK (see [`BatchGuard`]): a
    /// shutdown that lands between taking the batch and the store's ack must not
    /// eat it.
    async fn flush_once(&self, kernel: &Arc<Kernel>) {
        let has_pending = !self.state.lock().expect("pump poisoned").pending.is_empty();
        if !has_pending {
            return;
        }
        let Some(store_id) = find_store(kernel) else {
            let mut st = self.state.lock().expect("pump poisoned");
            if !st.said_no_store {
                st.said_no_store = true;
                println!("  [messages] no store wired — messages stay in RAM");
            }
            st.pending.clear();
            return;
        };
        let batch: Vec<i64> = {
            let mut st = self.state.lock().expect("pump poisoned");
            std::mem::take(&mut st.pending)
        };
        let mut guard = BatchGuard {
            state: Arc::clone(&self.state),
            batch: Some(batch.clone()),
        };
        // PROBE ONCE. The header question is answered the first time a flush
        // asks it and remembered; rust probed before EVERY append, which is one
        // extra round-trip per message on the store's own call rows.
        let probe_needed = self.state.lock().expect("pump poisoned").header_needed;
        let header_needed = match probe_needed {
            Some(v) => v,
            None => {
                let v = header_probe(kernel, &store_id).await;
                self.state.lock().expect("pump poisoned").header_needed = Some(v);
                v
            }
        };
        // READ THE ROW AT FLUSH TIME, not when it was queued: an outcome
        // stamped onto a `call` row inside the debounce window belongs in the
        // line we are about to write. That is why the tail carries ids.
        let mut lines = String::new();
        for id in &batch {
            let Some(row) = kernel.rows.get_one(*id) else {
                continue;
            };
            lines.push_str(&row_to_xml(&row));
        }
        if header_needed {
            // The root, once, at the head of a NEW log; never closed (a stream).
            lines.insert_str(0, HEADER);
        }
        let (reply, _) = crate::persistence::store_send_binary(
            kernel,
            &store_id,
            json!({"type": "write_stream", "path": MESSAGES_FILE}),
            lines.into_bytes(),
        )
        .await;
        // PYTHON-TRUTHY, and the python TEXT. A present-but-null `error` is not
        // an error, and the line prints the value's `str()`, not a JSON-quoted
        // serde Value.
        if let Some(e) = reply
            .get("error")
            .filter(|v| fantastic_bundle::py_truthy(v))
        {
            println!(
                "  [messages] append refused: {}",
                fantastic_bundle::py_str(e)
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
            // NOTHING LOST: the guard puts the batch back in front of whatever
            // the tail queued while we were writing, and the next flush retries.
            return;
        }
        guard.landed();
        // Only a LANDED write closes the header question: a refused or cancelled
        // append that had cleared it would leave the next one headerless.
        let mut st = self.state.lock().expect("pump poisoned");
        st.header_needed = Some(false);
    }

    /// Land everything the tail has seen — the shutdown's own writing pass.
    ///
    /// The CHECK takes the flush lock, so it waits out an append still in
    /// flight rather than reading an empty queue whose rows are in a flusher's
    /// local batch and calling that "done" (python `_MessagePump.catch_up`).
    pub async fn catch_up(&self, kernel: &Arc<Kernel>) {
        for _ in 0..1000 {
            tokio::task::yield_now().await;
            {
                let _g = self.lock.lock().await;
                let done = {
                    let st = self.state.lock().expect("pump poisoned");
                    st.seen >= kernel.rows.next_id() - 1 && st.pending.is_empty()
                };
                if done {
                    return;
                }
                self.flush_once(kernel).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Land every row minted so far, then stop both tasks. The kernel's
    /// shutdown seam (python `_MessagePump.stop`).
    ///
    /// ORDER MATTERS. The FLUSH LOOP dies first — it is the task that can be
    /// holding a half-written append, and killing it there re-queues the batch;
    /// then `catch_up` runs FROM THE STOPPING TASK (which nobody is stopping)
    /// and lands everything; the reader goes last, so a row minted during the
    /// catch-up is still seen.
    pub async fn stop(self, kernel: &Arc<Kernel>) {
        self.halt_flush.notify_one();
        tokio::task::yield_now().await;
        self.catch_up(kernel).await;
        self.halt_read.notify_one();
        for t in &self.tasks {
            kernel.tasks.forget(t);
        }
        self.wake.notify_waiters();
    }
}

/// Tail the store and append every row to the log, forever.
///
/// `after` is the highest id hydration saw — the pump replays anything minted
/// between then and now, so nothing that landed while the roster booted is lost.
///
/// NO STORE WIRED MEANS RAM ONLY, and it says so once. That is the law records
/// live under too: the kernel does not require a disk to work, and a loader that
/// wires one is what makes anything durable.
///
/// TWO KEEPALIVE KERNEL TASKS, not bare `tokio::spawn`s, and two of them because
/// python splits the read from the flush (`_MessagePump.VERBS`). Being in the
/// table is what puts the pump in `ps` and keeps `shutdown_agents` from waiting
/// on (or cancelling) a loop that never ends — the pump has to be alive AFTER
/// the shutdown verb to append what that verb just wrote.
pub fn install_message_pump(kernel: &Arc<Kernel>, after: i64) -> MessagePump {
    let state = Arc::new(std::sync::Mutex::new(PumpState {
        seen: after,
        ..PumpState::default()
    }));
    let wake = Arc::new(tokio::sync::Notify::new());
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let halt_flush = Arc::new(tokio::sync::Notify::new());
    let halt_read = Arc::new(tokio::sync::Notify::new());
    let root = kernel
        .root()
        .map_or_else(|| AgentId::from("kernel"), |r| r.id.clone());

    let k_read = Arc::clone(kernel);
    let s_read = Arc::clone(&state);
    let w_read = Arc::clone(&wake);
    let h_read = Arc::clone(&halt_read);
    let pump_agent = root.clone();
    let read = kernel.spawn_keepalive_task(&root, PUMP_VERBS[0], async move {
        let body = async {
            let mut tail = k_read.rows.subscribe(after);
            loop {
                let Some(id) = tail.recv().await else { return };
                // THE TAIL HAS SEEN IT either way — `catch_up` converges on this
                // number, and a SKIPPED row still moves it (python `_read`).
                {
                    let mut st = s_read.lock().expect("pump poisoned");
                    st.seen = st.seen.max(id);
                }
                // SKIP AT ARRIVAL, like python's `_read`: the decision is about
                // what the row IS (its kind, its agent, its verb), and a skipped
                // row never joins the queue at all.
                let store = find_store(&k_read);
                let keep = k_read
                    .rows
                    .get_one(id)
                    .is_some_and(|row| !skips(&k_read, store.as_ref(), &pump_agent, &row));
                if !keep {
                    continue;
                }
                s_read.lock().expect("pump poisoned").pending.push(id);
                w_read.notify_one();
            }
        };
        tokio::select! {
            () = h_read.notified() => {}
            () = body => {}
        }
    });

    let k_flush = Arc::clone(kernel);
    let s_flush = Arc::clone(&state);
    let w_flush = Arc::clone(&wake);
    let l_flush = Arc::clone(&lock);
    let h_flush = Arc::clone(&halt_flush);
    let hf_inner = Arc::clone(&halt_flush);
    let hr_inner = Arc::clone(&halt_read);
    let flush = kernel.spawn_keepalive_task(&root, PUMP_VERBS[1], async move {
        let handle = MessagePump {
            state: s_flush,
            wake: Arc::clone(&w_flush),
            lock: l_flush,
            halt_flush: hf_inner,
            halt_read: hr_inner,
            tasks: Vec::new(),
        };
        let body = async {
            loop {
                w_flush.notified().await;
                // DEBOUNCE: give the rest of this batch's rows a moment to land,
                // so one batch is one append rather than one per row.
                tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS)).await;
                handle.flush(&k_flush).await;
            }
        };
        // HALT DROPS THE FLUSH MID-APPEND, which is the point: the batch guard
        // puts the rows back and the stopping task's `catch_up` lands them.
        tokio::select! {
            () = h_flush.notified() => {}
            () = body => {}
        }
    });
    MessagePump {
        state,
        wake,
        lock,
        halt_flush,
        halt_read,
        tasks: vec![read, flush],
    }
}

/// Is the log absent (so the root must be written first)? Probed ONCE, on the
/// first flush, and remembered.
///
/// ON THE BINARY CHANNEL, like the write. `read_stream` is a binary verb: the
/// file bundle REFUSES it on the text path, so a plain send answers "error" for
/// a file that is sitting right there — and the root would be re-written at the
/// head of every append, in the middle of the document.
async fn header_probe(kernel: &Arc<Kernel>, store_id: &AgentId) -> bool {
    let (reply, _) = crate::persistence::store_send_binary(
        kernel,
        store_id,
        json!({"type": "read_stream", "path": MESSAGES_FILE, "offset": 0, "length": 1}),
        Vec::new(),
    )
    .await;
    reply.get("error").is_some()
}

/// THE PUMP SKIPS ITS OWN TRAFFIC. A flush is a send, so it mints a `call` row
/// on the store; persisting that row would mint another, forever. The pump's own
/// `task` row is the same recursion one level up, and a `settle` is the ending of
/// a call — skipping the call but keeping its ending would bring the loop back
/// through the other half.
///
/// Evaluated AT ARRIVAL, like python's `_MessagePump._skip`.
fn skips(
    kernel: &Arc<Kernel>,
    store_id: Option<&AgentId>,
    pump_agent: &AgentId,
    row: &Row,
) -> bool {
    let kind = row.get("kind").and_then(Value::as_str);
    let agent = row.get("agent").and_then(Value::as_str);
    if kind == Some("task") && agent == Some(pump_agent.0.as_str()) {
        return row
            .get("verb")
            .and_then(Value::as_str)
            .is_some_and(|v| PUMP_VERBS.contains(&v));
    }
    // The store's own OPERATOR-CONTEXT calls: the pump's writes and probes,
    // which carry no origin because nobody sent them.
    if kind == Some("call") && store_id.is_some_and(|s| agent == Some(s.0.as_str())) {
        return row.get("origin").is_none() || row.get("origin") == Some(&Value::Null);
    }
    if kind == Some("settle") {
        let Some(parent) = row.get("parent").and_then(Value::as_i64) else {
            return false;
        };
        return kernel
            .rows
            .get_one(parent)
            .is_some_and(|p| skips(kernel, store_id, pump_agent, &p));
    }
    false
}

#[cfg(test)]
#[path = "messages_log/tests.rs"]
mod tests;
