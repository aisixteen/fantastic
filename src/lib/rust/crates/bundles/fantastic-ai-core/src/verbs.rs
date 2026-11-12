//! The verbs of an AI chat, answered from their typed calls
//! ([`crate::declared::chat::Call`]). The chat bundle builds its provider from the
//! record and hands it to [`handle`]; the provider is the seam, constructed per call.

use crate::agent_loop::{run_generation, BackendConfig, GenFailure, Generation};
use crate::constants::MAX_CALL_DEPTH;
use crate::context::{budget as agent_budget, output_reserve, resolve_context_window};
use crate::declared::{History, Inquiry, Recall, Rows, Status, Unwatch, Watch};
use crate::events::emit_status;
use crate::helpers::{agent_meta, mint_send_id, now_secs, safe_client};
use crate::memory::{load_history, MemResult};
use crate::projection::{derive_reaction, round3, TRIGGER};
use crate::provider::Provider;
use crate::state::{state_for, status_snapshot, CurrentEntry, QueuedEntry};
use crate::stream_parts::{Part, PartKind};
use fantastic_bundle::{
    py_float_repr, py_json_dumps, py_repr, py_strip,
};
use fantastic_kernel::bundle::BundleError;
use fantastic_kernel::call_guard::{check_call_stack, CallStackRefusal};
use fantastic_kernel::refusal::{
    refusal, refusal_hint, with, BAD_ARG, MISSING_ARG, NOT_FOUND, NO_CONSUMER, UNAUTHORIZED,
};
use fantastic_kernel::{current_task, AgentId, Kernel, TaskId};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

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
    // A generation IS its kernel dispatch task — the backend holds no task
    // handle of its own. An in-flight entry present ⇔ generating.
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
/// agent's own store and wakes nothing.
async fn save_late(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    raw: &str,
) -> Result<(), BundleError> {
    crate::memory::save_history(
        agent_id,
        kernel,
        &[json!({
            "role": "user",
            "parent": Value::Null,
            "kind": "late",
            "content": format!(
                "[late reply — answered a call of a chain that has ended; \
                 recorded, not acted on]\n{raw}"
            ),
        })],
    )
    .await?;
    Ok(())
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
/// (the state slot), the routing count, the silent streaks, the strip set, and
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
}

/// The `on_cancel` bundle hook, shared by every backend: the kernel is killing
/// `task_id` and will DROP this generation's future the moment we return.
///
/// A dropped future frees the HTTP request (the provider stream) and the FIFO
/// guard on its own — what it does NOT do is close the turn out for the caller.
/// So that is exactly what happens here, while the task is still alive: clear
/// the in-flight entry (`status` would otherwise report a phantom generation
/// forever) and emit the final `status(done, reason=cancelled)` + `done` the
/// client is waiting on. No-op when the cancelled task is some OTHER dispatch on
/// this agent (a `history`, a `reflect`).
///
/// THE CUT: every open part ABORTS for its watchers, and the
/// turn leaves the incoming message + a marker row — never a half answer — so the
/// model's next turn knows what it was asked, what it had visibly said, and who
/// stopped it. Nothing of a stopped pass was dispatched. A store that refuses
/// the rows fails the hook (python raises out of `on_cancel`; the kernel logs it
/// and the kill still happens).
pub async fn on_cancel(
    agent_id: &AgentId,
    task_id: &str,
    kernel: &Arc<Kernel>,
) -> Result<(), BundleError> {
    let Some(entry) = crate::state::take_current_for_cancel(agent_id, task_id) else {
        // Not the running pass: a message HELD on this chain whose sender's
        // deadline passed is withdrawn, and a marker row tells the next pass
        // not to answer it. (A held message a running wake pass already took
        // is not here: the kernel still settles it, its answer goes nowhere.)
        if let Some(h) = crate::state::withdraw_held(agent_id, task_id) {
            let row = json!({
                "role": "user",
                "kind": "withdrawn",
                "content": format!(
                    "[withdrawn] the message above from {} passed its sender's deadline and went to dead letter {task_id}; do not answer it.",
                    h.origin
                ),
                "id": kernel.mint_id(),
                "parent": h.row_id,
            });
            crate::memory::save_history(agent_id, kernel, &[row])
                .await
                .map_err(|e| BundleError::from(e.0))?;
        }
        return Ok(());
    };
    close_stopped(agent_id, entry, kernel).await
}

/// Close out a turn that was STOPPED — by the kernel's cancel ([`on_cancel`]) or by
/// `stop` — from its in-flight entry, which the caller has taken. The open parts
/// abort, the turn keeps its incoming message and a `[stopped by …]` marker row,
/// and `status` goes to `done` with `reason='cancelled'`.
pub async fn close_stopped(
    agent_id: &AgentId,
    entry: CurrentEntry,
    kernel: &Arc<Kernel>,
) -> Result<(), BundleError> {
    let state = state_for(agent_id);
    let mut unsent: Vec<Part> = Vec::new();
    // HELD TO THE END OF THE HOOK: the doomed stream loop runs on another worker
    // until the kernel drops it, and must not open a part after these aborts.
    let mut _parts_guard = None;
    if let Some(parts) = &entry.parts {
        let mut p = parts.lock().await;
        unsent = p
            .splitter
            .parts
            .iter()
            .filter(|x| x.kind == PartKind::Call)
            .cloned()
            .collect();
        let evs = p.splitter.stop("stopped");
        p.publisher.push(evs, true).await;
        _parts_guard = Some(p);
    }
    let who = entry
        .stopped_by
        .clone()
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "cancel".to_string());
    let in_id = kernel.mint_id();
    let in_parent = crate::agent_loop::incoming_parent(&entry.text);
    let (shown, notice) = crate::stream_parts::stop_rows(&entry.text_so_far, &who, &unsent);
    let mut rows = vec![crate::row_attrs::with_source(
        crate::images::stamped(
            crate::agent_loop::incoming_row(&entry.text, in_id, in_parent, entry.notice),
            entry.attached.as_ref(),
        ),
        &entry.origin,
    )];
    let mut prev = in_id;
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
    crate::memory::save_history(agent_id, kernel, &rows)
        .await
        // python's `str(e)` — the message alone, as the kernel's log line prints it.
        .map_err(|e| BundleError::from(e.0))?;
    let client_id = entry.client_id;
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("cancelled"));
    detail.insert("send_id".to_string(), json!(entry.send_id));
    // THE ENTRY IS ALREADY TAKEN, so `emit_status`'s auto-fill has nothing left
    // to read: python passes `started_at` explicitly from the entry it took, and
    // rust's cancel detail was the one `done` event missing it.
    detail.insert("started_at".to_string(), json!(entry.started_at));
    emit_status(kernel, &state, agent_id, &client_id, "done", detail).await;
    Ok(())
}

