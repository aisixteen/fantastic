//! The verbs of an AI chat, answered from their typed calls
//! ([`crate::declared::chat::Call`]). The chat bundle builds its provider from the
//! record and hands it to [`handle`]; the provider is the seam, constructed per call.

use crate::agent_loop::BackendConfig;
use crate::constants::MAX_CALL_DEPTH;
use crate::context::{budget as agent_budget, output_reserve, resolve_context_window};
use crate::declared::{History, Inquiry, Recall, Rows, Status, Unwatch, Watch};
use crate::events::emit_status;
use crate::helpers::{agent_meta, safe_client};
use crate::memory::MemResult;
use crate::model_format::pass::Pass;
use crate::projection::{round3, TRIGGER};
use crate::provider::Provider;
use crate::state::{state_for, status_snapshot, CurrentEntry};
use crate::stream_parts::{Part, PartKind};
use fantastic_bundle::{py_json_dumps, py_repr};
use fantastic_kernel::bundle::BundleError;
use fantastic_kernel::call_guard::{check_call_stack, CallStackRefusal};
use fantastic_kernel::refusal::{
    refusal, refusal_hint, with, BAD_ARG, MISSING_ARG, NOT_FOUND, NO_CONSUMER, UNAUTHORIZED,
};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// The reflect body EVERY ai_core backend answers with — python's
/// `_make_reflect`, key for key and in its order: `id, sentence, model,
/// endpoint, context_window, context_strategy, memory_store_id, generating`. A
/// backend adds only its own `reflect_extra` keys, which land after `generating`
/// exactly as python's `out.update(extra)` does. No verb docs.
///
/// `model`/`endpoint` are the RECORD's values, raw (absent ⇒ `null`: nothing is
/// guessed); `context_strategy` takes its default only when ABSENT — a `null`
/// on the record reflects back as `null`. `context_window` resolves against
/// `model_window`, the backend's own window ([`BackendConfig::model_window`]).
#[must_use]
pub fn reflect_base(
    agent_id: &AgentId,
    kernel: &Kernel,
    sentence: &str,
    model_window: i64,
) -> Map<String, Value> {
    let meta = agent_meta(agent_id, kernel);
    let mut out = Map::new();
    out.insert("id".to_string(), json!(agent_id.as_str()));
    out.insert("sentence".to_string(), json!(sentence));
    // What the RECORD names — nothing guessed, so an unset field reads null.
    out.insert(
        "model".to_string(),
        meta.get("model").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "endpoint".to_string(),
        meta.get("endpoint").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "context_window".to_string(),
        json!(resolve_context_window(&meta, model_window)),
    );
    out.insert(
        "context_strategy".to_string(),
        meta.get("context_strategy")
            .cloned()
            .unwrap_or_else(|| json!(crate::strategies::DEFAULT_STRATEGY)),
    );
    out.insert(
        "memory_store_id".to_string(),
        meta.get("memory_store_id").cloned().unwrap_or(Value::Null),
    );
    // WHERE its key is read — `{store, ref}`, a pointer, never the secret. An agent
    // copying this one's record needs it.
    out.insert(
        "credentials".to_string(),
        meta.get("credentials").cloned().unwrap_or(Value::Null),
    );
    // A pass in flight ⇔ generating.
    out.insert(
        "generating".to_string(),
        json!(crate::state::is_generating(agent_id)),
    );
    if let Some(why) = crate::prompt_file::error_of(agent_id) {
        out.insert("system_prompt_file_error".to_string(), json!(why));
    }
    out
}

/// A reply that answers a call of a chain that is over — a previous incarnation's,
/// or one a stop / clear / lost wake ended — is kept as a `late` row in this
/// agent's own store and wakes nothing. A STEP: `next` gets the store's word.
pub(crate) async fn save_late<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, raw: &str, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<()>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let row = json!({
        "role": "user",
        "parent": Value::Null,
        "kind": "late",
        "content": format!(
            "[late reply — answered a call of a chain that has ended; \
             recorded, not acted on]\n{raw}"
        ),
    });
    crate::steps::append(agent_id, kernel, &[row], rec, move |rec, r| next(rec, r.map(|_| ()))).await;
}

/// A HANDLER'S DEFERRED SETTLE: the execution `d` settles with `result` — an `Err` is
/// the raise a handler's error makes (`failed`, `{error, raised}`), a store's refusal
/// that of its `MemoryStoreError`. `None`: the handler ran as the kernel's bookkeeping
/// (an `await_set` wake), which answers no call — nothing settles, and an `Err` is said.
pub(crate) fn settle_with<E: Into<BundleError>>(kernel: &Kernel, d: Option<fantastic_kernel::Deferred>, result: Result<Value, E>) {
    let Some(d) = d else {
        if let Err(e) = result {
            eprintln!("  [ai] a message the kernel handed over was not taken: {}", e.into());
        }
        return;
    };
    let pid = d.pid();
    let out = match result {
        Ok(v) => fantastic_kernel::Outcome::new(Some(pid), fantastic_kernel::Status::Completed, v),
        Err(e) => fantastic_kernel::Outcome::new(
            Some(pid),
            fantastic_kernel::Status::Failed,
            json!({"error": e.into().to_string(), "raised": true}),
        ),
    };
    kernel.settle_deferred(pid, out);
}

/// `status` verb.
pub fn status(agent_id: &AgentId, args: &Status) -> Value {
    status_snapshot(agent_id, args.client_id.as_deref())
}

/// The `on_delete` bundle hook, SHARED by every backend (each one's hook calls
/// it, like [`on_cancel`]; vllm drops its own table too). Drops every row this
/// crate keeps for the deleted id, so an id re-created later starts clean and
/// nothing leaks: the FIFO lock and queue, the in-flight entry,
/// the projection and its marks, the fold and recall counters, the calibration
/// (the state slot), the jobs and open inquiries, the strip set, and
/// this kernel's head, bodies, grants and watchers, and its rows in the memory
/// store (`keep_memory:true` on the delete keeps those). Python `on_delete`.
///
/// NOT the incarnation: it stays monotonic across a re-create, so a reply to a
/// call the deleted agent made never becomes a live turn of the new one.
pub async fn on_delete(agent_id: &AgentId, kernel: &Arc<Kernel>) {
    // Its persisted rows go with it (unless `delete_agent keep_memory:true`).
    crate::memory::purge(agent_id, kernel).await;
    crate::state::drop_state(agent_id);
    crate::route::forget(agent_id.as_str());
    crate::strategies::strip::forget(agent_id.as_str());
    crate::memory::forget(agent_id, kernel);
    crate::prompt_file::drop_file(agent_id, kernel).await;
    crate::remote_provider::forget_watch(agent_id, kernel).await;
}

