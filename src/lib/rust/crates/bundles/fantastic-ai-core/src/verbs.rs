//! Verb entrypoints the backends dispatch through. A backend's `Bundle`
//! does only its own pre-checks (file_bridge_id, api_key) + provider
//! build, then calls these. The provider is the per-backend seam,
//! constructed by the backend and passed in per `send`.

use crate::agent_loop::{run_generation, BackendConfig};
use crate::context::{budget as agent_budget, output_reserve, resolve_context_window};
use crate::events::emit_status;
use crate::helpers::{agent_meta, mint_send_id, now_secs, safe_client, DEFAULT_CLIENT_ID};
use crate::history::load_history;
use crate::projection::derive_reaction;
use crate::provider::Provider;
use crate::state::{state_for, status_snapshot, CurrentEntry, QueuedEntry};
use fantastic_kernel::call_guard::{check_call_stack, CallStackRefusal, MAX_CALL_DEPTH};
use fantastic_kernel::{current_task, AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// What every backend's `reflect` says INSTEAD of an `interrupt` verb — the
/// substrate's universal cancel, taught in one line so a caller (or an LLM
/// reading the reflect) needs no bundle-private knowledge to stop a turn.
pub const CANCEL_DOC: &str = "No `interrupt` verb — an in-flight `send` IS a kernel task. \
Kill it with the substrate's universal system verbs: send 'kernel' {type:'ps'} for the live task \
table, find the row {agent:'<this agent>', verb:'send'}, then send 'kernel' \
{type:'cancel', task_id:'<agent>:<n>'}. The turn closes with status(phase='done', \
reason='cancelled') + done, its HTTP stream and FIFO lock are released, and the blocked caller's \
`send` resolves to {cancelled:true}. Cancel kills the task's whole SUBTREE, so any work the turn \
spawned dies with it.";

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
pub async fn on_cancel(
    agent_id: &AgentId,
    task_id: &str,
    kernel: &Arc<Kernel>,
    route: crate::events::CallerRoute,
) {
    let Some(entry) = crate::state::take_current_for_cancel(agent_id, task_id) else {
        return;
    };
    let state = state_for(agent_id);
    let client_id = entry.client_id;
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("cancelled"));
    detail.insert("send_id".to_string(), json!(entry.send_id));
    emit_status(kernel, &state, agent_id, &client_id, route, "done", detail).await;
}

/// `history` verb — the conversation at DIGEST resolution, plus the raw turns.
///
/// TWO SHAPES, ON PURPOSE. `rows` is python's: `{id, parent, kind, role, head,
/// bytes}` per row, heads capped at 120 characters, bodies behind `pull` and
/// text behind `recall` — that is what a MODEL should be handed, and what
/// python returns and nothing else.
///
/// `messages` is the full `{role, content}` list, and it stays because a
/// SPAWNED runtime has no other way out: the e2e harness builds its entire wire
/// transcript from this key (`e2e/helpers/host/wire.py`), where python's
/// in-process reader reads the store directly. Dropping it would leave every
/// spawned-runtime probe recording zero turns while the run burned its whole
/// idle window with nothing to show.
pub async fn history(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_CLIENT_ID),
    );
    let messages = load_history(agent_id, kernel, &client_id).await;
    let limit = payload.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
    let after = payload.get("after").and_then(Value::as_i64);
    let mut digest: Vec<Value> = messages
        .iter()
        .filter(|r| r.get("id").is_some())
        .map(|r| {
            let content = r.get("content").and_then(Value::as_str).unwrap_or("");
            json!({
                "id": r.get("id"),
                "parent": r.get("parent"),
                // `kind` FALLS BACK TO `role`: a plain turn carries no kind, and
                // a reader that switches on it must still see what it is.
                "kind": r.get("kind").or_else(|| r.get("role")),
                "role": r.get("role"),
                // 120 CHARACTERS, newlines flattened — a head is for
                // recognising a row, not for reading it.
                "head": content.chars().take(120).collect::<String>().replace('\n', " "),
                "bytes": content.len(),
            })
        })
        .collect();
    if let Some(a) = after {
        // AN UNKNOWN CURSOR PAGES NOTHING, rather than starting over: silently
        // re-serving the head would read as "no more rows" to a caller that had
        // already seen them.
        let at = digest
            .iter()
            .position(|d| d["id"].as_i64() == Some(a))
            .map(|i| i + 1)
            .unwrap_or(digest.len());
        digest.drain(..at);
    }
    let total = messages.len();
    // THE CAP INVARIANT, as python's `_make_history`: a digest page must answer
    // UNDER the inline cap or it is stored and dehydrated and the model pulls
    // its own history. `limit` bounds the count; the cap bounds the bytes.
    let meta = agent_meta(agent_id, kernel);
    let (kept, truncated) = crate::context::fit_reply(
        &digest,
        crate::context::inline_cap(&meta),
        (limit > 0).then_some(limit),
    );
    let next = truncated
        .then(|| kept.last().and_then(|d| d["id"].as_i64()))
        .flatten();
    json!({
        "rows": kept,
        "next": next,
        "total": total,
        "messages": messages,
        "client_id": client_id,
    })
}