/// `stop` — python `_stop`. args: `stream_id:str?` (a part of the generation in
/// flight). STOP this agent's in-flight generation — how a receiver of its
/// streaming parts stops it: the provider stream closes, open parts abort, the
/// turn keeps a `[stopped by <you>]` marker. Returns `{stopped:bool}`:
/// `false` when nothing is in flight or `stream_id` is not one of this pass's
/// parts.
pub async fn stop(agent_id: &AgentId, stream_id: Option<&str>, kernel: &Arc<Kernel>) -> Value {
    let state = state_for(agent_id);
    let (task_id, parts) = {
        let cur = state.current_meta.lock().expect("current poisoned");
        match cur.as_ref() {
            Some(CurrentEntry {
                task_id: Some(t),
                parts,
                ..
            }) => (t.clone(), parts.clone()),
            _ => return json!({"stopped": false}),
        }
    };
    if let Some(sid) = stream_id {
        let Some(parts) = parts else {
            return json!({"stopped": false});
        };
        let known = parts
            .lock()
            .await
            .splitter
            .parts
            .iter()
            .any(|p| sid == p.stream_id.as_str());
        if !known {
            return json!({"stopped": false});
        }
    }
    let who = fantastic_kernel::current_origin()
        .map(|a| a.0)
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "operator".to_string());
    let pid = task_id
        .parse::<i64>()
        .expect("the in-flight task id is the kernel's TaskId number");
    // The kernel's cancel first: its task walk runs `on_cancel`, which closes the
    // turn (marker row, `status` done), and drops the dispatch with its stream.
    // THE CAUSE OF A STOP THAT LEAVES A TURN GENERATING (#177): the walk finds the
    // turn only by the task id it recorded, which is the id of its own dispatch row
    // (`current_task()`). When that id is not in the kernel's task table the walk
    // answers `{cancelled:false}` and runs no `on_cancel`, so nothing closes the
    // turn. A reply-woken turn is not that case: its send is a dispatch row nested
    // under the `deliver_reply` task, and the walk reaches it (pinned by
    // `stop_reaches_a_reply_woken_turn`). The miss is named below, not dropped.
    if let Some(c) = state
        .current_meta
        .lock()
        .expect("current poisoned")
        .as_mut()
        .filter(|c| c.task_id.as_deref() == Some(task_id.as_str()))
    {
        c.stopped_by = Some(who.clone());
    }
    // ONE CHAIN PER AGENT (#227): stop means stop. The chain ends and the held
    // messages are dropped from the wake list (their rows stay stored); no wake.
    {
        let mut chain = state.chain.lock().expect("chain poisoned");
        chain.alive = false;
        chain.held.clear();
        // Rotate the token: a routed reply still in flight carries the old one (#227 Bug 5).
        chain.token = mint_send_id();
    }
    let reached = kernel.cancel_task(&TaskId(pid)).await;
    // STOP REACHES THE TURN ITSELF (#177). When the task walk did not reach the
    // generation, the turn is still in flight: stop closes it here, so `status`
    // reports `generating: false` the moment this returns, and fires the
    // generation's own abort, which drops its provider stream. Before this, a stop
    // the kernel did not deliver left the turn and its stream running while `stop`
    // answered `stopped: true`.
    if reached["cancelled"] != json!(true) {
        // The kernel's answer is named, not dropped: `{cancelled:false}` means the
        // task id the turn recorded is not in the kernel's task table.
        println!(
            "  [stop] {} kernel cancel did not reach task {pid}: {reached}",
            agent_id.0
        );
    }
    if let Some(mut entry) = crate::state::take_current_for_cancel(agent_id, &task_id) {
        entry.stopped_by = Some(who);
        entry.abort.notify_one();
        if let Err(e) = close_stopped(agent_id, entry, kernel).await {
            return json!({"stopped": true, "error": e.to_string()});
        }
    }
    json!({"stopped": true})
}

/// A turn that ends in a failure stores its own rows (#177): the incoming message
/// and an error row naming why, so the model reads the failure on its next turn and
/// the operator sees it in the history. Nothing of the turn was dispatched.
async fn store_failed_turn(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    text: &str,
    notice: bool,
    reason: &str,
    origin: &str,
    attached: Option<&crate::images::Attached>,
) -> Result<(), String> {
    let in_id = kernel.mint_id();
    let in_parent = crate::agent_loop::incoming_parent(text);
    let error_id = kernel.mint_id();
    let rows = [
        crate::row_attrs::with_source(
            crate::images::stamped(
                crate::agent_loop::incoming_row(text, in_id, in_parent, notice),
                attached,
            ),
            origin,
        ),
        json!({
            "role": "user",
            "kind": "error",
            "content": format!(
                "[turn failed] {reason}. No answer was stored, and nothing this turn meant \
                 to send was dispatched."
            ),
            "id": error_id,
            "parent": in_id,
        }),
    ];
    crate::memory::save_history(agent_id, kernel, &rows)
        .await
        .map(|_| ())
        .map_err(|e| e.0)
}

/// A turn that failed before any pass ran, as the kernel's one refusal shape plus the
/// `client_id` of the send it belonged to.
fn turn_refusal(code: &str, error: String, client_id: &str) -> Value {
    let mut r = fantastic_kernel::refusal::refusal(code, error);
    fantastic_kernel::refusal::with(&mut r, "client_id", json!(client_id));
    r
}