/// The `on_cancel` bundle hook, shared by every backend: the kernel is killing
/// `task_id`, one of this agent's own tasks. Nothing of a pass needs closing here: a
/// pass is no task, and a cancel of its request (the provider's `complete`, or the task
/// that reads an in-process provider) settles that request as cancelled, which ends
/// the pass as stopped ([`crate::agent_loop`]).
pub async fn on_cancel(
    _agent_id: &AgentId,
    _task_id: &str,
    _kernel: &Arc<Kernel>,
) -> Result<(), BundleError> {
    Ok(())
}

/// Close out a pass that was STOPPED — by a cancel of its request or by `stop` — from
/// its in-flight entry, which the caller has taken. The open parts abort, the pass
/// leaves what it had visibly said and a `[stopped by …]` marker row after the messages
/// it read (stored when they arrived; the append is a step, #795), and `status` goes to
/// `done` with `reason='cancelled'`; then `next` gets how the rows' write went.
pub(crate) async fn close_stopped<R, F, Fut>(
    agent_id: &AgentId,
    entry: CurrentEntry,
    kernel: &Arc<Kernel>,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, Result<(), BundleError>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut unsent: Vec<Part> = Vec::new();
    // HELD TO THE END OF THE CLOSE: the doomed stream loop runs on another worker
    // until the kernel drops it, and must not open a part after these aborts.
    let mut parts_guard = None;
    if let Some(parts) = &entry.parts {
        let mut p = Arc::clone(parts).lock_owned().await;
        unsent = p
            .splitter
            .parts
            .iter()
            .filter(|x| x.kind == PartKind::Call)
            .cloned()
            .collect();
        let evs = p.splitter.stop("stopped");
        p.publisher.push(evs, true).await;
        parts_guard = Some(p);
    }
    let who = entry
        .stopped_by
        .clone()
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "cancel".to_string());
    let (shown, notice) = crate::stream_parts::stop_rows(&entry.text_so_far, &who, &unsent);
    let mut rows = Vec::new();
    let mut prev = entry.answers;
    if let Some(text) = shown {
        let answer_id = kernel.mint_id();
        let mut row = json!({
            "role": "assistant",
            "content": text,
            "id": answer_id,
            "parent": prev,
        });
        if let Some(m) = row.as_object_mut() {
            m.extend(crate::think::thinking_fields(&entry.text_so_far));
        }
        rows.push(row);
        prev = answer_id;
    }
    rows.push(json!({
        "role": "user",
        "kind": "stopped",
        "content": notice,
        "id": kernel.mint_id(),
        "parent": prev,
    }));
    let client_id = entry.client_id;
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("cancelled"));
    detail.insert("send_id".to_string(), json!(entry.send_id));
    // THE ENTRY IS ALREADY TAKEN, so `emit_status`'s auto-fill has nothing left
    // to read: python passes `started_at` explicitly from the entry it took, and
    // rust's cancel detail was the one `done` event missing it.
    detail.insert("started_at".to_string(), json!(entry.started_at));
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    crate::steps::append(agent_id, kernel, &rows, (rec, parts_guard), move |(rec, guard), r| async move {
        if let Err(e) = r {
            drop(guard);
            // python's `str(e)` — the message alone, as the kernel's log line prints it.
            return next(rec, Err(BundleError::from(e.0))).await;
        }
        emit_status(&k, &state_for(&me), &me, &client_id, "done", detail).await;
        drop(guard);
        next(rec, Ok(())).await;
    })
    .await;
}

/// `stop` — python `_stop`. args: `stream_id:str?` (a part of the pass in flight).
/// STOP this agent's pass in flight — how a receiver of its streaming parts stops it:
/// the request is cancelled (its stream closes), open parts abort, the pass keeps a
/// `[stopped by <you>]` marker (a step). Stop means stop: the messages not read yet
/// stay stored and wake nothing. `next` gets `{stopped:bool}`: `false` when nothing is
/// in flight or `stream_id` is not one of this pass's parts.
pub async fn stop<R, F, Fut>(
    agent_id: &AgentId,
    stream_id: Option<&str>,
    who: String,
    kernel: &Arc<Kernel>,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, Value) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let state = state_for(agent_id);
    let current = state
        .current_meta
        .lock()
        .expect("current poisoned")
        .as_ref()
        .map(|c| c.parts.clone());
    let Some(parts) = current else {
        return next(rec, json!({"stopped": false})).await;
    };
    if let Some(sid) = stream_id {
        let Some(parts) = parts else {
            return next(rec, json!({"stopped": false})).await;
        };
        let known = parts
            .lock()
            .await
            .splitter
            .parts
            .iter()
            .any(|p| sid == p.stream_id.as_str());
        if !known {
            return next(rec, json!({"stopped": false})).await;
        }
    }
    // THE PASS ENDS NOW: its request's settle acts on nothing from here
    crate::life::abandon(agent_id);
    let Some(mut entry) = crate::state::take_current(agent_id) else {
        return next(rec, json!({"stopped": false})).await;
    };
    entry.stopped_by = Some(who);
    // the request in flight: an in-process provider's read drops its stream on the
    // abort; the kernel's cancel reaches the provider's `complete` (#177)
    entry.abort.notify_one();
    if let Some(pid) = entry.task_id.as_deref().and_then(|t| t.parse::<i64>().ok()) {
        let reached = kernel.cancel_task(&TaskId(pid)).await;
        if reached["cancelled"] != json!(true) {
            // named, not dropped: the request had ended already, or was never a task
            println!("  [stop] {} kernel cancel did not reach task {pid}: {reached}", agent_id.0);
        }
    }
    close_stopped(agent_id, entry, kernel, rec, move |rec, r| async move {
        let reply = match r {
            Ok(()) => json!({"stopped": true}),
            Err(e) => json!({"stopped": true, "error": e.to_string()}),
        };
        next(rec, reply).await;
    })
    .await;
}

/// Who stops a pass from the dispatch running here: its asker, else the operator.
pub(crate) fn stopper() -> String {
    fantastic_kernel::current_origin()
        .map(|a| a.0)
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "operator".to_string())
}

/// TESTS: [`stop`] driven to its end.
#[cfg(test)]
pub(crate) async fn stopped(agent_id: &AgentId, stream_id: Option<&str>, kernel: &Arc<Kernel>) -> Value {
    let (tx, rx) = tokio::sync::oneshot::channel();
    stop(agent_id, stream_id, stopper(), kernel, tx, |tx, v| async move {
        let _ = tx.send(v);
    })
    .await;
    rx.await.unwrap_or(Value::Null)
}