/// Render ONE stored turn for a `recall` reply. Turns are pure text (tool
/// calls/replies are inline `<call name=…>` / its reply tag text), so a turn with
/// content renders as that content and anything else falls back to the row.
///
/// WHOLE, NOT CAPPED. Rust/swift truncated each turn at 2000 chars until
/// 2026-09-02 while python returned it verbatim — same verb, same store, two
/// different replies, and the cut half is exactly what a model pages back FOR.
fn recall_render(m: &Value) -> String {
    let content = m.get("content").and_then(Value::as_str).unwrap_or("");
    if content.is_empty() {
        serde_json::to_string(m).unwrap_or_default()
    } else {
        content.to_string()
    }
}

/// `recall` verb: page turns back from the DURABLE chat store (the FULL
/// conversation, never trimmed — so anything compaction dropped is one call
/// away). Read-only. args: `client_id?`, `query?` (case-insensitive
/// substring), `limit?` (default 20, max 100), `before?` (store index).
pub async fn recall(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    // No explicit client: the model calls recall MID-GENERATION, and that
    // generation knows which conversation it is (`current_meta.client_id`) —
    // reading it is a fact about the in-flight turn, not a default. Py-canonical,
    // measured 2026-08-07: the old 'cli' default handed a builder running under
    // client 'probe' an empty thread, and it concluded its history was gone.
    let inflight = crate::state::state_for(agent_id)
        .current_meta
        .lock()
        .expect("current poisoned")
        .as_ref()
        .map(|c| c.client_id.clone());
    let client_id = safe_client(
        payload
            .get("client_id")
            .and_then(Value::as_str)
            .or(inflight.as_deref())
            .unwrap_or(DEFAULT_CLIENT_ID),
    );
    let full = load_history(agent_id, kernel, &client_id).await;
    let q = payload
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let q = q.trim();
    let limit = payload
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(20)
        .clamp(1, 100) as usize;
    let before = payload
        .get("before")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let mut indexed: Vec<(usize, &Value)> = full.iter().enumerate().collect();
    if let Some(before) = before {
        indexed.retain(|(i, _)| *i < before);
    }
    if !q.is_empty() {
        indexed.retain(|(_, m)| {
            serde_json::to_string(m)
                .unwrap_or_default()
                .to_lowercase()
                .contains(q)
        });
    }
    let total = indexed.len();
    let truncated = total > limit;
    let mut page: Vec<&(usize, &Value)> = indexed[total.saturating_sub(limit)..].iter().collect();
    // A PAGE MUST FIT THE WINDOW IT IS GOING INTO. Measured 2026-09-05 (python):
    // the default 20 turns of 6.8KB reads came back as one 14,786-token reply
    // against a 12,000 window; `too_small` fired as a failfast and the chain
    // died. The page is trimmed from the OLDEST end (the newest of what was
    // asked for is kept) and says so, with the index to page further back
    // from — the caller loses a page, never the run. Measured on the WHOLE
    // reply serialized as one user turn, which is the unit delivery measures.
    let cap = crate::context::inline_cap(&agent_meta(agent_id, kernel));
    let render_page = |pg: &[&(usize, &Value)]| -> Vec<Value> {
        pg.iter()
            .map(|(i, m)| {
                json!({"index": i, "role": m.get("role").and_then(Value::as_str), "content": recall_render(m)})
            })
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
    let messages = render_page(&page);
    let mut reply = json!({
        "messages": messages, "total": total,
        "truncated": truncated || trimmed > 0, "client_id": client_id,
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
                "query {q:?} matched none of the {} stored turns for client {client_id:?}.",
                full.len()
            )
        } else if !fired {
            "nothing has been compacted away — recall pages back turns that \
             compaction DROPPED, and none have been. If you can see this reply, \
             your live context already holds the whole conversation."
                .to_string()
        } else {
            format!(
                "no stored turns for client {client_id:?} in this page; the durable \
                 thread is keyed per client_id — name one to read another thread."
            )
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
    let strategy = meta
        .get("context_strategy")
        .and_then(Value::as_str)
        .unwrap_or("compact");
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
pub const SEND_TIMEOUT_SECS: u64 = 180;

/// The effective per-`send` ceiling: `FANTASTIC_AI_SEND_TIMEOUT` if set, else
/// [`SEND_TIMEOUT_SECS`]. Parity with python's `SEND_TIMEOUT` and swift's
/// `sendTimeoutSeconds`, which read the same env.
pub fn send_timeout_secs() -> u64 {
    std::env::var("FANTASTIC_AI_SEND_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(SEND_TIMEOUT_SECS)
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
pub async fn send(
    provider: Arc<dyn Provider>,
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    cfg: BackendConfig,
) -> Value {
    let route = cfg.route;
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
    let send_to = payload
        .get("send_to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
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
            return json!({
                "error": format!("{}: cycle detected", cfg.name),
                "response": "",
                "cycle": cycle,
                "client_id": client_id,
            });
        }
        Err(CallStackRefusal::TooDeep) => {
            return json!({
                "error": format!("{}: max call depth {MAX_CALL_DEPTH} reached", cfg.name),
                "response": "",
                "client_id": client_id,
            });
        }
    }
    crate::route::trace_pub(&format!(
        "send verb ENTER agent={} client={} text_len={}",
        agent_id.as_str(),
        client_id,
        text.len()
    ));
    let send_id = mint_send_id();
    let state = state_for(agent_id);

    // Enqueue.
    let entry = QueuedEntry {
        client_id: client_id.clone(),
        text: text.clone(),
        send_id: send_id.clone(),
        queued_at: now_secs(),
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
        emit_status(
            kernel, &state, agent_id, &client_id, route, "queued", detail,
        )
        .await;
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
            started_at: now_secs(),
            phase: "thinking".to_string(),
            text_so_far: String::new(),
            last_tool: None,
            // The kernel task this generation IS — how `cancel` reaches it, and
            // how `on_cancel` recognises its own turn.
            task_id: current_task().map(|t| t.0.to_string()),
        });
    }
    emit_status(
        kernel,
        &state,
        agent_id,
        &client_id,
        route,
        "thinking",
        Map::new(),
    )
    .await;

    // Run the generation inline, bounded by the per-send ceiling. Cancellation
    // is the KERNEL's (`cancel <task_id>` → `on_cancel` → this future is
    // dropped, releasing the provider stream and the FIFO guard) — the backend
    // holds no task handle of its own.
    let timeout_secs = send_timeout_secs();
    let outcome = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
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

    let mut turn_id: Option<i64> = None;
    let reply = match outcome {
        Ok(Ok((final_text, tid))) => {
            turn_id = Some(tid);
            json!({
                "response": final_text,
                "final": final_text,
                "client_id": client_id,
            })
        }
        Ok(Err(e)) => {
            emit_status(kernel, &state, agent_id, &client_id, route, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("error"));
                m.insert("error".to_string(), json!(e.clone()));
                m
            })
            .await;
            json!({"error": e, "client_id": client_id})
        }
        Err(_elapsed) => {
            // WHAT IT HAD WRITTEN WHEN THE CLOCK RAN OUT. The generation future
            // is dropped here and its partial text goes with it, so a timeout
            // used to report only that it happened — not whether the model had
            // produced nothing, was mid-answer, or was looping. `text_so_far` is
            // already accumulated per chunk; carrying its SIZE and both ENDS
            // makes the three distinguishable without keeping the whole turn.
            let (partial_chars, head, tail) = {
                let cur = state.current_meta.lock().expect("current poisoned");
                match cur.as_ref() {
                    Some(c) => {
                        let t = &c.text_so_far;
                        let n = t.chars().count();
                        (
                            n,
                            t.chars().take(300).collect::<String>(),
                            t.chars().skip(n.saturating_sub(300)).collect::<String>(),
                        )
                    }
                    None => (0, String::new(), String::new()),
                }
            };
            emit_status(kernel, &state, agent_id, &client_id, route, "done", {
                let mut m = Map::new();
                m.insert("reason".to_string(), json!("timeout"));
                m.insert("partial_chars".to_string(), json!(partial_chars));
                m.insert("partial_head".to_string(), json!(head));
                m.insert("partial_tail".to_string(), json!(tail));
                m
            })
            .await;
            // The SIZE rides the error too. The status event carries the full
            // detail, but a subscriber can be absent (measured 2026-09-04: the
            // announcement never reached the harness), and the error string is
            // the one thing that always lands in the caller's reply.
            json!({
                "error": format!(
                    "send: timeout after {timeout_secs}s (had written \
                     {partial_chars} chars)"
                ),
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
                timeout_secs as f64,
                route,
                turn_id,
            )
            .await;
        });
    }
    reply
}

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
            let rendered: Vec<String> = opts
                .iter()
                .map(|o| {
                    o.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| o.to_string())
                })
                .collect();
            text.push_str("\n\nOptions: ");
            text.push_str(&rendered.join(", "));
        }
    }
    if let Some(act) = payload.get("action").filter(|v| v.is_object()) {
        text.push_str("\n\nProposed action: ");
        text.push_str(&act.to_string());
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
            let answer = c
                .arguments
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or("");
            // THE PLACEHOLDER IS NOT AN ANSWER. The refusal above SHOWS the
            // shape, and a model that could not answer once will copy that
            // example back verbatim on the retry — measured 2026-09-04 on
            // peer_review, where "..." was written into review.md as the verdict
            // and scored as a reviewer that had judged. Same trap as
            // `send_binary`'s literal "@body", and it earns the same refusal.
            if answer.trim().trim_matches('.').is_empty() {
                json!({
                    "error": format!(
                        "that is the PLACEHOLDER, not an answer — `answer` came back as {answer:?}. Say what you actually concluded."
                    ),
                    "reason": "bad_arg",
                    "arg": "answer",
                    "request_id": payload.get("request_id"),
                })
            } else {
                json!({
                    "answer": answer,
                    "request_id": payload.get("request_id"),
                })
            }
        }
    }
}