/// The failure a turn reports, with the store's outcome: when its error row could
/// not be stored, the message says so too.
fn with_store_outcome(message: String, stored: Result<(), String>) -> String {
    match stored {
        Ok(()) => message,
        Err(e) => format!("{message}; the turn's error row could not be stored: {e}"),
    }
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
pub async fn history(
    agent_id: &AgentId,
    args: &History,
    kernel: &Arc<Kernel>,
    model_window: i64,
) -> MemResult<Value> {
    let mut head: Option<Value> = None;
    let rows: Vec<Value> = match args.fold {
        Some(fold_id) => {
            let everything = load_history(agent_id, kernel, true).await?;
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
                return Ok(r);
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
        None => load_history(agent_id, kernel, false).await?,
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
        let cards: Vec<Value> = crate::embed::card_rows(agent_id, kernel)
            .await?
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
    Ok(out)
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

/// `recall` verb: page turns back from the DURABLE chat store (the FULL
/// conversation, never trimmed — so anything compaction dropped is one call
/// away). Read-only. args: `query?` (case-insensitive substring), `limit?`
/// (default 20, max 100), `before?` (store index), `id?` (a row id: that row whole).
///
/// The reply is python's and nothing more: `{messages, total, truncated[,
/// note]}`. There is no `client_id` — a model reading its own recall was handed
/// a key python does not send.
pub async fn recall(
    agent_id: &AgentId,
    args: &Recall,
    kernel: &Arc<Kernel>,
    model_window: i64,
) -> MemResult<Value> {
    let full = load_history(agent_id, kernel, true).await?;
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
            return Ok(r);
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
        return Ok(json!({"messages": [m], "total": 1, "truncated": false}));
    }
    let q = args.query.as_deref().map(str::to_lowercase);
    let limit = args.limit as usize;
    let before = args.before;
    let mut indexed: Vec<(usize, &Value)> = full.iter().enumerate().collect();
    if let Some(before) = before {
        indexed.retain(|(i, _)| (*i as i64) < before);
    }
    if let Some(q) = &q {
        indexed.retain(|(_, m)| py_json_dumps(m, true, None).to_lowercase().contains(q.as_str()));
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
    Ok(reply)
}

/// `context_status` verb: the context-budget posture + the last overflow
/// projection + the model's derived reaction to it. Read-only.
pub async fn context_status(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    model_window: i64,
) -> MemResult<Value> {
    let meta = agent_meta(agent_id, kernel);
    let state = state_for(agent_id);
    let last_projection = state
        .projection
        .lock()
        .expect("projection poisoned")
        .clone()
        .unwrap_or(Value::Null);
    let last_reaction = derive_reaction(agent_id, &state, kernel)
        .await?
        .unwrap_or(Value::Null);
    // RAW, as python's `rec.get("context_strategy", DEFAULT_STRATEGY)`: only an
    // ABSENT key means `compact`.
    let strategy = meta
        .get("context_strategy")
        .cloned()
        .unwrap_or_else(|| json!(crate::strategies::DEFAULT_STRATEGY));
    let calib = state.calib.lock().expect("calib poisoned").unwrap_or(1.0);
    let reported = *state.reported.lock().expect("reported poisoned");
    Ok(json!({
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
    }))
}

pub use crate::constants::{send_timeout_secs, SEND_TIMEOUT_SECS};

/// The queue row for one submission, removed when this guard is DROPPED while
/// still armed — i.e. when the dispatch went away before it ever became the
/// in-flight turn.
///
/// A `cancel`/`timeout` landing during the FIFO wait drops the whole `send`
/// future at its await point, and the row it enqueued outlived it: `status`
/// reported a generation nobody was running, forever, and told the next caller it
/// was one deeper in the line than it was.
struct QueuedGuard {
    state: Arc<crate::state::BackendState>,
    send_id: String,
    armed: bool,
}

impl QueuedGuard {
    /// Pop the row and stop guarding it: the wait is over, the caller owns the
    /// lock, and the in-flight entry takes over from here.
    fn disarm(&mut self) {
        self.armed = false;
        Self::dequeue(&self.state, &self.send_id);
    }

    fn dequeue(state: &crate::state::BackendState, send_id: &str) {
        let mut q = state.queue.lock().expect("queue poisoned");
        if let Some(pos) = q.iter().position(|e| e.send_id == send_id) {
            q.remove(pos);
        }
    }
}

impl Drop for QueuedGuard {
    fn drop(&mut self) {
        if self.armed {
            Self::dequeue(&self.state, &self.send_id);
        }
    }
}

/// The owed pids a message settles, settled once: at the point the message is
/// visible where an `await` can see it, or at the latest when `send` returns.
struct OwedSettle {
    agent: String,
    text: Option<String>,
}

impl OwedSettle {
    fn now(&mut self) {
        if let Some(t) = self.text.take() {
            crate::route::settle_owed(&self.agent, &t);
        }
    }
}

impl Drop for OwedSettle {
    fn drop(&mut self) {
        self.now();
    }
}

/// Whether this send is one of the chain's own turns (never held).
fn is_chain_turn(
    state: &crate::state::BackendState,
    agent_id: &AgentId,
    chain_token: &str,
) -> bool {
    (!chain_token.is_empty() && state.chain.lock().expect("chain poisoned").token == chain_token)
        || fantastic_kernel::current_origin().is_some_and(|o| o == *agent_id)
}

/// What `send` will decide about the hold, read early (the chain can still end
/// before the real decision, which is why the settle also runs on every other path).
fn would_hold(state: &crate::state::BackendState, agent_id: &AgentId, args: &crate::declared::Send) -> bool {
    let token = args.chain.as_deref().unwrap_or("");
    !is_chain_turn(state, agent_id, token) && state.chain.lock().expect("chain poisoned").alive
}

/// Hold a message on the agent's chain. The chain may have ended while the row was
/// being stored (the caller read `alive` before that await): the push and the
/// `alive` read are ONE critical section, and a message pushed onto a dead chain
/// revives it and wakes it, or nothing would ever read the message.
pub(crate) fn push_held_or_wake(
    kernel: &Arc<Kernel>,
    agent_id: &AgentId,
    system_prompt: Option<&str>,
    msg: crate::state::HeldMsg,
) {
    let state = state_for(agent_id);
    let revived = {
        let mut chain = state.chain.lock().expect("chain poisoned");
        chain.held.push(msg);
        let dead = !chain.alive;
        if dead {
            chain.alive = true;
            if chain.token.is_empty() {
                chain.token = mint_send_id();
            }
        }
        dead
    };
    if revived {
        crate::route::chain_ended(kernel, agent_id, system_prompt);
    }
}

/// The shared `send` flow: guard the call chain → the memory store named (fail
/// loud) → a `late` reply recorded, never a turn → enqueue → FIFO lock → run the
/// generation under a timeout → clean up → ROUTE the pass's output. The backend
/// supplies a built `provider` (its key is read inside the pass, before each
/// request — [`crate::credentials`]) and its `cfg`.
///
/// The generation runs INLINE in this dispatch — the backend spawns no task of
/// its own. It doesn't need to: a `send` IS a kernel task (`"<agent>:<n>"`, one
/// row in `ps`), and the kernel's `cancel` kills it, running this bundle's
/// `on_cancel` first. That is why there is no `interrupt` verb any more; the
/// backend-private cancellation collapsed onto the substrate's.
///
/// Routing ([`crate::route::route_reply`]) is the one thing that does NOT run
/// inline: it is spawned as its own kernel task after the FIFO guard is dropped,
/// because a `reply_to:self` follow-up must be able to take the guard this
/// dispatch just held. Only a pass that did not error is routed
/// (python `core.py:1183`) — a broken chain must not keep spending turns.
///
/// `Err` is python's RAISE — a provider failure propagates out of the handler,
/// and the kernel settles that execution as FAILED with
/// `{error: "RuntimeError: <msg>", raised: true}`. A refusal and a timeout are
/// both `Ok`: the turn ANSWERED.
pub async fn send(
    provider: Arc<dyn Provider>,
    agent_id: &AgentId,
    args: &crate::declared::Send,
    kernel: &Arc<Kernel>,
    cfg: BackendConfig,
) -> Result<Value, BundleError> {
    // PAUSED: the message is held in the store, in order, and not run (#219).
    if let Some(held) = crate::pause::hold_if_paused(agent_id, args, kernel).await? {
        return Ok(held);
    }
    let mut text = args.text.clone();
    // AN ANSWER UNDER AN INQUIRY'S PID settles what this agent was owed.
    //
    // A message that will be HELD settles only once it is visible as held (see
    // `push_held_or_wake`): settling first drops the pid from `awaiting()` while
    // the message is in no queue, and a concurrent `await` then sees nothing to
    // wait for. Every other path settles here, as before.
    let mut owed = OwedSettle {
        agent: agent_id.as_str().to_string(),
        text: Some(text.clone()),
    };
    if !would_hold(&state_for(agent_id), agent_id, args) {
        owed.now();
    }
    // A MESSAGE FROM ANOTHER AGENT OPENS WITH WHO SENT IT — python's
    // `[from <sender> · to a, b · msg N · re N]` line. Without it a peer's
    // inquiry arrives as bare prose and the receiver cannot tell it is being
    // asked. The operator's own sends carry
    // no header, exactly as in python.
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
            // the stream this message was previewed as — what an AI consumer
            // that watched it stream matches its draft to
            if let Some(s) = &args.stream_id {
                parts.push(format!("stream {s}"));
            }
            text = format!("[{}]\n{text}", parts.join(" \u{b7} "));
        }
    }
    // captured for routing before any shadowing below
    let incoming_text = text.clone();
    // Optional caller-supplied system block (a caller teaches its own tool convention).
    let system_prompt = args.system_prompt.clone();
    // Cloned BEFORE the generation moves it: the follow-up turn must be assembled
    // against the SAME system block this pass ran under, or a caller-driven agent
    // loses its role on hop 2.
    let system_prompt_for_route = system_prompt.clone();
    // Where this pass's TEXT goes when the model issues no call of its own. Once,
    // and it does not propagate — a caller cannot spend the model's turns for it.
    //
    // KEPT RAW. python drops it only when it is not a string or is blank after a
    // FULL strip; the value that travels is the one the caller wrote. Trimming it
    // here silently retargeted `' peer '` at `peer`, so an unknown-agent refusal
    // named an id the caller never sent.
    let send_to = args
        .send_to
        .clone()
        .filter(|s| !py_strip(s).is_empty());
    let client_id = safe_client(&args.client_id);
    // The sender's id, kept in RAM for `status`: the calling agent, else the client.
    let origin_id = fantastic_kernel::current_origin()
        .map_or_else(|| client_id.clone(), |o| o.as_str().to_string());
    // Recursion guard — checked BEFORE the FIFO lock, so a cycle can never
    // DEADLOCK on it (A→B→A would otherwise block on A's held guard). The rule
    // itself is the kernel's, shared with the authored-runtime connectors; only
    // the wording is ours, because the reader here is a model.
    //
    // Read-only on purpose: routing does NOT propagate `_call_stack` onward
    // (python's `_deliver` does not carry it either), so a `reply_to:self` chain
    // never accumulates its own id and is never mistaken for a cycle on turn 2.
    match check_call_stack(agent_id.as_str(), args.call_stack.as_deref().unwrap_or(&[])) {
        Ok(_) => {}
        Err(CallStackRefusal::Cycle(cycle)) => {
            return Ok(json!({
                "error": format!("{}: cycle detected", cfg.name),
                "response": "",
                "cycle": cycle,
                "client_id": client_id,
            }));
        }
        Err(CallStackRefusal::TooDeep) => {
            return Ok(json!({
                "error": format!("{}: max call depth {MAX_CALL_DEPTH} reached", cfg.name),
                "response": "",
                "client_id": client_id,
            }));
        }
    }
    // NO STORE, NO TURN — said up front, not discovered mid-generation.
    if let Err(e) = crate::memory::store_of(agent_id, kernel) {
        let mut r = refusal(MISSING_ARG, e.0);
        with(&mut r, "arg", json!("memory_store_id"));
        with(&mut r, "client_id", json!(client_id));
        return Ok(r);
    }
    if args.late == Some(true) {
        // A reply to a call a PREVIOUS incarnation made (see `route::deliver`):
        // recorded in this agent's own store, never a turn.
        save_late(agent_id, kernel, &args.text).await?;
        return Ok(json!({"late": true, "client_id": client_id}));
    }
    let send_id = mint_send_id();
    let state = state_for(agent_id);
    // WHAT THE PROVIDER CAN DO is read from it when the chat starts and when its provider or
    // model changed, and the images of this send are brought inside the model's limits (#622)
    crate::images::refresh(agent_id, kernel, &state).await;
    let attached = match crate::images::attach(agent_id, kernel, args.images.as_deref()).await {
        Ok(a) => a,
        Err(e) => {
            let mut r = refusal(BAD_ARG, e);
            with(&mut r, "arg", json!("images"));
            with(&mut r, "client_id", json!(client_id));
            return Ok(r);
        }
    };
    // ONE CHAIN PER AGENT (#227): a message that lands while this agent's chain
    // is alive is HELD — stored as a row, read by the chain's next pass — unless
    // it is one of the chain's own turns (it carries the chain token).
    let chain_token = args.chain.clone().unwrap_or_default();
    // A message the agent sends to ITSELF is its own chain and is never held.
    let chain_turn = is_chain_turn(&state, agent_id, &chain_token);
    // An inquiry's answer obeys the hold like any other message: it is read by the
    // chain's next pass, never a second chain of its own.
    let chain_alive = state.chain.lock().expect("chain poisoned").alive;
    if !chain_turn && chain_alive {
        let row_id = kernel.mint_id();
        crate::memory::save_history(
            agent_id,
            kernel,
            // The SAME row the normal incoming path stores (`incoming_row`), so a
            // held message folds and recalls exactly like any other turn (#227).
            &[crate::row_attrs::with_source(
                crate::images::stamped(
                    crate::agent_loop::incoming_row(
                        &text,
                        row_id,
                        crate::agent_loop::incoming_parent(&text),
                        false,
                    ),
                    attached.as_ref(),
                ),
                &origin_id,
            )],
        )
        .await?;
        // A held send RETURNS THE ANSWER of the pass that reads it (#227): its
        // caller waits on the oneshot the answering pass sends into.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        push_held_or_wake(
            kernel,
            agent_id,
            system_prompt_for_route.as_deref(),
            crate::state::HeldMsg {
                client_id: client_id.clone(),
                send_id: send_id.clone(),
                reply: reply_tx,
                origin: origin_id.clone(),
                text: text.clone(),
                queued_at: now_secs(),
                task_id: current_task().map(|t| t.0.to_string()),
                row_id,
            },
        );
        // Only now is the message visible as held: settle what it answers.
        owed.now();
        let mut detail = Map::new();
        detail.insert("send_id".to_string(), json!(send_id));
        detail.insert("ahead".to_string(), json!(0));
        detail.insert("held".to_string(), json!(true));
        emit_status(kernel, &state, agent_id, &client_id, "queued", detail).await;
        let ceiling = send_timeout_secs();
        return Ok(
            match tokio::time::timeout(Duration::from_secs_f64(ceiling), reply_rx).await {
                Ok(Ok(v)) => v,
                Ok(Err(_)) => json!({"held": true, "cancelled": true, "client_id": client_id}),
                Err(_) => turn_refusal(fantastic_kernel::refusal::TIMEOUT, 
                    format!("send: timeout after {}s", py_float_repr(ceiling)),
                    &client_id,
                ),
            },
        );
    }

    // Enqueue.
    owed.now();
    let queued_at = now_secs();
    // A notice is this agent's own harness text: honoured only when the agent sent
    // it to itself (or the operator did), so no peer can hide a message from a
    // person's view. Python `_send`.
    let notice = args.notice == Some(true)
        && fantastic_kernel::current_origin().is_none_or(|s| s == *agent_id);
    let entry = QueuedEntry {
        client_id: client_id.clone(),
        text: text.clone(),
        send_id: send_id.clone(),
        queued_at,
        notice,
        origin: origin_id.clone(),
    };
    // WHO IS AHEAD, counted in the SAME critical section as the push. python's
    // enqueue → `lock.locked()` → acquire runs without yielding, so "the lock is
    // held" there also covers everyone already queued; on a multi-threaded
    // runtime two sends can both push and both see the lock free before either
    // takes it, and neither says `queued` (an event lost, and every later event
    // ordinal shifted with it).
    let prior = {
        let mut q = state.queue.lock().expect("queue poisoned");
        let prior = q.len();
        q.push_back(entry);
        prior
    };
    // ARMED FROM HERE UNTIL THE LOCK IS OURS. A `cancel` (or an item `timeout`)
    // that lands while this dispatch WAITS on the FIFO drops the future at the
    // await below — the queue row would survive it forever, so `status` reported a
    // phantom `mine_pending`/`others_pending` and every later caller's `ahead` was
    // off by one. The guard's Drop removes the row; it is disarmed the moment the
    // row is popped into the in-flight entry.
    let mut queued_guard = QueuedGuard {
        state: Arc::clone(&state),
        send_id: send_id.clone(),
        armed: true,
    };

    // Best-effort contention detection. ONE event, and it is the `status` one —
    // never a bare `{type:'queued'}` ahead of it, an undocumented type python
    // never sends, which would break any per-submission event count and every
    // cross-runtime event-stream diff.
    let contested = prior > 0 || state.lock.try_lock().is_err();
    if contested {
        let ahead = state
            .queue
            .lock()
            .expect("queue poisoned")
            .len()
            .saturating_sub(1);
        let mut detail = Map::new();
        detail.insert("send_id".to_string(), json!(send_id));
        detail.insert("ahead".to_string(), json!(ahead));
        emit_status(kernel, &state, agent_id, &client_id, "queued", detail).await;
    }

    // Acquire FIFO lock.
    let lock_arc = Arc::clone(&state.lock);
    let _guard = lock_arc.lock_owned().await;

    // Pop ourselves from the queue, become the in-flight entry — and disarm the
    // guard, whose job (the wait is over) is done.
    queued_guard.disarm();
    // ONE CHAIN PER AGENT (#227): a routed reply carries the token of the chain it
    // was decided under. If that chain has since ended (a stop, a reboot, a clear,
    // a lost wake) the token no longer matches: the reply is RECORDED, never a
    // turn, so it cannot resume or double the chain.
    let stale =
        !chain_token.is_empty() && state.chain.lock().expect("chain poisoned").token != chain_token;
    if stale {
        save_late(agent_id, kernel, &args.text).await?;
        drop(_guard);
        return Ok(json!({"late": true, "client_id": client_id}));
    }
    // ONE CHAIN PER AGENT (#227): this pass owns the chain. A new chain mints its
    // token; this pass TAKES the held messages (it reads their rows) and answers
    // each of them with its own reply below.
    let answering = {
        let mut chain = state.chain.lock().expect("chain poisoned");
        if !chain.alive {
            chain.token = mint_send_id();
        }
        chain.alive = true;
        std::mem::take(&mut chain.held)
    };
    // Fired by `stop` ([`stop`]) — the generation below watches it.
    let abort = Arc::new(tokio::sync::Notify::new());
    {
        let mut cur = state.current_meta.lock().expect("current poisoned");
        *cur = Some(CurrentEntry {
            client_id: client_id.clone(),
            text: text.clone(),
            send_id: send_id.clone(),
            // PROMOTED, not rebuilt — python's `{**entry, …}` carries the queue
            // row's `queued_at` into the in-flight entry, and `status` reports it.
            queued_at,
            origin: origin_id.clone(),
            started_at: now_secs(),
            phase: "thinking".to_string(),
            text_so_far: String::new(),
            last_tool: None,
            // The kernel task this generation IS — how `cancel` reaches it, and
            // how `on_cancel` recognises its own turn.
            task_id: current_task().map(|t| t.0.to_string()),
            parts: None,
            stopped_by: None,
            notice,
            attached: attached.clone(),
            abort: Arc::clone(&abort),
        });
    }
    emit_status(kernel, &state, agent_id, &client_id, "thinking", Map::new()).await;

    // Run the generation inline, bounded by the per-send ceiling. Cancellation
    // is the KERNEL's (`cancel <task_id>` → `on_cancel` → this future is
    // dropped, releasing the provider stream and the FIFO guard) — the backend
    // holds no task handle of its own.
    let timeout_secs = send_timeout_secs();
    // TRANSPORT RETRIES (#198 part 2): a pass that failed on the link is re-run,
    // visibly, up to the agent's `transport_retries` (0 = off).
    let retries = crate::record::ai::RECORD
        .get(&agent_meta(agent_id, kernel), "transport_retries")
        .and_then(|v| v.as_i64())
        .expect("transport_retries declares its default") as u32;
    let mut attempt: u32 = 0;
    let outcome = loop {
        let r = tokio::select! {
            biased;
            // STOPPED: `stop` closed this turn already, and the generation below is
            // dropped — its provider stream with it (#177).
            _ = abort.notified() => None,
            r = tokio::time::timeout(
                Duration::from_secs_f64(timeout_secs),
                run_generation(
                    &provider,
                    agent_id,
                    &state,
                    &text,
                    kernel,
                    &client_id,
                    cfg,
                    system_prompt.clone(),
                ),
            ) => Some(r),
        };
        match r {
            Some(Ok(Err(GenFailure::Provider(ref pe)))) if pe.transport && attempt < retries => {
                attempt += 1;
                let message = pe.message.clone();
                // A visible row: the model and the operator both read the retry.
                let _ = crate::memory::save_history(
                    agent_id,
                    kernel,
                    &[json!({
                        "role": "user",
                        "parent": Value::Null,
                        "kind": "notice",
                        "content": format!(
                            "[retry {attempt}/{retries}] the link dropped ({message}) — retrying the turn"
                        ),
                    })],
                )
                .await;
                let mut detail = Map::new();
                detail.insert("attempt".to_string(), json!(attempt));
                detail.insert("of".to_string(), json!(retries));
                detail.insert("error".to_string(), json!(message));
                emit_status(kernel, &state, agent_id, &client_id, "retrying", detail).await;
                // Backoff 2 s, 4 s, 8 s …; tests use a few milliseconds.
                let wait = if cfg!(test) {
                    Duration::from_millis(5)
                } else {
                    Duration::from_secs(1u64 << attempt)
                };
                let stopped = tokio::select! {
                    biased;
                    _ = abort.notified() => true,
                    _ = tokio::time::sleep(wait) => false,
                };
                if stopped {
                    break None;
                }
            }
            other => break other,
        }
    };
    let Some(outcome) = outcome else {
        // Nothing is routed and nothing is stored here: `stop` wrote the marker row
        // and the `done` status. Clear the entry the stop left, release the lock.
        *state.current_meta.lock().expect("current poisoned") = None;
        // ONE CHAIN PER AGENT (#227): a stopped chain ends and wakes nothing.
        {
            let mut chain = state.chain.lock().expect("chain poisoned");
            chain.alive = false;
            chain.held.clear();
            chain.token = mint_send_id();
        }
        drop(_guard);
        return Ok(json!({"cancelled": true, "client_id": client_id}));
    };

    let mut message_id: Option<i64> = None;
    // Every closed call part `(stream_id, body)` — routing stamps each dispatched
    // message with the stream it was previewed as.
    let mut call_streams: Vec<(String, String)> = Vec::new();
    // A pass the model stopped with a block open dispatches NOTHING.
    let mut cut = false;
    // A DEAD turn (only thinking) is discarded unstored and routed as a silent
    // turn — the reminder rerolls it, told why.
    let mut dead_turn = false;
    // A provider failure python does NOT map propagates out of the handler. The
    // raise is deferred to after the cleanup below (python does it from an
    // `except` whose `finally` clears the current entry and releases the lock).
    let mut raise: Option<(&'static str, String)> = None;
    // A failed pass (not a refused record) earns its own retry turn (#533).
    let mut failed_reason: Option<String> = None;
    let reply = match outcome {
        Ok(Ok(Generation::Dead { text: final_text })) => {
            dead_turn = true;
            // python `_run`'s dead-turn reply: no `message_id` — nothing was
            // stored, so there is no row to name.
            json!({
                "response": final_text,
                "client_id": client_id,
                "dead_turn": true,
            })
        }
        Ok(Ok(Generation::Answered {
            text: final_text,
            message_id: tid,
            call_streams: streams,
            cut: was_cut,
        })) => {
            message_id = Some(tid);
            cut = was_cut;
            // `message_id` IS PART OF THE REPLY — python `_run`'s last statement.
            // Rust computed it (routing needs it to parent the next turn) and
            // then dropped it on the floor, so a caller could not name the row
            // its own send produced.
            let reply = json!({
                "response": final_text,
                "client_id": client_id,
                "message_id": tid,
            });
            // The closed call parts feed routing only — never the caller's reply.
            call_streams = streams;
            reply
        }
        // THE SEAM'S OWN WORDS. A projection refusal never opened a provider
        // pass, so no `status(done)` closes it and no `client_id` is added.
        Ok(Err(GenFailure::Projection(refusal))) => refusal,
        // FAIL FAST: the store refused or is gone — no turn, said so.
        Ok(Err(GenFailure::Store(e))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(e.0.clone()));
                m
            })
            .await;
            turn_refusal(fantastic_kernel::refusal::STORE_REFUSED, e.0, &client_id)
        }
        // FAIL LOUD: a system_prompt_file is set and unusable — no turn on another
        // block, said so.
        Ok(Err(GenFailure::PromptFile(e))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(e.0.clone()));
                m
            })
            .await;
            turn_refusal(fantastic_kernel::refusal::INVALID_META, e.0, &client_id)
        }
        // FAIL LOUD: a fold's summary would not fit its budget
        // after every re-summarise — no turn on an unfolded view, said so.
        Ok(Err(GenFailure::FoldSummary(e))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(e.0.clone()));
                m
            })
            .await;
            turn_refusal(fantastic_kernel::refusal::LIMIT, e.0, &client_id)
        }
        // A FOLD THAT CANNOT BE PLACED (python `member_anchor`'s `ValueError`,
        // through `_send`'s `except Exception`): the turn fails, reported on
        // `status(done, reason='error')`, then raised — a FAILED execution, never
        // a turn that dies in silence.
        Ok(Err(GenFailure::FoldAnchor(e))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(e.0.clone()));
                m
            })
            .await;
            raise = Some(("ValueError", e.0));
            Value::Null
        }
        // PROVIDER FAILURE (python `core.py`'s `except Exception`):
        // `status(done, reason='error', error=str(e))` announces it, then python
        // re-raises, and a raise is a FAILED execution — which is what makes
        // `retries` live and dead-lettering possible. A completed
        // `{error, client_id}` would leave no AI failure retryable.
        Ok(Err(GenFailure::Provider(pe))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(pe.message.clone()));
                m
            })
            .await;
            // A FAILED PASS stores its own error row (#177): nothing ends silently. A
            // REFUSED RECORD (`ValueError`: the record cannot run) never opened a pass
            // and stores nothing, as ever.
            let message = if pe.kind == "ValueError" {
                pe.message
            } else {
                let stored = store_failed_turn(
                    agent_id,
                    kernel,
                    &incoming_text,
                    notice,
                    &pe.message,
                    &origin_id,
                    attached.as_ref(),
                )
                .await;
                failed_reason = Some(pe.message.clone());
                with_store_outcome(pe.message, stored)
            };
            raise = Some((pe.kind, message));
            Value::Null
        }
        Err(_elapsed) => {
            // THE DETAIL IS `reason` AND THE AUTO-FILLS, nothing else — python
            // `_emit_status(..., "done", reason="timeout")`. The
            // `partial_chars`/`partial_head`/`partial_tail` keys rust added were
            // a rust-only shape on the one event a UI reads to close a turn.
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("timeout"));
                m
            })
            .await;
            let message = format!("send: timeout after {}s", py_float_repr(timeout_secs));
            // The send ceiling ends the turn with an error row too (#177).
            let stored = store_failed_turn(
                agent_id,
                kernel,
                &incoming_text,
                notice,
                &message,
                &origin_id,
                attached.as_ref(),
            )
            .await;
            turn_refusal(fantastic_kernel::refusal::TIMEOUT, with_store_outcome(message, stored), &client_id)
        }
    };

    // Clear current_meta FIRST: the follow-up turn establishes its own entry, and
    // an `emit_status` on it must not attach to this dead one.
    *state.current_meta.lock().expect("current poisoned") = None;
    // Release the FIFO guard SECOND, before routing is spawned — a self-addressed
    // follow-up takes this very guard, and the contention probe above must see a
    // free lock rather than reporting the chain as a queued rival.
    drop(_guard);

    // ONE CHAIN PER AGENT (#227): a pass that is NOT routed ends its chain here,
    // so messages held meanwhile are woken once (see `chain_ended`).
    // A FAILED TURN IS RETRIED (#533): the chain stays alive and its own next turn
    // is delivered, under the silent-turn bound; at the bound the chain gives up.
    let retrying = match &failed_reason {
        Some(why) => {
            !crate::route::failed_turn_retry(
                kernel,
                agent_id,
                &client_id,
                send_to.as_deref(),
                system_prompt_for_route.as_deref(),
                why,
            )
            .await
        }
        None => false,
    };
    if !retrying && (reply.get("error").is_some() || raise.is_some()) {
        crate::route::chain_ended(kernel, agent_id, system_prompt_for_route.as_deref());
    }

    // ONE CHAIN PER AGENT (#227): the pass that read the held messages answers
    // each of them with its own reply, under that sender's own client_id.
    let answer = match &raise {
        Some((kind, msg)) => {
            let mut v = json!({"error": msg, "client_id": client_id});
            // A refused record (`ValueError`) is the caller's own fault, not a link
            // or provider failure: it carries no transport code.
            if *kind != "ValueError" {
                fantastic_kernel::refusal::with(
                    &mut v,
                    "reason",
                    json!(fantastic_kernel::refusal::TRANSPORT_ERROR),
                );
            }
            v
        }
        None => reply.clone(),
    };
    for held in answering {
        let mut v = answer.clone();
        if let Some(o) = v.as_object_mut() {
            o.insert("client_id".to_string(), json!(held.client_id));
        }
        let _ = held.reply.send(v);
    }

    // The raise happens HERE, after the cleanup python does in its `finally`:
    // the entry is cleared and the lock is free before the failure leaves this
    // dispatch, or the next caller inherits a phantom in-flight turn.
    if let Some((kind, msg)) = raise {
        return Err(Box::new(PythonRaise(kind, msg)));
    }

    // ROUTE this pass's output where it was addressed. DETACHED as its own kernel
    // task, which is the whole reason chaining works: `state.lock` is a
    // non-reentrant `tokio::sync::Mutex`, so awaiting a self-addressed send inline
    // — even here, after the drop — would keep THIS dispatch from returning to its
    // caller until the whole chain finished, and any hop that re-entered before the
    // drop would stall until the 180s ceiling.
    //
    // `Kernel::spawn_task` and not a bare `tokio::spawn`: it survives the parent
    // dispatch, is a row in `ps`, stays cancellable via the subtree walk, and
    // scopes `CURRENT_TASK` so the turns the chain spawns nest under it. Each turn
    // is individually killable and visible; a cancel does NOT reliably kill the
    // chain past the first hop (orphans are re-parented, exactly as in python).
    if reply.get("error").is_none() {
        let text = reply
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let route_kernel = Arc::clone(kernel);
        let route_self = agent_id.clone();
        let route_client = client_id.clone();
        // What this turn ANSWERED — tells routing whether it was a chain turn
        // (a delivered reply) and so whether a silent turn earns the nudge.
        let route_incoming = incoming_text.clone();
        // THE ROUTED CALLS REACH THEIR TARGETS BEFORE THIS REPLY GOES BACK
        // (python's event loop order): released at launch,
        // not at completion — a self-addressed call fans out before it needs the
        // lock, so this never waits on this agent.
        let launched = Arc::new(fantastic_kernel::Signal::default());
        let route_launched = Arc::clone(&launched);
        let deadline = crate::route::route_deadline(kernel);
        kernel.spawn_task(agent_id, "route_reply", async move {
            crate::route::route_reply(
                route_self,
                route_kernel,
                text,
                route_client,
                send_to,
                system_prompt_for_route,
                route_incoming,
                // THE ROUTE DEFAULT DEADLINE (root `send_timeout`, else the env /
                // 600 s), threaded to every call the router dispatches. The
                // kernel defaults nothing. `timeout_secs` stays the generation
                // ceiling.
                deadline,
                cfg.model_window,
                message_id,
                dead_turn,
                call_streams,
                cut,
                Some(route_launched),
            )
            .await;
        });
        launched.wait().await;
    }
    Ok(reply)
}