/// `rows` — a VIEW of this agent's conversation exactly as the agent HOLDS it in
/// memory — its loaded head, in the order it lived it; no store read, no transform,
/// so what a UI draws is what the agent has. Before the agent has loaded its head (a
/// fresh boot, before its first turn) it holds nothing: `{rows:[], loaded:false}`.
/// Returns `{rows, next, total, loaded}`.
///
/// THE HEAD IS THIS KERNEL'S CACHE (`Mem::heads`), never a store mirror (a mirror
/// desyncs). `after` is a row id (an id not in the head pages nothing); `next` is the
/// page's last row id only when more rows follow; `total` is the whole head.
#[must_use]
pub fn rows(agent_id: &AgentId, args: &Rows, kernel: &Kernel) -> Value {
    let head = crate::memory::mem(kernel)
        .heads
        .lock()
        .expect("heads poisoned")
        .get(&agent_id.0)
        .cloned();
    let Some(head) = head else {
        return json!({"rows": [], "next": null, "total": 0, "loaded": false});
    };
    let total = head.len();
    let mut rows = head;
    if let Some(after) = args.after {
        rows = match rows
            .iter()
            .position(|r| r.get("id").and_then(Value::as_i64) == Some(after))
        {
            Some(at) => rows.split_off(at + 1),
            None => Vec::new(),
        };
    }
    let limit = args
        .limit
        .map(|l| usize::try_from(l).unwrap_or(usize::MAX));
    let more = limit.is_some_and(|l| l < rows.len());
    let page: Vec<Value> = match limit {
        Some(l) => rows.into_iter().take(l).collect(),
        None => rows,
    };
    let next = if more {
        page.last()
            .and_then(|r| r.get("id").cloned())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    json!({"rows": page, "next": next, "total": total, "loaded": true})
}

/// One row at DIGEST resolution — python `_digest`: `{id, parent, kind, role,
/// head, bytes}` (+ the row's `attributes`, whole), the head capped at 120 code points
/// with newlines flattened.
fn digest(r: &Value) -> Value {
    // python `str(r.get("content", ""))`: an ABSENT content is the empty string, a
    // null one is `None` (4 bytes), a dict one is its repr.
    let content = r.get("content").cloned().unwrap_or_else(|| json!(""));
    let text = crate::helpers::shown(&content);
    let mut d = json!({
        "id": r.get("id"),
        "parent": r.get("parent"),
        // `kind` falls back to `role` when the row has none.
        "kind": match r.get("kind") {
            Some(k) if !k.is_null() => k.clone(),
            _ => r.get("role").cloned().unwrap_or(Value::Null),
        },
        "role": r.get("role"),
        "head": text.chars().take(120).collect::<String>().replace('\n', " "),
        "bytes": text.chars().count(),
    });
    // what the row knows about itself, whole and for the caller only
    if let Some(a) = r.get("attributes") {
        d["attributes"] = a.clone();
    }
    // a row that kept its generation's thinking says so (python `has_thinking`)
    if r.get("thinking").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
        d["has_thinking"] = json!(true);
    }
    d
}

/// `history` verb — the conversation at DIGEST resolution, or, with `fold`, that
/// fold's title + summary and its members (one level down; a member that is
/// itself a fold opens the same way). Python `_history`.
///
/// ONE SHAPE: `{rows, next, total, fold?}`. Bodies are behind `recall` / `grep`,
/// never here.
pub(crate) async fn history(
    agent_id: &AgentId,
    args: History,
    kernel: &Arc<Kernel>,
    model_window: i64,
    settle: crate::steps::Settle,
) {
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    // ONE READ, a step (#795): the whole conversation for a fold, else the head
    if args.fold.is_some() {
        crate::steps::whole(agent_id, kernel, (args, settle), move |(args, settle), r| async move {
            settle.with(r.map(|all| history_page(&me, &args, &k, model_window, all, Vec::new())));
        })
        .await;
    } else {
        crate::steps::refreshed(agent_id, kernel, (args, settle), move |(args, settle), r| async move {
            settle.with(r.map(|head| {
                let cards: Vec<Value> = head.iter().filter(|r| crate::embed::is_card_row(r)).cloned().collect();
                history_page(&me, &args, &k, model_window, crate::memory::turns(head), cards)
            }));
        })
        .await;
    }
}

/// The `history` reply over what its read gave: `rows`, the whole conversation for a
/// fold, else the head's turns; `cards`, the head's card rows.
fn history_page(
    agent_id: &AgentId,
    args: &History,
    kernel: &Kernel,
    model_window: i64,
    rows: Vec<Value>,
    cards: Vec<Value>,
) -> Value {
    let mut head: Option<Value> = None;
    let rows: Vec<Value> = match args.fold {
        Some(fold_id) => {
            let everything = rows;
            let by_id: std::collections::HashMap<i64, &Value> = everything
                .iter()
                .filter_map(|r| r.get("id").and_then(Value::as_i64).map(|i| (i, r)))
                .collect();
            let f = by_id.get(&fold_id).filter(|f| {
                matches!(
                    f.get("kind").and_then(Value::as_str),
                    Some("fold" | "clear")
                )
            });
            let Some(f) = f else {
                let mut r = refusal(
                    NOT_FOUND,
                    format!("history: {fold_id} is not a fold in your conversation"),
                );
                with(&mut r, "arg", json!("fold"));
                return r;
            };
            let refs: Vec<i64> = f
                .get("refs")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            head = Some(json!({
                "id": fold_id,
                "kind": f.get("kind"),
                "title": f.get("title"),
                "summary": f.get("body").filter(|b| !b.is_null()).cloned().unwrap_or(json!("")),
                "members": refs.len(),
            }));
            refs.iter()
                .filter_map(|m| by_id.get(m).map(|r| (*r).clone()))
                .collect()
        }
        None => rows,
    };
    let mut digests: Vec<Value> = rows
        .iter()
        .filter(|r| r.get("id").and_then(Value::as_i64).is_some())
        .map(digest)
        .collect();
    // CHAT CARDS (#339): embed / unembed rows have no role, so `load_history` drops
    // them; the page redraws them from here, in their place among the turns.
    let mut card_count = 0;
    if args.fold.is_none() {
        let cards: Vec<Value> = cards
            .iter()
            .filter(|r| r.get("id").and_then(Value::as_i64).is_some())
            .map(digest)
            .collect();
        card_count = cards.len();
        if !cards.is_empty() {
            digests.extend(cards);
            digests.sort_by_key(|d| d["id"].as_i64().unwrap_or(0));
        }
    }
    if let Some(after) = args.after {
        // AN UNKNOWN CURSOR PAGES NOTHING, rather than starting over.
        digests = match digests.iter().position(|d| d["id"].as_i64() == Some(after)) {
            Some(i) => digests.split_off(i + 1),
            None => Vec::new(),
        };
    }
    let total = rows.len() + card_count;
    // THE CAP INVARIANT: a digest page must answer UNDER the inline cap.
    let meta = agent_meta(agent_id, kernel);
    let (kept, truncated) = crate::context::fit_reply(
        &digests,
        crate::context::inline_cap(&meta, model_window),
        args.limit,
    );
    let next = truncated
        .then(|| kept.last().map(|d| d["id"].clone()))
        .flatten()
        .unwrap_or(Value::Null);
    let mut out = json!({ "rows": kept, "next": next, "total": total });
    if let Some(h) = head {
        out["fold"] = h;
    }
    out
}

