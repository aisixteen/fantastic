//! THE TRACES (#766, spec 1.10): the chat's own record of its passes, local only.
//!
//! A turn collects its events in RAM ([`Events`]): the render's `render_cold` or
//! `render_warm` and its `cache_dropped`, each failed attempt's `retry`, each call read
//! after a closers-only repair of its JSON (`repaired {attempt, added}`), the `stop`, the
//! endpoint's `prefix_cache`, a `model_switched`. They are written at the turn's end, in
//! ONE `append` under the chat's own key, after the turn's rows are stored
//! ([`write`]); a cancelled turn or a dead process writes none. A trace row is
//! `{kind: "trace", event, mode, type, content: "", client_id, row?, ..}`: no `role`, so
//! it is in no conversation and no model view; an empty `content`, so a store `search`
//! never finds it. The head cache skips it (`memory.rs`), a fork copies none
//! (`fork.rs`).
//!
//! The rows are capped ([`TRACE_CAP`]): after the append, the oldest beyond the cap are
//! read and pruned by their ids. The chat keeps the count in RAM, read from the store
//! once per process; only the chat writes its own key and a fork copies no trace row,
//! so the count stays true. A refused trace write never fails the turn. Nothing is sent
//! anywhere.

use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use super::pass::{Mode, Pass};
use crate::constants::TRACE_CAP;
use crate::memory::{rows_of, store_call};
use crate::state::BackendState;

/// The `kind` of a trace row.
pub(crate) const KIND: &str = "trace";

/// One turn's trace rows, in the order they happened.
#[derive(Debug, Default)]
pub(crate) struct Events {
    rows: Vec<Value>,
}

impl Events {
    /// Add one event: `{kind: "trace", event, mode, type, content: "", ..fields}`. The
    /// send's `client_id` and the stored row's id are added when the turn writes them
    /// ([`write`]). A field named like a fixed key is not taken.
    pub(crate) fn push(
        &mut self,
        mode: &'static str,
        kind: &'static str,
        event: &'static str,
        fields: Map<String, Value>,
    ) {
        let mut row = Map::new();
        row.insert("kind".into(), json!(KIND));
        row.insert("event".into(), json!(event));
        row.insert("mode".into(), json!(mode));
        row.insert("type".into(), json!(kind));
        row.insert("content".into(), json!(""));
        for (k, v) in fields {
            row.entry(k).or_insert(v);
        }
        self.rows.push(Value::Object(row));
    }

    /// The rows, taken.
    pub(crate) fn into_rows(self) -> Vec<Value> {
        self.rows
    }
}

/// THE TURN'S EVENTS, taken in order: the turn's pass's (its helpers' included), then
/// those of the pass a type restart ran under (Q26), which took the earlier ones over.
pub(crate) fn of_turn(pass: &Pass, restarted: Option<&Pass>) -> Vec<Value> {
    let mut rows = pass.take_events().into_rows();
    if let Some(next) = restarted {
        rows.extend(next.take_events().into_rows());
    }
    rows
}

