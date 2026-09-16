//! Verb entrypoints the backends dispatch through. A backend's `Bundle`
//! does only its own pre-checks (file_bridge_id, api_key) + provider
//! build, then calls these. The provider is the per-backend seam,
//! constructed by the backend and passed in per `send`.

use crate::agent_loop::{run_generation, BackendConfig, GenFailure};
use crate::context::{budget as agent_budget, output_reserve, resolve_context_window};
use crate::events::emit_status;
use crate::helpers::{agent_meta, mint_send_id, now_secs, safe_client, DEFAULT_CLIENT_ID};
use crate::history::load_history;
use crate::projection::derive_reaction;
use crate::provider::Provider;
use crate::state::{state_for, status_snapshot, CurrentEntry, QueuedEntry};
use fantastic_bundle::{
    py_float, py_float_repr, py_int_or, py_json_dumps, py_repr, py_repr_value, py_str, py_strip,
    py_truthy,
};
use fantastic_kernel::bundle::BundleError;
use fantastic_kernel::call_guard::{check_call_stack, CallStackRefusal, MAX_CALL_DEPTH};
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
        "send": "args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller — cli (stdout) for client_id='cli', or the browser tab whose WS is subscribed and filters by client_id otherwise. Persists per-client chat.json. Per-backend FIFO lock: concurrent callers serialize. If the lock is held when this call arrives, emits a `status` event (phase='queued', detail.ahead) for the caller; first `token` (or `status` of phase != queued) for the same client_id implicitly unqueues. Optional system_prompt:str REPLACES the auto-built prompt (caller-supplied role/context — read it from a state agent yourself, no yaml coupling here); optional messages:list REPLACES persisted history (fully stateless). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Optional send_to:str hands this pass's TEXT to that agent id as a fresh turn, once. The model routes its own work with `<reply_to>` inside its calls. Returns {response, final, client_id}.",
        "inquiry": "args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?, client_id:str?. Sends THIS agent an inquiry and returns {answer, request_id} — the answer is the one it emits with the `answer` tool. The same verb a human answers; a sender writes one call for either.",
        "history": "args: limit:int? (default: as many as fit inline), after:str? (a row id — page forward from it). The conversation at DIGEST resolution — {id, parent, kind, role, head, bytes} per row, oldest first, paged. Bodies are behind `pull`; a row's text is behind `recall`. Returns {rows, next, total}.",
        "status": "args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}.",
        "context_status": "No args. The context-budget posture + the last overflow projection + the model's",
        "recall": "args: query?:str (case-insensitive substring over the serialized turn — matches tool args/replies too), limit?:int (default 20, max 100), before?:int (store index — page backward). Pages turns back from the DURABLE store (the FULL conversation, never trimmed — so anything compaction dropped is one call away). Read-only. A page always fits INLINE (the cap); trimmed from the oldest end when it would not, with the index to continue from. Returns {messages:[{index,role,content}], total, truncated, note?}.",
        "pull": "args: pid:str (req — the pid on a settled reply's tag), offset:int? (0), length:int? (chars; default and max = your inline cap). Reads ONE PAGE of a reply body that was too large to inline. Returns {pid, offset, text, next_offset, eof} — page with `offset: next_offset` until `eof`.",
        "grep": "args: query:str (req — case-insensitive substring), ids:list[int]? (row ids / pids / fold ids to search; default: every row of this conversation and every stored reply body it received), limit:int? (matches, default: as many as fit inline), width:int? (chars of each matched line returned, from its start; default 200 — raise it when the value you need sits deep in a long line). Finds the LINE that mentions something, wherever it is — a stored body you never pulled, a digest line, an old turn. Returns {matches:[{id, line, text}], total, truncated}.",
        "context_recall": "args: dry:bool? (mechanical fit only — no model call). Rebuild this agent's VIEW from its store: fit the head to the budget at digest resolution, then ask the model which rows it wants in full, persist the id list, log the decision as a row, and drop RAM scratch. Runs on boot, after a fold batch, or on request. Returns {kept, expanded, dropped, budget, why, degraded}.",
        "boot": "On boot, an agent with a store REBUILDS ITS VIEW — `context_recall`. A fresh",
    })
}