/// Render ONE stored turn for a `recall` reply — python `_recall_render`.
///
/// WHOLE, NOT CAPPED — python returns it verbatim, and the cut half would be
/// exactly what a model pages back FOR.
///
/// FROM THE CONTENT, NEVER THE ROW: a string content is itself (an EMPTY one
/// included), and anything else is `json.dumps(content, default=str)` — so
/// `{"k": "é"}` renders as `{"k": "é"}`. Serializing the whole row when the
/// content is empty would hand the model its own bookkeeping.
fn recall_render(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        other => py_json_dumps(other.unwrap_or(&Value::Null), true, None),
    }
}

/// Whether `m` is a delivered reply whose every block answers this agent's own
/// `recall` or `grep` (the reply tag names the agent and the verb).
fn own_search_reply(agent_id: &AgentId, m: &Value) -> bool {
    if m.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(text) = m.get("content").and_then(Value::as_str) else {
        return false;
    };
    let blocks = crate::history_view::reply_blocks(text);
    !blocks.is_empty()
        && blocks.iter().all(|b| {
            matches!(crate::history_view::parse_reply_tag(b),
                Some((who, "recall" | "grep")) if who == agent_id.as_str())
        })
}

/// `recall` verb: page turns back from the DURABLE chat store (the FULL
/// conversation, never trimmed — so anything compaction dropped is one call
/// away). Read-only. args: `query?` (case-insensitive substring), `limit?`
/// (default 20, max 100), `before?` (store index), `id?` (a row id: that row whole).
///
/// The reply is python's and nothing more: `{messages, total, truncated[,
/// note]}`. There is no `client_id` — a model reading its own recall was handed
/// a key python does not send.
pub(crate) async fn recall(
    agent_id: &AgentId,
    args: Recall,
    kernel: &Arc<Kernel>,
    model_window: i64,
    settle: crate::steps::Settle,
) {
    // the whole conversation is read by a step (#795); the page is cut from it
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    crate::steps::whole(agent_id, kernel, (args, settle), move |(args, settle), r| async move {
        settle.with(r.map(|full| recall_page(&me, &args, &k, model_window, &full)));
    })
    .await;
}

/// The `recall` reply over `full`, the whole conversation.
fn recall_page(agent_id: &AgentId, args: &Recall, kernel: &Kernel, model_window: i64, full: &[Value]) -> Value {
    // BY ID: the row whole — its content and, when it had any, its thinking.
    if let Some(id) = args.id {
        let Some((i, row)) = full
            .iter()
            .enumerate()
            .find(|(_, r)| r.get("id").and_then(Value::as_i64) == Some(id))
        else {
            let mut r = refusal(
                NOT_FOUND,
                format!("recall: {id} is not a row in your conversation"),
            );
            with(&mut r, "arg", json!("id"));
            return r;
        };
        let mut m = json!({"index": i, "role": row.get("role"), "content": recall_render(row)});
        if let Some(t) = row
            .get("thinking")
            .filter(|t| t.as_str().is_some_and(|s| !s.is_empty()))
        {
            m["thinking"] = t.clone();
        }
        // AN IMAGE ROW comes back with its image, once: shown whole in the next request
        // by a model that can see it (#622)
        if let Some(parts) = crate::images::parts_of(row) {
            let seen = crate::images::can(&agent_meta(agent_id, kernel), crate::images::NEEDS);
            let how = if seen {
                state_for(agent_id)
                    .recalled_images
                    .lock()
                    .expect("recalled poisoned")
                    .push(id);
                format!(
                    "{} image(s) of this row come back in your next request, once",
                    parts.len()
                )
            } else {
                format!(
                    "{} image(s) of this row, not shown to this model",
                    parts.len()
                )
            };
            let text = m["content"].as_str().unwrap_or("");
            m["content"] = json!(if text.is_empty() {
                format!("[{how}]")
            } else {
                format!("{text}\n[{how}]")
            });
        }
        return json!({"messages": [m], "total": 1, "truncated": false});
    }
    let q = args.query.as_deref().map(str::to_lowercase);
    let limit = args.limit as usize;
    let before = args.before;
    let mut indexed: Vec<(usize, &Value)> = full.iter().enumerate().collect();
    if let Some(before) = before {
        indexed.retain(|(i, _)| (*i as i64) < before);
    }
    if let Some(q) = &q {
        // a QUERY searches the conversation, not its own past searches: a reply to this
        // agent's own `recall` or `grep` echoes what it found and is left out
        indexed.retain(|(_, m)| {
            !own_search_reply(agent_id, m)
                && py_json_dumps(m, true, None)
                    .to_lowercase()
                    .contains(q.as_str())
        });
    }
    let total = indexed.len();
    let truncated = total > limit;
    let mut page: Vec<&(usize, &Value)> = indexed[total.saturating_sub(limit)..].iter().collect();
    // A PAGE MUST FIT THE WINDOW IT IS GOING INTO: a default page of large reads
    // can exceed the window, `too_small` fires as a failfast and the chain dies.
    // The page is trimmed from the OLDEST end (the newest of what was
    // asked for is kept) and says so, with the index to continue from — the
    // caller loses a page, never the run. Measured on the WHOLE reply serialized
    // as one user turn, which is the unit delivery measures.
    let cap = crate::context::inline_cap(&agent_meta(agent_id, kernel), model_window);
    let render_page = |pg: &[&(usize, &Value)]| -> Vec<Value> {
        pg.iter()
            .map(|(i, m)| json!({"index": i, "role": m.get("role"), "content": recall_render(m)}))
            .collect()
    };
    let serialized = |pg: &[&(usize, &Value)]| -> i64 {
        let probe = json!({
            "messages": render_page(pg), "total": total, "truncated": true,
            "note": format!(
                "page trimmed by 999 older turn(s) to fit your window (cap {cap} tok); \
                 pass before=999999 to page further back."
            ),
        });
        crate::context::estimate_one(&json!({
            "role": "user",
            "content": crate::tool_parse::render_reply(&probe),
        }))
    };
    let mut trimmed = 0usize;
    while page.len() > 1 && serialized(&page) > (cap as f64 * 0.8) as i64 {
        page.remove(0);
        trimmed += 1;
    }
    let mut reply = json!({
        "messages": render_page(&page), "total": total,
        "truncated": truncated || trimmed > 0,
    });
    if trimmed > 0 {
        reply["note"] = json!(format!(
            "page trimmed by {trimmed} older turn(s) to fit your window (cap {cap} tok); \
             pass before={} to page further back.",
            page[0].0
        ));
    }
    if page.is_empty() {
        // An empty page with no reason reads as "your history is lost" — the
        // opposite of what it usually means (py-canonical `note`).
        let fired = crate::state::state_for(agent_id)
            .projection
            .lock()
            .expect("projection poisoned")
            .as_ref()
            .and_then(|p| p.get("fired").and_then(Value::as_bool))
            .unwrap_or(false);
        let note = if let (Some(q), false) = (&q, full.is_empty()) {
            format!(
                "query {} matched none of the {} stored turns.",
                py_repr(q),
                full.len()
            )
        } else if !fired {
            "nothing has been compacted away — recall pages back turns that \
             compaction DROPPED, and none have been. If you can see this reply, \
             your live context already holds the whole conversation."
                .to_string()
        } else {
            "no stored turns in this page.".to_string()
        };
        reply["note"] = json!(note);
    }
    reply
}

