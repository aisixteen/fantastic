//! Verb entrypoints the backends dispatch through. A backend's `Bundle`
//! does only its own provider build (from the record's fields), then calls
//! these. The provider is the per-backend seam,
//! constructed by the backend and passed in per `send`.

use crate::agent_loop::{run_generation, BackendConfig, GenFailure, Generation};
use crate::constants::MAX_CALL_DEPTH;
use crate::context::{budget as agent_budget, output_reserve, resolve_context_window};
use crate::events::emit_status;
use crate::helpers::{agent_meta, mint_send_id, now_secs, safe_client, DEFAULT_CLIENT_ID};
use crate::memory::{load_history, MemResult};
use crate::projection::{derive_reaction, round3, TRIGGER};
use crate::provider::Provider;
use crate::state::{state_for, status_snapshot, CurrentEntry, QueuedEntry};
use fantastic_bundle::{
    py_float_repr, py_int_or, py_json_dumps, py_repr, py_repr_value, py_str, py_strip, py_truthy,
};
use fantastic_kernel::bundle::BundleError;
use fantastic_kernel::call_guard::{check_call_stack, CallStackRefusal};
use fantastic_kernel::{current_task, AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// What every backend's `reflect` says INSTEAD of an `interrupt` verb — the
/// substrate's universal cancel, taught in one line so a caller (or an LLM
/// reading the reflect) needs no bundle-private knowledge to stop a turn.
pub const CANCEL_DOC: &str = "No `interrupt` verb — an in-flight `send` IS a kernel task. \
Kill it with the substrate's universal system verbs: send 'kernel' <type>ps</type> for the live \
task table, find the row {agent:'<this agent>', verb:'send'}, then send 'kernel' \
<type>cancel</type> <task_id>n</task_id>. The turn closes with status(phase='done', \
reason='cancelled') + done, its HTTP stream and FIFO lock are released, and the blocked caller's \
`send` resolves to {cancelled:true}. Cancel kills the task's whole SUBTREE, so any work the turn \
spawned dies with it.";

/// Python's `reflect.verbs` for every ai_core backend — the FIRST LINE of each
/// verb's docstring, in `build`'s order, verbatim. ONE map, every backend: the
/// wording never differed per provider in python, and a backend that wrote its
/// own copy taught the model a surface that did not exist.
#[must_use]
pub fn reflect_verbs() -> Value {
    json!({
        "reflect": "Identity + model + endpoint + generating flag + context posture. No args.",
        "send": "args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller — cli (stdout) for client_id='cli', or the browser tab whose WS is subscribed and filters by client_id otherwise. Persists per-client chat.json. Per-backend FIFO lock: concurrent callers serialize. If the lock is held when this call arrives, emits a `status` event (phase='queued', detail.ahead) for the caller; first `token` (or `status` of phase != queued) for the same client_id implicitly unqueues. Optional system_prompt:str REPLACES the auto-built prompt (caller-supplied role/context — read it from a state agent yourself, no yaml coupling here). The conversation is always this agent's own (its memory store). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Optional send_to:str hands this pass's TEXT to that agent id as a fresh turn, once. The model routes its own work with `<reply_to>` inside its calls. Returns {response, final, client_id}.",
        "inquiry": "args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?, client_id:str?. Sends THIS agent an inquiry and returns {answer, request_id} — the answer is the one it emits with the `answer` tool. The same verb a human answers; a sender writes one call for either.",
        "history": "args: fold:int? (a fold or clear id — list ITS members instead), limit:int? (default: as many as fit inline), after:int? (a row id — page forward from it). The conversation at DIGEST resolution — {id, parent, kind, role, head, bytes} per row, oldest first, paged. With `fold`, that fold's title + summary and its members (one level down; a member that is itself a fold opens the same way). A row's text is behind `recall` / `grep`. Returns {rows, next, total, fold?}.",
        "status": "args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}.",
        "context_status": "No args. The context-budget posture + the last overflow projection + the model's",
        "recall": "args: query?:str (case-insensitive substring over the serialized turn — matches tool args/replies too), limit?:int (default 20, max 100), before?:int (store index — page backward). Pages turns back from the DURABLE store (the FULL conversation, never trimmed — so anything compaction dropped is one call away). Read-only. A page always fits INLINE (the cap); trimmed from the oldest end when it would not, with the index to continue from. Returns {messages:[{index,role,content}], total, truncated, note?}.",
        "grep": "args: query:str (req — case-insensitive substring), ids:list[int]? (row ids / pids / fold ids to search; a fold id searches its members), limit:int? (matches, default: as many as fit inline), width:int? (chars of each matched line returned, from its start; default 200 — raise it when the value you need sits deep in a long line). Finds the LINE that mentions something ANYWHERE in your memory — every row you ever wrote or received (behind every clear, inside every fold), and the reply bodies still held in memory this run. Returns {matches:[{id, line, text}], total, truncated}.",
        "context_recall": "args: dry:bool? (mechanical fit only — no model call). Rebuild this agent's VIEW from its store: fit the head to the budget at digest resolution, then ask the model which rows it wants in full, persist the id list, log the decision as a row, and drop RAM scratch. Runs on boot, after a fold batch, or on request. Returns {kept, expanded, dropped, budget, why, degraded}.",
        "context_clear": "args: why:str? — start a NEW THREAD on the same store: a CLEAR-FOLD — one row whose members are every row since the previous clear (no summary) — and the model's view begins at its `[cleared <id> · N rows]` line. Nothing is deleted or forgotten: grep, recall and `history {fold: <id>}` reach every member. Empties the persisted view and drops RAM scratch (projection, fold counters, pending recall). Returns {cleared:id, rows:N}.",
        "watch": "args: body:bool? (default false), watcher:str? (operator only — an inbox id to deliver to). Announce every row this agent appends to YOUR inbox as {type:'row', source, row:{id, kind, role, bytes}} — content-free; with body:true the row's `content` too. Needs a registered inbox. Returns {watching:true, watcher, body}.",
        "unwatch": "args: watcher:str? (operator only). Stop announcing rows to you. Returns {watching:false}.",
        "boot": "LAZY: never touches the memory store. Checks the record names one (fail loud otherwise), starts a new incarnation, and marks the view for rebuilding — the first turn reads the head from the store and runs `context_recall`.",
    })
}

/// The events every ai_core backend announces — python's `reflect.emits`,
/// verbatim. `done` is NOT one of them: the phase machine's `done` rides on
/// `status`.
#[must_use]
pub fn reflect_emits() -> Value {
    json!({
        "token": "{type:'token', text:str, source, client_id} — streaming chunk. Routed ONLY to the caller: cli (stdout) when client_id='cli', else this agent's own inbox (browser filters by client_id).",
        "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'done', detail:{send_id, started_at, queue_depth, ...phase_specific}} — single channel for phase transitions. detail.reason='ok'|'cancelled'|'timeout'|'error'|'awaiting'|'dead_turn' (only thinking — discarded, rerolled)|'gave_up' (MAX_SILENT_TURNS turns in a row with no call; detail.error says so) for done. detail.ahead for queued.",
        "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the context protocol's push half. compacted: detail={strategy, dropped_messages, kept_messages, summarized} when the live view was compacted to fit the window. too_small: detail={context_window, system_tokens, hint} when even the system block + one turn won't fit (the model is NOT called — a failfast). Pull counterpart: the `context_status` verb.",
    })
}

/// python's `reflect.concurrency` line, verbatim.
pub const CONCURRENCY_DOC: &str = "Per-backend FIFO lock around `send`: one generation at a time. Other callers wait (and receive a `queued` event so their UI can show it). `reflect`/`history`/`status` skip the lock and stay snappy.";
/// The reflect body EVERY ai_core backend answers with — python's
/// `_make_reflect`, key for key and in its order: `id, sentence, model,
/// endpoint, context_window, context_strategy, memory_store_id, verbs,
/// generating, cancel, emits, concurrency`. A backend adds only its own `reflect_extra` keys, which
/// land after `concurrency` exactly as python's `out.update(extra)` does.
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
    out.insert("verbs".to_string(), reflect_verbs());
    // A generation IS its kernel dispatch task — the backend holds no task
    // handle of its own. An in-flight entry present ⇔ generating.
    out.insert(
        "generating".to_string(),
        json!(crate::state::is_generating(agent_id)),
    );
    out.insert("cancel".to_string(), json!(CANCEL_DOC));
    out.insert("emits".to_string(), reflect_emits());
    out.insert("concurrency".to_string(), json!(CONCURRENCY_DOC));
    out
}