/// The events every ai_core backend announces — python's `reflect.emits`,
/// verbatim. `done` is NOT one of them: the phase machine's `done` rides on
/// `status`.
#[must_use]
pub fn reflect_emits() -> Value {
    json!({
        "token": "{type:'token', text:str, source, client_id} — streaming chunk. Routed ONLY to the caller: cli (stdout) when client_id='cli', else this agent's own inbox (browser filters by client_id).",
        "status": "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail:{send_id, started_at, queue_depth, ...phase_specific}} — single channel for phase transitions. detail.tool={call_id,target,verb,args,reply_preview?} for tool_calling (entry has no reply_preview, exit re-emits same call_id with it). detail.reason='ok'|'cancelled'|'timeout'|'error' for done. detail.ahead for queued.",
        "context": "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail:{...}} — the context protocol's push half. compacted: detail={strategy, dropped_turns, kept_turns, summarized} when the live view was compacted to fit the window. too_small: detail={context_window, system_tokens, hint} when even the system block + one turn won't fit (the model is NOT called — a failfast). Pull counterpart: the `context_status` verb.",
    })
}

/// python's `reflect.concurrency` line, verbatim.
pub const CONCURRENCY_DOC: &str = "Per-backend FIFO lock around `send`: one generation at a time. Other callers wait (and receive a `queued` event so their UI can show it). `reflect`/`history`/`status` skip the lock and stay snappy.";
/// The reflect body EVERY ai_core backend answers with — python's
/// `_make_reflect`, key for key and in its order: `id, sentence, model,
/// endpoint, context_window, context_strategy, verbs, generating, cancel,
/// emits, concurrency`. A backend adds only its own `reflect_extra` keys, which
/// land after `concurrency` exactly as python's `out.update(extra)` does.
///
/// `model`/`endpoint`/`context_strategy` are the RECORD's values, raw: python
/// reads them with `rec.get(key, default)`, so only an ABSENT key takes the
/// default and a `null` on the record reflects back as `null`.
#[must_use]
pub fn reflect_base(
    agent_id: &AgentId,
    kernel: &Kernel,
    sentence: &str,
    default_model: &str,
    default_endpoint: &str,
) -> Map<String, Value> {
    let meta = agent_meta(agent_id, kernel);
    let mut out = Map::new();
    out.insert("id".to_string(), json!(agent_id.as_str()));
    out.insert("sentence".to_string(), json!(sentence));
    out.insert(
        "model".to_string(),
        meta.get("model")
            .cloned()
            .unwrap_or_else(|| json!(default_model)),
    );
    out.insert(
        "endpoint".to_string(),
        meta.get("endpoint")
            .cloned()
            .unwrap_or_else(|| json!(default_endpoint)),
    );
    out.insert(
        "context_window".to_string(),
        json!(resolve_context_window(&meta)),
    );
    out.insert(
        "context_strategy".to_string(),
        meta.get("context_strategy")
            .cloned()
            .unwrap_or_else(|| json!(crate::strategies::DEFAULT_STRATEGY)),
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

/// `history` verb — the conversation at DIGEST resolution.
///
/// ONE SHAPE: `{rows, next, total}`, python's. `rows` is `{id, parent, kind,
/// role, head, bytes}` per row, heads capped at 120 code points, bodies behind
/// `pull` and text behind `recall` — that is what a MODEL should be handed, and
/// what python returns and nothing else. The rust-only `messages` (the whole
/// `{role, content}` list) and `client_id` keys are gone: a model reading them
/// got the full transcript back inside a verb whose whole point is that it does
/// not.
pub async fn history(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let rows = load_history(agent_id, kernel, DEFAULT_CLIENT_ID).await;
    let mut digest: Vec<Value> = rows
        .iter()
        .filter(|r| r.get("id").is_some())
        .map(|r| {
            // python `str(r.get("content", ""))`: an ABSENT content is the empty
            // string, a null one is `None` (4 bytes), a dict one is its repr.
            let content = r.get("content").cloned().unwrap_or_else(|| json!(""));
            let text = py_str(&content);
            json!({
                "id": r.get("id"),
                "parent": r.get("parent"),
                // `kind` FALLS BACK TO `role` on PYTHON TRUTHINESS (`kind or
                // role`), so ''/null/0 fall through and a non-string kind is
                // kept raw.
                "kind": match r.get("kind") {
                    Some(k) if py_truthy(k) => k.clone(),
                    _ => r.get("role").cloned().unwrap_or(Value::Null),
                },
                "role": r.get("role"),
                // 120 CODE POINTS, then newlines flattened — a head is for
                // recognising a row, not for reading it.
                "head": text.chars().take(120).collect::<String>().replace('\n', " "),
                "bytes": text.chars().count(),
            })
        })
        .collect();
    // python `int(payload.get("limit") or 0)` under a try/except → 0.
    let null = Value::Null;
    let limit = py_int_or(payload.get("limit").unwrap_or(&null), 0).unwrap_or(0);
    if let Some(after) = payload.get("after").and_then(crate::retrieval::as_id) {
        // AN UNKNOWN CURSOR PAGES NOTHING, rather than starting over: silently
        // re-serving the head would read as "no more rows" to a caller that had
        // already seen them.
        digest = match digest.iter().position(|d| d["id"].as_i64() == Some(after)) {
            Some(i) => digest.split_off(i + 1),
            None => Vec::new(),
        };
    }
    let total = rows.len();
    // THE CAP INVARIANT, as python's `_make_history`: a digest page must answer
    // UNDER the inline cap or it is stored and dehydrated and the model pulls
    // its own history. `limit` bounds the count; the cap bounds the bytes.
    let meta = agent_meta(agent_id, kernel);
    let (kept, truncated) = crate::context::fit_reply(
        &digest,
        crate::context::inline_cap(&meta),
        (limit != 0).then_some(limit),
    );
    let next = truncated
        .then(|| kept.last().map(|d| d["id"].clone()))
        .flatten()
        .unwrap_or(Value::Null);
    json!({ "rows": kept, "next": next, "total": total })
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
pub async fn recall(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let full = load_history(agent_id, kernel, DEFAULT_CLIENT_ID).await;
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
    let cap = crate::context::inline_cap(&agent_meta(agent_id, kernel));
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
    reply
}

/// `context_status` verb: the context-budget posture + the last overflow
/// projection + the model's derived reaction to it. Read-only.
pub async fn context_status(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let meta = agent_meta(agent_id, kernel);
    let state = state_for(agent_id);
    let last_projection = state
        .projection
        .lock()
        .expect("projection poisoned")
        .clone()
        .unwrap_or(Value::Null);
    let last_reaction = derive_reaction(agent_id, &state, kernel)
        .await
        .unwrap_or(Value::Null);
    // RAW, as python's `rec.get("context_strategy", DEFAULT_STRATEGY)`: only an
    // ABSENT key means `compact`. `as_str().unwrap_or("compact")` reported a
    // null/numeric strategy as `"compact"` — a fallback that hid a broken record
    // from the model AND from the projection, which refuses that same value.
    let strategy = meta
        .get("context_strategy")
        .cloned()
        .unwrap_or_else(|| json!(crate::strategies::DEFAULT_STRATEGY));
    json!({
        "context_window": resolve_context_window(&meta),
        "output_reserve": output_reserve(&meta),
        "budget": agent_budget(&meta),
        "strategy": strategy,
        "last_projection": last_projection,
        "last_reaction": last_reaction,
        "view_source": state.view_source.lock().expect("view_source poisoned").clone(),
    })
}

/// Default per-`send` ceiling, in seconds — overridable via the
/// `FANTASTIC_AI_SEND_TIMEOUT` env (see [`send_timeout_secs`]). Mirrors the
/// python/swift default; a slow LOCAL model can raise it via the env.
pub const SEND_TIMEOUT_SECS: f64 = 180.0;

/// The effective per-`send` ceiling: `FANTASTIC_AI_SEND_TIMEOUT` if set, else
/// [`SEND_TIMEOUT_SECS`]. python `SEND_TIMEOUT = float(os.environ.get(
/// "FANTASTIC_AI_SEND_TIMEOUT", "180"))`.
///
/// A FLOAT, and PARSED LIKE PYTHON'S. `0.05` used to parse as `u64` and fail,
/// silently falling back to 180 — so a test that set a sub-second ceiling waited
/// three minutes, and the ceiling printed in the refusal (`180s`) was not the
/// one python prints (`180.0s`). A value python's `float()` rejects is a
/// MISCONFIGURATION and fails loud NAMING the variable, exactly as python's
/// import-time `float()` raises; there is no fallback to the default.
pub fn send_timeout_secs() -> f64 {
    let Ok(raw) = std::env::var("FANTASTIC_AI_SEND_TIMEOUT") else {
        return SEND_TIMEOUT_SECS;
    };
    match py_float(&json!(raw)) {
        Ok(v) => v,
        Err(e) => panic!("FANTASTIC_AI_SEND_TIMEOUT is not a number: {e}"),
    }
}

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

/// The shared `send` flow: guard the call chain → enqueue → FIFO lock → run the
/// generation under a timeout → clean up → ROUTE the pass's output. The backend
/// supplies a built `provider` (it has already failfast-checked file_bridge_id /
/// api_key) and its `cfg`.
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
/// `Err` is python's RAISE — an unmapped provider failure propagates out of the
/// handler, and the kernel settles that execution as FAILED with
/// `{error: "RuntimeError: <msg>", raised: true}`. A mapped failure, a refusal
/// and a timeout are all `Ok`: the turn ANSWERED.
pub async fn send(
    provider: Option<Arc<dyn Provider>>,
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
    // A caller-supplied `messages` list REPLACES the persisted history (fully
    // stateless — this agent holds no per-call state for it).
    let messages_override = payload.get("messages").and_then(Value::as_array).cloned();
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
    // A BACKEND WHOSE PROVIDER IS A PRECONDITION FAILFASTS HERE — python
    // `_send`'s `if cfg.require_provider`, AFTER the recursion guard and with
    // the caller's `client_id` on the refusal. nvidia's unset `NVIDIA_API_KEY`
    // is the only case; rust used to answer it from the backend BEFORE the
    // guard and without the `client_id`.
    let Some(provider) = provider else {
        return Ok(json!({
            "error": cfg
                .provider_missing_error
                .map_or_else(|| format!("{}: provider unavailable", cfg.name), str::to_string),
            "client_id": client_id,
        }));
    };
    crate::route::trace_pub(&format!(
        "send verb ENTER agent={} client={} text_len={}",
        agent_id.as_str(),
        client_id,
        text.len()
    ));
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
    state.queue.lock().expect("queue poisoned").push_back(entry);
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
    let contested = state.lock.try_lock().is_err();
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
            messages_override,
        ),
    )
    .await;

    let mut turn_id: Option<i64> = None;
    // A provider failure python does NOT map propagates out of the handler. The
    // raise is deferred to after the cleanup below (python does it from an
    // `except` whose `finally` clears the current entry and releases the lock).
    let mut raise: Option<String> = None;
    let reply = match outcome {
        Ok(Ok((final_text, tid))) => {
            turn_id = Some(tid);
            // `turn_id` IS PART OF THE REPLY — python `_run`'s last statement.
            // Rust computed it (routing needs it to parent the next turn) and
            // then dropped it on the floor, so a caller could not name the row
            // its own send produced.
            json!({
                "response": final_text,
                "final": final_text,
                "client_id": client_id,
                "turn_id": tid,
            })
        }
        // THE SEAM'S OWN WORDS. A projection refusal never opened a provider
        // pass, so no `status(done)` closes it and no `client_id` is added.
        Ok(Err(GenFailure::Projection(refusal))) => refusal,
        // PROVIDER FAILURE, TWO KINDS (python `core.py`'s `except Exception`).
        // Either way `status(done, reason='error', error=str(e))` announces it.
        // The backend's mapper turns nvidia's HTTP-status answers into a clean
        // sentence the caller READS; everything else python re-raises, and a
        // raise is a FAILED execution — which is what makes `retries` live and
        // dead-lettering possible. Rust used to answer every failure with a
        // completed `{error, client_id}`, so no AI failure was ever retryable.
        Ok(Err(GenFailure::Provider(pe))) => {
            emit_status(kernel, &state, agent_id, &client_id, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(pe.message.clone()));
                m
            })
            .await;
            if pe.mapped {
                json!({"error": pe.message, "client_id": client_id})
            } else {
                raise = Some(pe.message);
                Value::Null
            }
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
    if let Some(msg) = raise {
        return Err(Box::new(PythonRaise(msg)));
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
                turn_id,
            )
            .await;
        });
    }
    Ok(reply)
}

/// A handler RAISE with python's exception text. The kernel settles a bundle
/// `Err` as `{error: <Display>, raised: true}`, and python's settle spells that
/// `f"{type(e).__name__}: {e}"` — every provider failure python raises is a
/// `RuntimeError`, so that is the name a caller reads.
#[derive(Debug)]
struct PythonRaise(String);

impl std::fmt::Display for PythonRaise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RuntimeError: {}", self.0)
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