/// `context_status` verb: the context-budget posture + the last overflow
/// projection + the model's derived reaction to it. Read-only. The reaction reads the
/// head after a projection fired — a step (#795).
pub(crate) async fn context_status(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    model_window: i64,
    settle: crate::steps::Settle,
) {
    let state = state_for(agent_id);
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    crate::projection::derive_reaction(agent_id, &state, kernel, settle, move |settle, r| async move {
        settle.with(r.map(|reaction| status_of_context(&me, &k, model_window, reaction)));
    })
    .await;
}

/// The `context_status` reply, the reaction read.
fn status_of_context(agent_id: &AgentId, kernel: &Kernel, model_window: i64, reaction: Option<Value>) -> Value {
    let meta = agent_meta(agent_id, kernel);
    let state = state_for(agent_id);
    let last_projection = state
        .projection
        .lock()
        .expect("projection poisoned")
        .clone()
        .unwrap_or(Value::Null);
    let last_reaction = reaction.unwrap_or(Value::Null);
    // RAW, as python's `rec.get("context_strategy", DEFAULT_STRATEGY)`: only an
    // ABSENT key means `compact`.
    let strategy = meta
        .get("context_strategy")
        .cloned()
        .unwrap_or_else(|| json!(crate::strategies::DEFAULT_STRATEGY));
    let calib = state.calib.lock().expect("calib poisoned").unwrap_or(1.0);
    let reported = *state.reported.lock().expect("reported poisoned");
    json!({
        "context_window": resolve_context_window(&meta, model_window),
        "output_reserve": output_reserve(&meta),
        "budget": agent_budget(&meta, model_window),
        "trigger": TRIGGER,
        "calibration": round3(calib),
        "reported_prompt_tokens": reported,
        "strategy": strategy,
        "last_projection": last_projection,
        "last_reaction": last_reaction,
        "view_source": state.view_source.lock().expect("view_source poisoned").clone(),
    })
}

pub use crate::constants::{send_timeout_secs, SEND_TIMEOUT_SECS};

/// THE ARRIVAL OF A MESSAGE (#795): guard the call chain → the memory store named (fail
/// loud) → a `late` reply recorded, never a pass → the images brought inside the
/// model's limits → the row STORED → the ACK. The message is read by the agent's next
/// pass, which starts now when none runs ([`crate::life`]); nothing here waits for it.
///
/// Returns `{accepted: true, re: <row id>, client_id}`; paused, `{held, paused}`. A
/// refusal (a cycle, no store, bad images) is the reply, and nothing is stored. The
/// answer, if the model gives one, is the receiving agent's own later message.
pub async fn send(
    agent_id: &AgentId,
    args: &crate::declared::Send,
    kernel: &Arc<Kernel>,
    cfg: BackendConfig,
) -> Result<Value, BundleError> {
    let mut text = args.text.clone();
    // A MESSAGE FROM ANOTHER AGENT OPENS WITH WHO SENT IT — python's
    // `[from <sender> · to a, b · msg N · re N]` line. Without it a peer's inquiry
    // arrives as bare prose and the receiver cannot tell it is being asked, nor whom to
    // answer. The operator's own sends carry no header, exactly as in python.
    if let Some(sender) = fantastic_kernel::current_origin() {
        if sender != *agent_id {
            let mut parts = vec![format!("from {}", sender.as_str())];
            if let Some(to) = args.to.as_ref().filter(|a| !a.is_empty()) {
                parts.push(format!("to {}", to.join(", ")));
            }
            if let Some(m) = args.msg {
                parts.push(format!("msg {m}"));
            }
            if let Some(r) = args.re {
                parts.push(format!("re {r}"));
            }
            // the stream this message was previewed as — what an AI consumer that
            // watched it stream matches its draft to
            if let Some(s) = &args.stream_id {
                parts.push(format!("stream {s}"));
            }
            text = format!("[{}]\n{text}", parts.join(" \u{b7} "));
        }
    }
    let client_id = safe_client(&args.client_id);
    // The sender's id: the calling agent, else the client.
    let origin_id = fantastic_kernel::current_origin()
        .map_or_else(|| client_id.clone(), |o| o.as_str().to_string());
    // Recursion guard — the kernel's rule, shared with the authored-runtime connectors;
    // only the wording is ours, because the reader here is a model.
    match check_call_stack(agent_id.as_str(), args.call_stack.as_deref().unwrap_or(&[])) {
        Ok(_) => {}
        Err(CallStackRefusal::Cycle(cycle)) => {
            return Ok(json!({
                "error": format!("{}: cycle detected", cfg.name),
                "cycle": cycle,
                "client_id": client_id,
            }));
        }
        Err(CallStackRefusal::TooDeep) => {
            return Ok(json!({
                "error": format!("{}: max call depth {MAX_CALL_DEPTH} reached", cfg.name),
                "client_id": client_id,
            }));
        }
    }
    // NO STORE, NO MESSAGE — said up front.
    if let Err(e) = crate::memory::store_of(agent_id, kernel) {
        let mut r = refusal(MISSING_ARG, e.0);
        with(&mut r, "arg", json!("memory_store_id"));
        with(&mut r, "client_id", json!(client_id));
        return Ok(r);
    }
    // THE ACK IS THE STORED ROW (#795): this call settles once the store took the
    // message; the steps between are calls of their own, and nothing here waits. A
    // kernel's own hand-over (bookkeeping) answers no call: its steps run, nothing settles
    let d = kernel.defer().ok();
    if args.late == Some(true) {
        // A reply to a call a PREVIOUS incarnation made (see `route::deliver`): recorded
        // in this agent's own store, never a pass.
        let k = Arc::clone(kernel);
        save_late(agent_id, kernel, &args.text, client_id, move |cid, r| async move {
            settle_with(&k, d, r.map(|()| json!({"late": true, "client_id": cid})));
        })
        .await;
        return Ok(Value::Null);
    }
    // A notice is this agent's own harness text: honoured only when the agent sent it to
    // itself (or the operator did), so no peer can hide a message from a person's view.
    let notice = args.notice == Some(true)
        && fantastic_kernel::current_origin().is_none_or(|s| s == *agent_id);
    let a = Accepting {
        kernel: Arc::clone(kernel),
        me: agent_id.clone(),
        d,
        text,
        client_id,
        origin: origin_id,
        notice,
    };
    let state = state_for(agent_id);
    let images = args.images.clone();
    // WHAT THE PROVIDER CAN DO is read from it when the chat starts and when its provider
    // or model changed, and the images of this send are brought inside the model's limits
    // (#622)
    crate::images::refresh(agent_id, kernel, state, a, move |a| async move {
        let (k, me) = (Arc::clone(&a.kernel), a.me.clone());
        crate::images::attach(&me, &k, images, a, |a, attached| a.attached(attached)).await;
    })
    .await;
    Ok(Value::Null)
}