/// A scalar as python prints it in a header: a string bare, anything else as JSON.
fn scalar_text(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

/// `status` verb.
pub fn status(agent_id: &AgentId, payload: &Value) -> Value {
    status_snapshot(agent_id, payload)
}

/// The `on_delete` bundle hook, SHARED by every backend (each one's hook calls
/// it, like [`on_cancel`]; vllm drops its own table too). Drops every row this
/// crate keeps for the deleted id, so an id re-created later starts clean and
/// nothing leaks (audit ARCH2-2): the FIFO lock and queue, the in-flight entry,
/// the projection and its marks, the fold and recall counters, the calibration
/// (the state slot), the routing count, the silent streaks, the strip set, and
/// this kernel's head, bodies, grants and watchers. Python `on_delete`.
///
/// NOT the incarnation: it stays monotonic across a re-create, so a reply to a
/// call the deleted agent made never becomes a live turn of the new one.
pub fn on_delete(agent_id: &AgentId, kernel: &Kernel) {
    crate::state::drop_state(agent_id);
    crate::route::forget(agent_id.as_str());
    crate::strategies::strip::forget(agent_id.as_str());
    crate::memory::forget(agent_id, kernel);
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
pub async fn on_cancel(agent_id: &AgentId, task_id: &str, kernel: &Arc<Kernel>) {
    let Some(entry) = crate::state::take_current_for_cancel(agent_id, task_id) else {
        return;
    };
    let state = state_for(agent_id);
    let client_id = entry.client_id;
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("cancelled"));
    detail.insert("send_id".to_string(), json!(entry.send_id));
    // THE ENTRY IS ALREADY TAKEN, so `emit_status`'s auto-fill has nothing left
    // to read: python passes `started_at` explicitly from the entry it took, and
    // rust's cancel detail was the one `done` event missing it.
    detail.insert("started_at".to_string(), json!(entry.started_at));
    emit_status(kernel, &state, agent_id, &client_id, "done", detail).await;
}

/// One row at DIGEST resolution — python `_digest`: `{id, parent, kind, role,
/// head, bytes}`, the head capped at 120 code points with newlines flattened.
fn digest(r: &Value) -> Value {
    // python `str(r.get("content", ""))`: an ABSENT content is the empty string, a
    // null one is `None` (4 bytes), a dict one is its repr.
    let content = r.get("content").cloned().unwrap_or_else(|| json!(""));
    let text = py_str(&content);
    json!({
        "id": r.get("id"),
        "parent": r.get("parent"),
        // `kind` FALLS BACK TO `role` on PYTHON TRUTHINESS (`kind or role`).
        "kind": match r.get("kind") {
            Some(k) if py_truthy(k) => k.clone(),
            _ => r.get("role").cloned().unwrap_or(Value::Null),
        },
        "role": r.get("role"),
        "head": text.chars().take(120).collect::<String>().replace('\n', " "),
        "bytes": text.chars().count(),
    })
}

/// `history` verb — the conversation at DIGEST resolution, or, with `fold`, that
/// fold's title + summary and its members (one level down; a member that is
/// itself a fold opens the same way). Python `_history`.
///
/// ONE SHAPE: `{rows, next, total, fold?}`. Bodies are behind `recall` / `grep`,
/// never here.
pub async fn history(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    model_window: i64,
) -> MemResult<Value> {
    let fold_raw = payload.get("fold").filter(|v| !v.is_null());
    let mut head: Option<Value> = None;
    let rows: Vec<Value> = match fold_raw {
        Some(raw) => {
            let Some(fold_id) = crate::retrieval::as_id(raw) else {
                return Ok(json!({
                    "error": "history: `fold` must be a fold id (int)",
                    "reason": "bad_arg",
                    "arg": "fold",
                }));
            };
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
                return Ok(json!({
                    "error": format!("history: {fold_id} is not a fold in your conversation"),
                    "reason": "not_found",
                    "arg": "fold",
                }));
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
                "summary": f.get("body").filter(|b| py_truthy(b)).cloned().unwrap_or(json!("")),
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
        .filter(|r| r.get("id").is_some_and(py_truthy))
        .map(digest)
        .collect();
    // python `int(payload.get("limit") or 0)` under a try/except → 0.
    let null = Value::Null;
    let limit = py_int_or(payload.get("limit").unwrap_or(&null), 0).unwrap_or(0);
    if let Some(after) = payload.get("after").and_then(crate::retrieval::as_id) {
        // AN UNKNOWN CURSOR PAGES NOTHING, rather than starting over.
        digests = match digests.iter().position(|d| d["id"].as_i64() == Some(after)) {
            Some(i) => digests.split_off(i + 1),
            None => Vec::new(),
        };
    }
    let total = rows.len();
    // THE CAP INVARIANT: a digest page must answer UNDER the inline cap.
    let meta = agent_meta(agent_id, kernel);
    let (kept, truncated) = crate::context::fit_reply(
        &digests,
        crate::context::inline_cap(&meta, model_window),
        (limit != 0).then_some(limit),
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
/// WHOLE, NOT CAPPED. Rust/swift truncated each turn at 2000 chars until
/// 2026-09-02 while python returned it verbatim — same verb, same store, two
/// different replies, and the cut half is exactly what a model pages back FOR.
///
/// FROM THE CONTENT, NEVER THE ROW: a string content is itself (an EMPTY one
/// included), and anything else is `json.dumps(content, default=str)` — so
/// `{"k": "é"}` renders as `{"k": "\u00e9"}`. Serializing the whole row when the
/// content was empty handed the model its own bookkeeping.
fn recall_render(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        other => py_json_dumps(other.unwrap_or(&Value::Null), true, None),
    }
}

/// `recall` verb: page turns back from the DURABLE chat store (the FULL
/// conversation, never trimmed — so anything compaction dropped is one call
/// away). Read-only. args: `query?` (case-insensitive substring), `limit?`
/// (default 20, max 100), `before?` (store index).
///
/// The reply is python's and nothing more: `{messages, total, truncated[,
/// note]}`. There is no `client_id` — a model reading its own recall was handed
/// a key python does not send.
pub async fn recall(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    model_window: i64,
) -> MemResult<Value> {
    let full = load_history(agent_id, kernel, true).await?;
    let null = Value::Null;
    // python `str(payload.get("query") or "").lower().strip()`.
    let raw_q = payload.get("query").unwrap_or(&null);
    let q_owned = if py_truthy(raw_q) {
        py_str(raw_q)
    } else {
        String::new()
    }
    .to_lowercase();
    let q = py_strip(&q_owned);
    // python `int(payload.get("limit") or 20)` under a try/except → 20.
    let limit = py_int_or(payload.get("limit").unwrap_or(&null), 20)
        .unwrap_or(20)
        .clamp(1, 100) as usize;
    // ANY JSON INT (python `isinstance(before, int) and not isinstance(before,
    // bool)`) — a negative one pages nothing, a float is not an index.
    let before = match payload.get("before") {
        Some(Value::Number(n)) if !n.is_f64() => n.as_i64(),
        _ => None,
    };
    let mut indexed: Vec<(usize, &Value)> = full.iter().enumerate().collect();
    if let Some(before) = before {
        indexed.retain(|(i, _)| (*i as i64) < before);
    }
    if !q.is_empty() {
        indexed.retain(|(_, m)| py_json_dumps(m, true, None).to_lowercase().contains(q));
    }
    let total = indexed.len();
    let truncated = total > limit;
    let mut page: Vec<&(usize, &Value)> = indexed[total.saturating_sub(limit)..].iter().collect();
    // A PAGE MUST FIT THE WINDOW IT IS GOING INTO. Measured 2026-09-05 (python):
    // the default 20 turns of 6.8KB reads came back as one 14,786-token reply
    // against a 12,000 window; `too_small` fired as a failfast and the chain
    // died. The page is trimmed from the OLDEST end (the newest of what was
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
        let note = if !q.is_empty() && !full.is_empty() {
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
    payload: &Value,
    kernel: &Arc<Kernel>,
    cfg: BackendConfig,
) -> Result<Value, BundleError> {
    let mut text = match payload.get("text") {
        Some(Value::String(s)) => s.clone(),
        // The tag grammar types a bare number; a number is text too (python).
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    // A MESSAGE FROM ANOTHER AGENT OPENS WITH WHO SENT IT — python's
    // `[from <sender> · to a, b · msg N · re N]` line. Without it a peer's
    // inquiry arrives as bare prose and the receiver cannot tell it is being
    // asked (measured 2026-09-12: six straight `peer_review:rust` runs whose
    // reviewer never emitted `inquiry_answer`). The operator's own sends carry
    // no header, exactly as in python.
    if let Some(sender) = fantastic_kernel::current_origin() {
        if sender != *agent_id {
            let mut parts = vec![format!("from {}", sender.as_str())];
            if let Some(to) = payload
                .get("to")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
            {
                let names: Vec<String> = to.iter().map(scalar_text).collect();
                parts.push(format!("to {}", names.join(", ")));
            }
            if let Some(m) = payload.get("msg").filter(|v| !v.is_null()) {
                parts.push(format!("msg {}", scalar_text(m)));
            }
            if let Some(r) = payload.get("re").filter(|v| !v.is_null()) {
                parts.push(format!("re {}", scalar_text(r)));
            }
            text = format!("[{}]\n{text}", parts.join(" \u{b7} "));
        }
    }
    // captured for routing before any shadowing below
    let incoming_text = text.clone();
    // Optional caller-supplied system block (a caller teaches its own tool convention).
    let system_prompt = payload
        .get("system_prompt")
        .and_then(Value::as_str)
        .map(str::to_string);
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
    let send_to = payload
        .get("send_to")
        .and_then(Value::as_str)
        .filter(|s| !py_strip(s).is_empty())
        .map(str::to_string);
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_CLIENT_ID),
    );
    // Recursion guard — checked BEFORE the FIFO lock, so a cycle can never
    // DEADLOCK on it (A→B→A would otherwise block on A's held guard). The rule
    // itself is the kernel's, shared with the authored-runtime connectors; only
    // the wording is ours, because the reader here is a model.
    //
    // Read-only on purpose: routing does NOT propagate `_call_stack` onward
    // (python's `_deliver` does not carry it either), so a `reply_to:self` chain
    // never accumulates its own id and is never mistaken for a cycle on turn 2.
    match check_call_stack(agent_id.as_str(), payload) {
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
        return Ok(json!({"error": e.0, "reason": "missing_arg", "client_id": client_id}));
    }
    if payload.get("late").is_some_and(py_truthy) {
        // A reply to a call a PREVIOUS incarnation made (see `route::deliver`):
        // recorded in this agent's own store, never a turn.
        let raw = payload.get("text").map_or_else(String::new, py_str);
        crate::memory::save_history(
            agent_id,
            kernel,
            &[json!({
                "role": "user",
                "parent": Value::Null,
                "kind": "late",
                "content": format!(
                    "[late reply — answered a call made before this agent last booted; \
                     recorded, not acted on]\n{raw}"
                ),
            })],
        )
        .await?;
        return Ok(json!({"late": true, "client_id": client_id}));
    }
    let send_id = mint_send_id();
    let state = state_for(agent_id);

    // Enqueue.
    let queued_at = now_secs();
    let entry = QueuedEntry {
        client_id: client_id.clone(),
        text: text.clone(),
        send_id: send_id.clone(),
        queued_at,
    };
    // WHO IS AHEAD, counted in the SAME critical section as the push. python's
    // enqueue → `lock.locked()` → acquire runs without yielding, so "the lock is
    // held" there also covers everyone already queued; on a multi-threaded
    // runtime two sends can both push and both see the lock free before either
    // takes it, and neither said `queued` (measured, parity 2026-09-30: the
    // `cycle` case lost its `status queued` event, and every later event ordinal
    // shifted with it).
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

    // Best-effort contention detection. ONE event, and it is the `status` one:
    // a bare `{type:'queued'}` rode ahead of it on rust/swift until 2026-09-02,
    // an undocumented extra type python never sends, which broke any per-submission
    // event count and every cross-runtime event-stream diff.
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
    {
        let mut cur = state.current_meta.lock().expect("current poisoned");
        *cur = Some(CurrentEntry {
            client_id: client_id.clone(),
            text: text.clone(),
            send_id: send_id.clone(),
            // PROMOTED, not rebuilt — python's `{**entry, …}` carries the queue
            // row's `queued_at` into the in-flight entry, and `status` reports it.
            queued_at,
            started_at: now_secs(),
            phase: "thinking".to_string(),
            text_so_far: String::new(),
            last_tool: None,
            // The kernel task this generation IS — how `cancel` reaches it, and
            // how `on_cancel` recognises its own turn.
            task_id: current_task().map(|t| t.0.to_string()),
        });
    }
    emit_status(kernel, &state, agent_id, &client_id, "thinking", Map::new()).await;

    // Run the generation inline, bounded by the per-send ceiling. Cancellation
    // is the KERNEL's (`cancel <task_id>` → `on_cancel` → this future is
    // dropped, releasing the provider stream and the FIFO guard) — the backend
    // holds no task handle of its own.
    let timeout_secs = send_timeout_secs();
    let outcome = tokio::time::timeout(
        Duration::from_secs_f64(timeout_secs),
        run_generation(
            &provider,
            agent_id,
            &state,
            &text,
            kernel,
            &client_id,
            cfg,
            system_prompt,
        ),
    )
    .await;

    let mut message_id: Option<i64> = None;
    // A DEAD turn (only thinking) is discarded unstored and routed as a silent
    // turn — the reminder rerolls it, told why.
    let mut dead_turn = false;
    // A provider failure python does NOT map propagates out of the handler. The
    // raise is deferred to after the cleanup below (python does it from an
    // `except` whose `finally` clears the current entry and releases the lock).
    let mut raise: Option<(&'static str, String)> = None;
    let reply = match outcome {
        Ok(Ok(Generation::Dead { text: final_text })) => {
            dead_turn = true;
            // python `_run`'s dead-turn reply: no `message_id` — nothing was
            // stored, so there is no row to name.
            json!({
                "response": final_text,
                "final": final_text,
                "client_id": client_id,
                "dead_turn": true,
            })
        }
        Ok(Ok(Generation::Answered {
            text: final_text,
            message_id: tid,
        })) => {
            message_id = Some(tid);
            // `message_id` IS PART OF THE REPLY — python `_run`'s last statement.
            // Rust computed it (routing needs it to parent the next turn) and
            // then dropped it on the floor, so a caller could not name the row
            // its own send produced.
            json!({
                "response": final_text,
                "final": final_text,
                "client_id": client_id,
                "message_id": tid,
            })
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
            json!({"error": e.0, "reason": "memory_store", "client_id": client_id})
        }
        // PROVIDER FAILURE (python `core.py`'s `except Exception`):
        // `status(done, reason='error', error=str(e))` announces it, then python
        // re-raises, and a raise is a FAILED execution — which is what makes
        // `retries` live and dead-lettering possible. Rust used to answer every
        // failure with a completed `{error, client_id}`, so no AI failure was
        // ever retryable.
        Ok(Err(GenFailure::Provider(pe))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(pe.message.clone()));
                m
            })
            .await;
            raise = Some((pe.kind, pe.message));
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
            json!({
                "error": format!("send: timeout after {}s", py_float_repr(timeout_secs)),
                "client_id": client_id,
            })
        }
    };

    // Clear current_meta FIRST: the follow-up turn establishes its own entry, and
    // an `emit_status` on it must not attach to this dead one.
    *state.current_meta.lock().expect("current poisoned") = None;
    // Release the FIFO guard SECOND, before routing is spawned — a self-addressed
    // follow-up takes this very guard, and the contention probe above must see a
    // free lock rather than reporting the chain as a queued rival.
    drop(_guard);

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
        kernel.spawn_task(agent_id, "route_reply", async move {
            crate::route::route_reply(
                route_self,
                route_kernel,
                text,
                route_client,
                send_to,
                system_prompt_for_route,
                route_incoming,
                // THE LAYER'S PER-CALL DEADLINE, threaded to every model call the
                // router dispatches. The kernel defaults nothing.
                timeout_secs,
                cfg.model_window,
                message_id,
                dead_turn,
            )
            .await;
        });
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

/// Open an `inquiry`: validate it and render it as the `send` payload that runs.
///
/// ONE INQUIRY VERB, WHOEVER RECEIVES IT. A person answers `inquiry` from a record and
/// a peer AI answers it by THINKING; the sender writes the same call either way
/// and reads the answer off the same reply. Before this an AI sent an inquiry
/// answered `unknown type` — so "consult whoever can answer" was true only for
/// humans, and inquiry between agents had no shape at all.
///
/// `Err` is the refusal to return as-is.
pub fn inquiry_open(payload: &Value) -> Result<Value, Value> {
    let q = payload.get("text").and_then(Value::as_str).unwrap_or("");
    if q.trim().is_empty() {
        return Err(json!({
            "error": "inquiry: `text` must be a non-empty string",
            "reason": "missing_arg",
            "arg": "text",
        }));
    }
    // The inquiry IS the turn. `options`/`kind` ride in the text because a model
    // reads prose, not a schema; `action` is carried verbatim so an answering
    // agent can see what it is being asked to endorse.
    let mut text = q.to_string();
    if let Some(opts) = payload.get("options").and_then(Value::as_array) {
        if !opts.is_empty() {
            // python `", ".join(str(o) for o in opts)`: `True` reads `True`, a
            // structure reads its repr. JSON (`true`) was a second dialect in a
            // sentence a person reads.
            let rendered: Vec<String> = opts.iter().map(py_str).collect();
            text.push_str("\n\nOptions: ");
            text.push_str(&rendered.join(", "));
        }
    }
    if let Some(act) = payload.get("action").filter(|v| v.is_object()) {
        // python `json.dumps(act)` — `, `/`: ` separators and ensure_ascii.
        text.push_str("\n\nProposed action: ");
        text.push_str(&py_json_dumps(act, true, None));
    }
    let mut send_payload = payload.clone();
    if let Some(o) = send_payload.as_object_mut() {
        o.insert("type".into(), json!("send"));
        o.insert("text".into(), json!(text));
    }
    Ok(send_payload)
}

/// Close an `inquiry`: read the answer the model DECIDED on out of its turn.
///
/// Taking the turn's prose would make every stray sentence an answer and would
/// put the runtime back in the business of reading minds off text.
/// `inquiry_answer` is a control call in the same class as `done`: the model
/// says what its answer IS. A turn that never emits one did not answer, and the
/// asker is told so rather than handed a paragraph to parse.
pub fn inquiry_close(out: Value, payload: &Value) -> Value {
    if out.get("error").is_some() {
        return out;
    }
    let text = out.get("response").and_then(Value::as_str).unwrap_or("");
    let said = crate::tool_parse::extract_tool_calls(text)
        .into_iter()
        .rfind(|c| c.name == "inquiry_answer");
    match said {
        None => json!({
            "error": "you were sent an inquiry and did not answer it: emit \
                      <call name=\"inquiry_answer\"><answer>what you concluded\
                      </answer></call>",
            "reason": "unhandled",
            "request_id": payload.get("request_id"),
        }),
        Some(c) => {
            // THE RAW VALUE, python `args.get("answer")`: a non-string is a
            // refusal in its own right, and the refusal RENDERS what came back
            // with python's `repr` — `42`, `None`, `'...'`. Rust coerced a
            // non-string to `""` and printed rust's `Debug`, so a model that
            // answered `42` was told its answer came back as `""`.
            let answer = c.arguments.get("answer").cloned().unwrap_or(Value::Null);
            // THE PLACEHOLDER IS NOT AN ANSWER. The refusal above SHOWS the
            // shape, and a model that could not answer once will copy that
            // example back verbatim on the retry — measured 2026-09-04 on
            // peer_review, where "..." was written into review.md as the verdict
            // and scored as a reviewer that had judged. Same trap as
            // `send_binary`'s literal "@body", and it earns the same refusal.
            //
            // `strip()` then `strip(".")`: python's whitespace set first, then
            // dots off BOTH ends — so `'. x'` is a real answer and `'...'` is not.
            let said = answer
                .as_str()
                .filter(|s| !py_strip(s).trim_matches('.').is_empty());
            match said {
                None => json!({
                    "error": format!(
                        "that is the PLACEHOLDER, not an answer — `answer` came back as {}. Say what you actually concluded.",
                        py_repr_value(&answer)
                    ),
                    "reason": "bad_arg",
                    "arg": "answer",
                    "request_id": payload.get("request_id"),
                }),
                Some(a) => json!({
                    "answer": a,
                    "request_id": payload.get("request_id"),
                }),
            }
        }
    }
}

// ── boot, the row event, context by grant ───────────────────────────────────

/// `boot` — LAZY: never touches the memory store. Checks the record names one
/// (fail loud otherwise), starts a new incarnation, and marks the view for
/// rebuilding — the first turn reads the head from the store and runs
/// `context_recall`. Python `_make_boot`.
pub fn boot(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    crate::state::bump_incarnation(agent_id);
    if let Err(e) = crate::memory::store_of(agent_id, kernel) {
        return json!({"error": e.0, "reason": "missing_arg", "arg": "memory_store_id"});
    }
    // A new incarnation reads its store afresh; bodies are this run's only.
    crate::memory::forget_head(agent_id, kernel);
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
pub async fn watch(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> MemResult<Value> {
    let caller = fantastic_kernel::current_origin();
    let watcher = match &caller {
        None => match payload.get("watcher").and_then(Value::as_str) {
            Some(w) if !w.is_empty() => w.to_string(),
            _ => {
                return Ok(json!({
                    "error": "watch: the operator must name a `watcher` inbox",
                    "reason": "missing_arg",
                    "arg": "watcher",
                }))
            }
        },
        Some(c) => {
            let named = payload.get("watcher").filter(|w| !w.is_null());
            if named.is_some_and(|w| w.as_str() != Some(c.as_str())) {
                return Ok(json!({
                    "error": "watch: an agent watches for ITSELF — `watcher` is the operator's",
                    "reason": "unauthorized",
                }));
            }
            c.0.clone()
        }
    };
    if !kernel
        .inboxes
        .contains_key(&AgentId::from(watcher.as_str()))
    {
        return Ok(json!({
            "error": format!(
                "watch: {} has no registered inbox, so nothing would read the row events",
                py_repr(&watcher)
            ),
            "reason": "no_consumer",
            "hint": format!(
                "register the reader first — ctx.ensure_inbox({}) — then watch",
                py_repr(&watcher)
            ),
        }));
    }
    let body = payload.get("body").is_some_and(py_truthy);
    crate::memory::add_watcher(agent_id, kernel, &watcher, body);
    Ok(json!({"watching": true, "watcher": watcher, "body": body}))
}

/// `unwatch` — stop announcing rows to the caller. Python `_unwatch`.
pub fn unwatch(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let watcher = match fantastic_kernel::current_origin() {
        Some(c) => Value::from(c.0),
        None => payload.get("watcher").cloned().unwrap_or(Value::Null),
    };
    if let Some(w) = watcher.as_str() {
        crate::memory::remove_watcher(agent_id, kernel, w);
    }
    json!({"watching": false, "watcher": watcher})
}

/// EVERY VERB AN ai_core BACKEND ANSWERS the same way — python `build()`'s one
/// `VERBS` table — except `reflect`, which carries each backend's own extras, and
/// the unknown-type refusal, which names the backend. `None` = not one of these.
///
/// `provider_for(client_id)` builds the backend's provider — always: a record
/// missing what a pass needs raises from inside that pass, naming the field.
pub async fn handle_common<F>(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    cfg: BackendConfig,
    provider_for: F,
) -> Result<Option<Value>, BundleError>
where
    F: Fn(&str) -> Arc<dyn Provider>,
{
    let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let reply = match verb {
        "boot" => boot(agent_id, kernel),
        "send" => send(provider_for(&client_id), agent_id, payload, kernel, cfg).await?,
        // ONE ASK VERB, WHOEVER IS ASKED: a person answers `inquiry` from a
        // record, this agent answers it by THINKING, and the asker writes the
        // same call either way.
        "inquiry" => match inquiry_open(payload) {
            Err(refusal) => refusal,
            Ok(sp) => {
                let out = send(provider_for(&client_id), agent_id, &sp, kernel, cfg).await?;
                inquiry_close(out, payload)
            }
        },
        "history" => history(agent_id, payload, kernel, cfg.model_window).await?,
        "status" => status(agent_id, payload),
        "context_status" => context_status(agent_id, kernel, cfg.model_window).await?,
        "recall" => recall(agent_id, payload, kernel, cfg.model_window).await?,
        // FIND the line anywhere in memory — rows, shared rows, bodies held this run.
        "grep" => crate::retrieval::grep(agent_id, payload, kernel, cfg.model_window).await?,
        // Rebuild the view from the store: on boot, after folds, on request.
        "context_recall" => {
            let p = provider_for(DEFAULT_CLIENT_ID);
            crate::retrieval::context_recall(
                agent_id,
                payload,
                kernel,
                Some(&p),
                cfg.model_window,
                None,
            )
            .await?
        }
        // A new thread on the same store: the view starts here, nothing is deleted.
        "context_clear" => {
            crate::retrieval::context_clear(agent_id, payload, kernel, cfg.model_window).await?
        }
        // CONTEXT BY GRANT: start from another agent's shared rows; ask an owner.
        // THE ROW EVENT: this agent announces its own rows to its watchers.
        "watch" => watch(agent_id, payload, kernel).await?,
        "unwatch" => unwatch(agent_id, payload, kernel),
        _ => return Ok(None),
    };
    Ok(Some(reply))
}