/// The model type of the chat's previous turn pass: the RAM one, else (after a restart)
/// the `type` of the newest trace row its store holds. `None` on a chat's first pass,
/// and after a restart with no trace row left.
pub(crate) async fn previous_type(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    state: &BackendState,
) -> Option<String> {
    let ram = *state.last_type.lock().expect("last_type poisoned");
    if let Some(kind) = ram {
        return Some(kind.to_string());
    }
    let res = store_call(
        self_id,
        kernel,
        json!({"type": "read", "kinds": [KIND], "order": "desc", "limit": 1}),
    )
    .await
    .ok()?;
    rows_of(&res, self_id)
        .first()
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A `model_switched {from, to, model}` event on `pass` when the chat's previous turn
/// pass was of another type ([`previous_type`]).
pub(crate) async fn note_switch(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    state: &BackendState,
    pass: &Pass,
) {
    let Some(from) = previous_type(self_id, kernel, state).await else {
        return;
    };
    if from == pass.kind {
        return;
    }
    let mut fields = Map::new();
    fields.insert("from".into(), json!(from));
    fields.insert("to".into(), json!(pass.kind));
    fields.insert(
        "model".into(),
        json!(pass.model.clone().unwrap_or_default()),
    );
    pass.events()
        .push(Mode::Turn.name(), pass.kind, "model_switched", fields);
}

/// A `stop` the model wrote in the turn's stored `answer` (1.8): its `stop {reason}`
/// event on `pass`, with no `errors` (those are the harness's, when it wrote the stop).
/// The first `stop` call counts; one with no non-empty reason is none.
pub(crate) fn note_model_stop(pass: &Pass, answer: &str) {
    let Some(reason) = crate::tool_parse::extract_tool_calls(answer)
        .into_iter()
        .find(|c| c.name == "stop")
        .and_then(|c| {
            c.arguments
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|r| !r.trim().is_empty())
    else {
        return;
    };
    let mut fields = Map::new();
    fields.insert("reason".into(), json!(reason));
    pass.events()
        .push(Mode::Turn.name(), pass.kind, "stop", fields);
}

/// THE TURN'S TRACE ROWS, written as STEPS (#795): `events` with the send's `client_id`
/// and, when the turn stored one, its assistant row's id as `row`, in one `append` under
/// the chat's key; then the oldest beyond the cap pruned by id. Each store call is one
/// step; `next` runs when the write ended. A refusal anywhere ends the write and never
/// reaches the turn.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write<R, F, Fut>(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    state: &Arc<BackendState>,
    client_id: &str,
    row: Option<i64>,
    events: Vec<Value>,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let cap = cap_of(self_id);
    if events.is_empty() || cap == 0 {
        return next(rec).await;
    }
    let rows: Vec<Value> = events
        .into_iter()
        .map(|mut e| {
            if let Some(m) = e.as_object_mut() {
                m.insert("client_id".into(), json!(client_id));
                // an event's own `row` (the row `cache_dropped` names) is kept
                if let Some(id) = row {
                    m.entry("row").or_insert_with(|| json!(id));
                }
                m.insert("id".into(), json!(kernel.mint_id()));
            }
            e
        })
        .collect();
    let w = Writing {
        me: self_id.clone(),
        kernel: Arc::clone(kernel),
        state: Arc::clone(state),
        cap,
        next: Box::new(move || Box::pin(next(rec))),
    };
    // THE COUNT: the RAM one, else read once (a step)
    let ram = *state.trace_count.lock().expect("trace_count poisoned");
    match ram {
        Some(count) => w.append(count, rows).await,
        None => {
            let ask = json!({"type": "read", "kinds": [KIND], "limit": 1});
            crate::steps::store(self_id, kernel, ask, None, (w, rows), |(w, rows), r| async move {
                let n = r
                    .ok()
                    .and_then(|(res, _)| res.get("total").and_then(Value::as_u64))
                    .and_then(|n| usize::try_from(n).ok());
                match n {
                    Some(n) => {
                        *w.state.trace_count.lock().expect("trace_count poisoned") = Some(n);
                        w.append(n, rows).await;
                    }
                    None => w.end().await,
                }
            })
            .await;
        }
    }
}

/// A trace write in flight.
struct Writing {
    me: AgentId,
    kernel: Arc<Kernel>,
    state: Arc<BackendState>,
    cap: usize,
    next: Box<dyn FnOnce() -> futures_util::future::BoxFuture<'static, ()> + Send>,
}

impl Writing {
    async fn end(self) {
        (self.next)().await;
    }

    /// The rows appended (a step), then the oldest beyond the cap pruned.
    async fn append(self, count: usize, rows: Vec<Value>) {
        let n = rows.len();
        let (me, k) = (self.me.clone(), Arc::clone(&self.kernel));
        crate::steps::store(&me, &k, json!({"type": "append", "rows": rows}), None, self, move |w, r| async move {
            if r.is_err() {
                return w.end().await;
            }
            let count = count + n;
            if count <= w.cap {
                *w.state.trace_count.lock().expect("trace_count poisoned") = Some(count);
                return w.end().await;
            }
            w.prune(count).await;
        })
        .await;
    }

    /// The `count - cap` oldest trace rows read and pruned by their ids (two steps).
    async fn prune(self, count: usize) {
        let over = count - self.cap;
        let (me, k) = (self.me.clone(), Arc::clone(&self.kernel));
        let ask = json!({"type": "read", "kinds": [KIND], "order": "asc", "limit": over});
        crate::steps::store(&me, &k, ask, None, self, move |w, r| async move {
            let ids: Vec<i64> = r
                .map(|(res, _)| {
                    rows_of(&res, &w.me)
                        .iter()
                        .filter_map(|r| r.get("id").and_then(Value::as_i64))
                        .collect()
                })
                .unwrap_or_default();
            if ids.is_empty() {
                *w.state.trace_count.lock().expect("trace_count poisoned") = Some(count);
                return w.end().await;
            }
            let (me, k) = (w.me.clone(), Arc::clone(&w.kernel));
            crate::steps::store(&me, &k, json!({"type": "prune", "ids": ids}), None, w, move |w, r| async move {
                let pruned = r
                    .ok()
                    .and_then(|(res, _)| res.get("pruned").and_then(Value::as_u64))
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(0);
                *w.state.trace_count.lock().expect("trace_count poisoned") = Some(count.saturating_sub(pruned));
                w.end().await;
            })
            .await;
        })
        .await;
    }
}

/// TESTS: [`write`] driven to its end.
#[cfg(test)]
pub(crate) async fn written(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    state: &Arc<BackendState>,
    client_id: &str,
    row: Option<i64>,
    events: Vec<Value>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    write(self_id, kernel, state, client_id, row, events, (), move |()| async move {
        let _ = tx.send(());
    })
    .await;
    let _ = rx.await;
}

/// The cap of `self_id`'s traces: [`TRACE_CAP`], or a test's own.
fn cap_of(self_id: &AgentId) -> usize {
    test_cap(self_id).unwrap_or(TRACE_CAP)
}

/// No test sets a cap outside the tests.
#[cfg(not(test))]
fn test_cap(_self_id: &AgentId) -> Option<usize> {
    None
}

/// The cap a test set for `self_id` ([`set_cap_for_test`]).
#[cfg(test)]
fn test_cap(self_id: &AgentId) -> Option<usize> {
    test_caps()
        .lock()
        .expect("test caps poisoned")
        .get(self_id.as_str())
        .copied()
}

#[cfg(test)]
fn test_caps() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, usize>> {
    static CAPS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<String, usize>>> =
        std::sync::OnceLock::new();
    CAPS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// A test's cap for `self_id`'s traces, in place of [`TRACE_CAP`].
#[cfg(test)]
pub(crate) fn set_cap_for_test(self_id: &AgentId, cap: usize) {
    test_caps()
        .lock()
        .expect("test caps poisoned")
        .insert(self_id.as_str().to_string(), cap);
}