/// A handler RAISE with python's exception text. The kernel settles a bundle
/// `Err` as `{error: <Display>, raised: true}`, and python's settle spells that
/// `f"{type(e).__name__}: {e}"` — a provider failure is a `RuntimeError`, a
/// record field the backend cannot run without a `ValueError`.
#[derive(Debug)]
struct PythonRaise(&'static str, String);

impl std::fmt::Display for PythonRaise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0, self.1)
    }
}

impl std::error::Error for PythonRaise {}

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

/// Accept an `inquiry` (FULLY ASYNC): register it as open on
/// its own chain, run that chain detached, and answer at once
/// `{accepted, re, request_id}`. The answer the model gives with `inquiry_answer`
/// in ANY turn of that chain — or the news that it ended without one — reaches
/// the asker later as a message under this call's pid (`crate::route`).
pub fn inquiry_accept(
    agent_id: &AgentId,
    args: &Inquiry,
    text: &str,
    kernel: &Arc<Kernel>,
) -> Value {
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
                asker,
                pid,
                reply_client,
                request_id: request_id.clone(),
            },
        );
    crate::route::deliver_soon(
        kernel,
        agent_id.as_str(),
        text,
        &client,
        args.system_prompt.as_deref(),
        false,
    );
    json!({
        "accepted": true,
        "re": task.map_or(Value::Null, |t| json!(t.0)),
        "request_id": request_id,
    })
}