/// A `send` being accepted: the steps from its images to its stored row, and the
/// execution that settles with the ack.
struct Accepting {
    kernel: Arc<Kernel>,
    me: AgentId,
    d: Option<fantastic_kernel::Deferred>,
    text: String,
    client_id: String,
    origin: String,
    notice: bool,
}

impl Accepting {
    /// The images are attached (or refused): the message is stored, then acked.
    async fn attached(self, attached: crate::images::AttachResult) {
        let attached = match attached {
            Ok(a) => a,
            Err(e) => {
                let mut r = refusal(BAD_ARG, e);
                with(&mut r, "arg", json!("images"));
                with(&mut r, "client_id", json!(self.client_id));
                return settle_with(&self.kernel, self.d, Ok::<_, BundleError>(r));
            }
        };
        let m = crate::life::Incoming {
            text: self.text,
            client_id: self.client_id.clone(),
            origin: self.origin,
            notice: self.notice,
            attached,
        };
        let (k, me, d, cid) = (Arc::clone(&self.kernel), self.me.clone(), self.d, self.client_id);
        crate::life::arrive(&self.kernel, &self.me, m, true, (), move |(), r| async move {
            let reply = r.map(|row| {
                // PAUSED (#219): stored, read by the pass the resume starts
                if state_for(&me).life.lock().expect("life poisoned").paused {
                    json!({"held": crate::life::unread_count(&me), "paused": true})
                } else {
                    json!({"accepted": true, "re": row, "client_id": cid})
                }
            });
            settle_with(&k, d, reply);
        })
        .await;
    }
}

// ── inquiry ─────────────────────────────────────────────────────────────────

/// What an inquiry carries ON ITSELF: that it IS one and how it is answered.
/// Delivered bare as `[from ai]` + the question, a reviewer does the work, writes
/// its verdict as prose and ends with `done`. Python `_INQUIRY_HEAD`, verbatim
/// (who asked, the pid).
pub fn inquiry_head(asker: &str, pid: &str) -> String {
    format!(
        "[inquiry from {asker} · re {pid} — do any work it needs, then answer with \
         <call name=\"inquiry_answer\"><answer>…</answer></call> before done: nothing \
         else counts as your answer]"
    )
}

/// Open an `inquiry`: render it as the text of the `send` that runs it.
///
/// ONE INQUIRY VERB, WHOEVER RECEIVES IT. A person answers `inquiry` from a record and
/// a peer AI answers it by THINKING; the sender writes the same call either way
/// and reads the answer off the same reply.
#[must_use]
pub fn inquiry_text(args: &Inquiry) -> String {
    // The inquiry IS the turn. `options`/`kind` ride in the text because a model
    // reads prose, not a schema; `action` is carried verbatim so an answering
    // agent can see what it is being asked to endorse.
    let asker = fantastic_kernel::current_origin();
    let pid = fantastic_kernel::current_task().map_or_else(|| "-".to_string(), |t| t.0.to_string());
    let mut text = format!(
        "{}\n{}",
        inquiry_head(asker.as_ref().map_or("the operator", |a| a.as_str()), &pid),
        args.text
    );
    if let Some(opts) = args.options.as_ref().filter(|o| !o.is_empty()) {
        text.push_str("\n\nOptions: ");
        text.push_str(&opts.join(", "));
    }
    if let Some(act) = &args.action {
        // python `json.dumps(act)` — `, `/`: ` separators and ensure_ascii.
        text.push_str("\n\nProposed action: ");
        text.push_str(&py_json_dumps(&Value::Object(act.clone()), true, None));
    }
    text
}

/// Accept an `inquiry` (FULLY ASYNC): register it as open under its own client id,
/// store it as a message and wake on it, and answer at once `{accepted, re,
/// request_id}`. The answer the model gives with `inquiry_answer` in any later pass —
/// or the news that a pass failed or stopped without one — reaches the asker later as
/// a message under this call's pid (`crate::route`).
///
/// # Errors
/// The memory store refused the inquiry's row.
pub async fn inquiry_accept(
    agent_id: &AgentId,
    args: &Inquiry,
    text: &str,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    if let Err(e) = crate::memory::store_of(agent_id, kernel) {
        let mut r = refusal(MISSING_ARG, e.0);
        with(&mut r, "arg", json!("memory_store_id"));
        return Ok(r);
    }
    let asker = fantastic_kernel::current_origin().map(|a| a.as_str().to_string());
    let task = fantastic_kernel::current_task();
    let pid = task.map_or_else(|| "-".to_string(), |t| t.0.to_string());
    let client = safe_client(&format!("inquiry-{pid}"));
    let reply_client = safe_client(&args.reply_client_id);
    let request_id = args.request_id.clone().map_or(Value::Null, Value::String);
    crate::route::open_inquiries()
        .lock()
        .expect("open inquiries poisoned")
        .insert(
            (agent_id.as_str().to_string(), client.clone()),
            crate::route::OpenInquiry {
                asker: asker.clone(),
                pid,
                reply_client,
                request_id: request_id.clone(),
            },
        );
    let origin = asker.unwrap_or_else(|| client.clone());
    let m = crate::life::Incoming {
        text: text.to_string(),
        client_id: client,
        origin,
        notice: false,
        attached: None,
    };
    // acked once its row is stored (#795): a deferred settle, nothing waits
    let d = kernel.defer().ok();
    let k = Arc::clone(kernel);
    let reply = json!({
        "accepted": true,
        "re": task.map_or(Value::Null, |t| json!(t.0)),
        "request_id": request_id,
    });
    crate::life::arrive(kernel, agent_id, m, true, (), move |(), r| async move {
        settle_with(&k, d, r.map(|_| reply));
    })
    .await;
    Ok(Value::Null)
}

// ── boot, the row event, context by grant ───────────────────────────────────

/// `boot` — LAZY: never loads the conversation. Checks the record names a memory
/// store (fail loud otherwise), starts a new incarnation, and marks the view for
/// rebuilding — the first call reads the head from the store and runs
/// `context_recall`. Python `_make_boot`. The store reads: whether the agent is
/// paused (its last `pause`/`resume` row) — a paused agent says `paused` (#714) —,
/// when its record offers `line_summary`, its last line (#709), and the messages stored
/// after its last answer, which one pass reads (#795); a store that refuses a read
/// refuses the boot. After a kernel restart, the calls the previous process cut that
/// concern this agent come to it as one notice ([`crate::route::resume_cut`]).
pub async fn boot(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    crate::state::bump_incarnation(agent_id);
    // A new incarnation starts with no pass: a request of the previous one that settles
    // now acts on nothing.
    crate::life::abandon(agent_id);
    *state_for(agent_id).current_meta.lock().expect("current poisoned") = None;
    if let Err(e) = crate::memory::store_of(agent_id, kernel) {
        let mut r = refusal(MISSING_ARG, e.0);
        with(&mut r, "arg", json!("memory_store_id"));
        return r;
    }
    // A new incarnation reads its store afresh; bodies are this run's only.
    crate::memory::forget_head(agent_id, kernel);
    // the provider is asked what it can do again at the first turn of this incarnation
    *state_for(agent_id)
        .caps_checked
        .lock()
        .expect("caps_checked poisoned") = None;
    *state_for(agent_id)
        .recall_due
        .lock()
        .expect("recall_due poisoned") = true;
    if let Err(e) = crate::pause::announce_at_boot(agent_id, kernel).await {
        return refusal(fantastic_kernel::refusal::STORE_REFUSED, e.0);
    }
    // THE MESSAGES STORED AND NOT READ (the process stopped first) are read by a pass
    // (#795): nothing acked is lost
    if let Err(e) = crate::life::restore_unread(kernel, agent_id).await {
        return refusal(fantastic_kernel::refusal::STORE_REFUSED, e.0);
    }
    // the chat's current line: the last `line_summary` its history holds (#709)
    if let Err(e) = crate::line_summary::restore(agent_id, kernel).await {
        return refusal(fantastic_kernel::refusal::STORE_REFUSED, e.0);
    }
    // WORK THE RESTART CUT (#726) goes on: what the previous process left in flight is
    // a dead letter now, and this agent is told, once.
    crate::route::resume_cut(kernel, agent_id, &kernel.take_cut(agent_id)).await;
    crate::life::wake(kernel, agent_id);
    Value::Null
}

/// `watch` — announce every row this agent appends to the caller's inbox as
/// `{type:'row', source, row:{id, kind, role, bytes}}`; with `body:true` the row's
/// `content` too, but only for rows a live grant from this agent gives the
/// watcher. Python `_watch`.
pub async fn watch(agent_id: &AgentId, args: &Watch, kernel: &Arc<Kernel>) -> MemResult<Value> {
    let caller = fantastic_kernel::current_origin();
    let watcher = match &caller {
        None => match args.watcher.as_deref() {
            Some(w) if !w.is_empty() => w.to_string(),
            _ => {
                let mut r = refusal(
                    MISSING_ARG,
                    "watch: the operator must name a `watcher` inbox",
                );
                with(&mut r, "arg", json!("watcher"));
                return Ok(r);
            }
        },
        Some(c) => {
            if args.watcher.as_deref().is_some_and(|w| w != c.as_str()) {
                return Ok(refusal(
                    UNAUTHORIZED,
                    "watch: an agent watches for ITSELF — `watcher` is the operator's",
                ));
            }
            c.0.clone()
        }
    };
    if !kernel
        .inboxes
        .contains_key(&AgentId::from(watcher.as_str()))
    {
        return Ok(refusal_hint(
            NO_CONSUMER,
            format!(
                "watch: {} has no registered inbox, so nothing would read the row events",
                py_repr(&watcher)
            ),
            format!(
                "register the reader first — ctx.ensure_inbox({}) — then watch",
                py_repr(&watcher)
            ),
        ));
    }
    let body = args.body == Some(true);
    crate::memory::add_watcher(agent_id, kernel, &watcher, body);
    Ok(json!({"watching": true, "watcher": watcher, "body": body}))
}

/// `unwatch` — stop announcing rows to the caller. Python `_unwatch`.
pub fn unwatch(agent_id: &AgentId, args: &Unwatch, kernel: &Arc<Kernel>) -> Value {
    let watcher = match fantastic_kernel::current_origin() {
        Some(c) => Value::from(c.0),
        None => args.watcher.clone().map_or(Value::Null, Value::String),
    };
    if let Some(w) = watcher.as_str() {
        crate::memory::remove_watcher(agent_id, kernel, w);
    }
    json!({"watching": false, "watcher": watcher})
}