// ── boot, the row event, context by grant ───────────────────────────────────

/// `boot` — LAZY: never touches the memory store. Checks the record names one
/// (fail loud otherwise), starts a new incarnation, and marks the view for
/// rebuilding — the first turn reads the head from the store and runs
/// `context_recall`. Python `_make_boot`.
pub fn boot(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    crate::state::bump_incarnation(agent_id);
    // ONE CHAIN PER AGENT (#227): a new incarnation starts with no chain. Held
    // senders are dropped here, so their callers get the cancelled shape (Bug 2).
    state_for(agent_id)
        .chain
        .lock()
        .expect("chain poisoned")
        .reset("send: the agent rebooted before a turn read this message");
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
    F: Fn(&str) -> Arc<dyn Provider>,
    R: FnOnce() -> Value,
{
    use crate::declared::chat::Call;
    let reply = match call {
        Call::Reflect(_) => reflect(),
        Call::Boot(_) => boot(agent_id, kernel),
        Call::Send(a) => {
            let client = safe_client(&a.client_id);
            send(provider_for(&client), agent_id, &a, kernel, cfg).await?
        }
        // A receiver of this agent's streaming parts stops the generation.
        Call::Stop(a) => stop(agent_id, a.stream_id.as_deref(), kernel).await,
        // What the agent HOLDS in memory — its loaded head, never a store read.
        Call::Rows(a) => rows(agent_id, &a, kernel),
        // ONE ASK VERB, WHOEVER IS ASKED: a person answers `inquiry` from a
        // record, this agent answers it by THINKING, and the asker writes the
        // same call either way.
        Call::Inquiry(a) => {
            let text = inquiry_text(&a);
            inquiry_accept(agent_id, &a, &text, kernel)
        }
        Call::History(a) => history(agent_id, &a, kernel, cfg.model_window).await?,
        // A CARD in the chat: a host agent's panel, shown inline (#339).
        Call::Embed(a) => crate::embed::embed(agent_id, &a.id, kernel).await?,
        Call::Unembed(a) => crate::embed::unembed(agent_id, &a.id, kernel).await?,
        Call::Status(a) => status(agent_id, &a),
        Call::ContextStatus(_) => context_status(agent_id, kernel, cfg.model_window).await?,
        Call::Recall(a) => recall(agent_id, &a, kernel, cfg.model_window).await?,
        // FIND the line anywhere in memory — rows, shared rows, bodies held this run.
        Call::Grep(a) => crate::retrieval::grep(agent_id, &a, kernel, cfg.model_window).await?,
        // Rebuild the view from the store: on boot, after folds, on request.
        Call::ContextRecall(a) => {
            let p = provider_for(&crate::declared::default_client_id());
            crate::retrieval::context_recall(
                agent_id,
                a.dry == Some(true),
                kernel,
                Some(&p),
                cfg.model_window,
                None,
                None,
            )
            .await?
        }
        // A new thread on the same store: the view starts here, nothing is deleted.
        Call::ContextClear(a) => {
            crate::retrieval::context_clear(agent_id, a.why.as_deref(), kernel, cfg.model_window)
                .await?
        }
        // Fold a chosen stretch of the view into one fold row — the operator's mark.
        Call::ContextFold(a) => {
            let p = provider_for(&crate::declared::default_client_id());
            crate::retrieval::context_fold(agent_id, &a, kernel, &p, cfg.model_window).await?
        }
        // Compact the store: drop the rows the folds already summarise.
        Call::ContextForget(a) => {
            crate::retrieval::context_forget(agent_id, a.include_cleared == Some(true), kernel)
                .await?
        }
        // A NEW agent from this one's first rows; a mark to fork from; this agent's marks.
        Call::Fork(a) => crate::fork::fork(agent_id, &a, kernel).await?,
        Call::Mark(a) => crate::fork::mark(agent_id, &a, kernel).await?,
        Call::ForkPoints(a) => crate::fork::fork_points(agent_id, &a, kernel).await?,
        // THE ROW EVENT: this agent announces its own rows to its watchers.
        Call::Watch(a) => watch(agent_id, &a, kernel).await?,
        Call::Unwatch(a) => unwatch(agent_id, &a, kernel),
        Call::Pause(a) => crate::pause::pause(agent_id, a.reason.as_deref(), kernel).await?,
        Call::Resume(a) => crate::pause::resume(agent_id, a.note.as_deref(), kernel).await?,
    };
    Ok(reply)
}