/// EVERY VERB OF AN AI CHAT, answered from its typed call. `provider_for(client_id)`
/// builds the provider — always: a record missing what a pass needs raises from inside
/// that pass, naming the field. `reflect` carries each bundle's own extras, so the
/// bundle hands in how it is answered; every other verb is answered here.
pub async fn handle<F, R>(
    agent_id: &AgentId,
    call: crate::declared::chat::Call,
    kernel: &Arc<Kernel>,
    cfg: BackendConfig,
    provider_for: F,
    reflect: R,
) -> Result<Value, BundleError>
where
    F: Fn(&str) -> Arc<dyn Provider> + Send + Sync + 'static,
    R: FnOnce() -> Value,
{
    use crate::declared::chat::Call;
    // HOW THIS AGENT'S PASSES REACH THEIR MODEL, kept for the passes that start outside a
    // call to it (a reply's wake, the next pass over what arrived meanwhile)
    let provider_for: crate::state::ProviderFor = Arc::new(provider_for);
    *state_for(agent_id).machine.lock().expect("machine poisoned") = Some(crate::state::Machine {
        provider: Arc::clone(&provider_for),
        cfg,
    });
    let reply = match call {
        Call::Reflect(_) => crate::line_summary::with_line(agent_id, kernel, reflect()),
        Call::Boot(_) => boot(agent_id, kernel).await,
        Call::Send(a) => send(agent_id, &a, kernel, cfg).await?,
        // A receiver of this agent's streaming parts stops the generation.
        Call::Stop(a) => {
            let settle = crate::steps::deferred(kernel);
            stop(agent_id, a.stream_id.as_deref(), stopper(), kernel, settle, |settle, v| async move {
                settle.with(Ok::<_, BundleError>(v));
            })
            .await;
            Value::Null
        }
        // What the agent HOLDS in memory — its loaded head, never a store read.
        Call::Rows(a) => rows(agent_id, &a, kernel),
        // ONE ASK VERB, WHOEVER IS ASKED: a person answers `inquiry` from a
        // record, this agent answers it by THINKING, and the asker writes the
        // same call either way.
        Call::Inquiry(a) => {
            let text = inquiry_text(&a);
            inquiry_accept(agent_id, &a, &text, kernel).await?
        }
        Call::History(a) => {
            history(agent_id, a, kernel, cfg.model_window, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        // A CARD in the chat: a host agent's panel, shown inline (#339).
        Call::Embed(a) => {
            crate::embed::embed(agent_id, &a.id, kernel, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        Call::Unembed(a) => {
            crate::embed::unembed(agent_id, &a.id, kernel, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        Call::Status(a) => status(agent_id, &a),
        Call::ContextStatus(_) => {
            context_status(agent_id, kernel, cfg.model_window, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        Call::Recall(a) => {
            recall(agent_id, a, kernel, cfg.model_window, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        // FIND the line anywhere in memory — rows, shared rows, bodies held this run.
        Call::Grep(a) => {
            crate::retrieval::grep(agent_id, a, kernel, cfg.model_window, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        // Rebuild the view from the store: on boot, after folds, on request.
        // A dry recall makes no model call, so it builds no pass (#766).
        Call::ContextRecall(a) => {
            let dry = a.dry == Some(true);
            let p = provider_for(&fantastic_kernel::inquiry::default_client_id());
            // its history, its pass's type, its prompt file and its own `reflect` are steps
            // (#795); the recall settles this call once it ran
            let settle = crate::steps::deferred(kernel);
            let (k, me, window) = (Arc::clone(kernel), agent_id.clone(), cfg.model_window);
            crate::steps::history(agent_id, kernel, settle, move |settle, rows| async move {
                let st = state_for(&me);
                let (k2, me2) = (Arc::clone(&k), me.clone());
                Pass::begin(p, &me, &st, &k, rows.unwrap_or_default(), settle, move |settle, pass| async move {
                    let (k3, me3) = (Arc::clone(&k2), me2.clone());
                    crate::prompt_file::refresh(&me2, &k2, (settle, pass), move |(settle, pass)| async move {
                        let (k4, me4) = (Arc::clone(&k3), me3.clone());
                        crate::steps::own_reflect(&me3, &k3, (settle, pass), move |(settle, pass), reflected| async move {
                            // A dry recall makes no model call, so it uses no pass (#766).
                            let used = (!dry).then_some(&pass);
                            let r = crate::retrieval::context_recall(
                                &me4, dry, &k4, used, pass.decl, window, None, &[], &reflected,
                            )
                            .await;
                            settle.with(r);
                        })
                        .await;
                    })
                    .await;
                })
                .await;
            })
            .await;
            Value::Null
        }
        // A new thread on the same store: the view starts here, nothing is deleted.
        Call::ContextClear(a) => {
            let settle = crate::steps::deferred(kernel);
            crate::retrieval::context_clear(agent_id, a.why, kernel, cfg.model_window, settle).await;
            Value::Null
        }
        // Fold a chosen stretch of the view into one fold row — the operator's mark.
        Call::ContextFold(a) => {
            let p = provider_for(&fantastic_kernel::inquiry::default_client_id());
            // its pass's type is a step (#795), over the history it read (a step)
            let settle = crate::steps::deferred(kernel);
            let (k, me, window) = (Arc::clone(kernel), agent_id.clone(), cfg.model_window);
            crate::steps::history(agent_id, kernel, settle, move |settle, rows| async move {
                let st = state_for(&me);
                let (k2, me2) = (Arc::clone(&k), me.clone());
                Pass::begin(p, &me, &st, &k, rows.unwrap_or_default(), settle, move |settle, pass| async move {
                    let r = crate::retrieval::context_fold(&me2, &a, &k2, &pass, window).await;
                    settle.with(r);
                })
                .await;
            })
            .await;
            Value::Null
        }
        // Compact the store: drop the rows the folds already summarise.
        Call::ContextForget(a) => {
            let settle = crate::steps::deferred(kernel);
            crate::retrieval::context_forget(agent_id, a.include_cleared == Some(true), kernel, settle).await;
            Value::Null
        }
        // A NEW agent from this one's first rows; a mark to fork from; this agent's marks.
        Call::Fork(a) => crate::fork::fork(agent_id, a, kernel).await,
        Call::Mark(a) => crate::fork::mark(agent_id, &a, kernel).await,
        Call::ForkPoints(a) => {
            crate::fork::fork_points(agent_id, a, kernel, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        // THE ROW EVENT: this agent announces its own rows to its watchers.
        Call::Watch(a) => watch(agent_id, &a, kernel).await?,
        Call::Unwatch(a) => unwatch(agent_id, &a, kernel),
        Call::Pause(a) => {
            crate::pause::pause(agent_id, a.reason, kernel, crate::steps::deferred(kernel)).await;
            Value::Null
        }
        Call::Resume(a) => {
            crate::pause::resume(agent_id, a.note, kernel, crate::steps::deferred(kernel)).await;
            Value::Null
        }
    };
    Ok(reply)
}

#[cfg(test)]
mod own_search_tests {
    use super::own_search_reply;
    use fantastic_kernel::AgentId;
    use serde_json::json;

    /// A query leaves out the replies to the agent's own `recall` and `grep`; a reply
    /// from another agent, or a batch with any other block, stays searchable.
    #[test]
    fn a_reply_to_the_agents_own_search_is_left_out_of_a_query() {
        let me = AgentId::from("ai");
        let row = |c: &str| json!({"role": "user", "content": c});
        assert!(own_search_reply(&me, &row("[reply to 3 · ai recall]\nPRJ-1")));
        assert!(own_search_reply(&me, &row("[reply to 4 · ai grep]\nPRJ-1")));
        assert!(own_search_reply(
            &me,
            &row("[reply to 5 · ai recall · 1 of 2 in batch ab]\nx\n\n[reply to 6 · ai grep · 2 of 2 in batch ab]\ny")
        ));
        // another agent's reply, another verb, a mixed batch, a root, an assistant row
        assert!(!own_search_reply(&me, &row("[reply to 3 · notes recall]\nPRJ-1")));
        assert!(!own_search_reply(&me, &row("[reply to 3 · ai history]\nPRJ-1")));
        assert!(!own_search_reply(
            &me,
            &row("[reply to 5 · ai recall · 1 of 2 in batch ab]\nx\n\n[reply to 6 · fb read · 2 of 2 in batch ab]\nPRJ-1")
        ));
        assert!(!own_search_reply(&me, &row("the code is PRJ-1")));
        assert!(!own_search_reply(
            &me,
            &json!({"role": "assistant", "content": "[reply to 3 · ai recall]\nx"})
        ));
    }
}
