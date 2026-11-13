//! The generation: ONE provider pass per `send` — assemble the prompt,
//! auto-compact to fit the window, stream tokens, persist history (RAW text out
//! to the caller, `<think>`-stripped text into the store). Per-backend knobs ride
//! on [`BackendConfig`].
//!
//! ONE PASS, and still no loop HERE — but that is not the same as "nothing
//! happens". This module generates; [`crate::route::route_reply`] then dispatches
//! whatever the completion named and delivers each reply to its own `reply_to`. A
//! chain therefore advances one pass at a time, and it advances only because the
//! model asked again: no call ends the turn, and stop is the default. Nothing
//! iterates on the model's behalf.

use crate::assembly::assemble_messages;
use crate::context::estimate_tokens;
use crate::events::{emit_status, to_caller, PartPublisher, PassParts};
use crate::memory::{save_history, MemoryStoreError};
use crate::provider::{Provider, ProviderError, ProviderEvent};
use crate::state::BackendState;
use crate::stream_parts::{cut_marker, PartSplitter};
use crate::think::without_thinking;
use fantastic_kernel::{current_task, AgentId, Kernel, TaskId};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Per-backend knobs threaded through the shared `send` so a single path
/// serves every backend. The provider is the streaming seam.
#[derive(Clone, Copy, Debug)]
pub struct BackendConfig {
    /// Backend error-message prefix (e.g. `"ollama_backend"`), used by the
    /// context-projection seam's `too_small` / config-error messages.
    pub name: &'static str,
    /// THE MODEL'S OWN WINDOW, in tokens — what `context_window` resolves to
    /// when the record names neither `context_window` nor `num_ctx`
    /// ([`crate::context::resolve_context_window`]). A server backend cannot
    /// know its model's window and keeps [`crate::constants::DEFAULT_CONTEXT_WINDOW`]
    /// (the record's `context_window` is the lever); a backend that can READ the
    /// real one says so here (foundation_models_backend: the SDK's
    /// `contextSize`). Its schema's `context_window` default is the same number
    /// ([`crate::schema::schema_with_window`]).
    pub model_window: i64,
    /// THE CAPABILITY: this backend can run the provider's own cache markers
    /// (`caching=provider_cache`). A backend that cannot refuses that mode at the
    /// write, and a turn on it refuses too. Only anthropic declares it.
    pub provider_cache: bool,
}

/// Why a generation produced no completion. The two kinds settle differently,
/// so they are not one string: python returns a projection refusal VERBATIM out
/// of `_run` (no status event, no `client_id`, no raise), while a provider
/// failure propagates.
pub enum GenFailure {
    /// The context-projection seam refused (`too_small` failsafe, unknown
    /// strategy). The dict is the reply — python `_run`'s
    /// `if isinstance(projected, dict): return projected`.
    Projection(Value),
    /// The provider pass failed. See [`ProviderError`].
    Provider(ProviderError),
    /// The agent's memory store refused or is unreachable — FAIL FAST, no turn
    /// (python `MemoryStoreError`, caught by `_send`).
    Store(MemoryStoreError),
    /// `system_prompt_file` is set and cannot be the block — FAIL LOUD, no turn
    /// on another block (python `PromptFileError`, caught by `_send`).
    PromptFile(crate::prompt_file::PromptFileError),
    /// A fold's summary would not fit its budget after every re-summarise — FAIL
    /// LOUD, no turn on an unfolded view (python `FoldSummaryError`, caught by
    /// `_send`).
    FoldSummary(crate::strategies::fold::FoldSummaryError),
    /// A fold's anchor cannot be known — python's `ValueError` out of
    /// `member_anchor`, which `_send`'s `except Exception` reports and re-raises.
    FoldAnchor(crate::folds::FoldAnchorError),
}

/// What one generation produced.
pub enum Generation {
    /// An ANSWER, stored as this turn's assistant row `message_id`. `text` is
    /// RAW — thinking markers intact — because routing parses it.
    Answered {
        /// The raw completion text.
        text: String,
        /// The assistant row this turn was stored as.
        message_id: i64,
        /// Every closed call part, `(stream_id, body)` — routing stamps each
        /// dispatched message with the stream it was previewed as (python's
        /// `call_streams`).
        call_streams: Vec<(String, String)>,
        /// The model stopped with a block open: NOTHING of this pass dispatches,
        /// its closed calls included.
        cut: bool,
    },
    /// A DEAD turn: nothing left once the thinking is stripped. DISCARDED —
    /// never stored, never an answer — and routed like any turn that dispatched
    /// nothing, so the no-call reminder rerolls it (python `_run`'s
    /// `dead_turn` reply).
    Dead {
        /// The raw completion text (thinking only, or nothing).
        text: String,
    },
}

impl From<MemoryStoreError> for GenFailure {
    fn from(e: MemoryStoreError) -> Self {
        Self::Store(e)
    }
}

impl From<crate::assembly::BlockFail> for GenFailure {
    fn from(e: crate::assembly::BlockFail) -> Self {
        match e {
            crate::assembly::BlockFail::Store(e) => Self::Store(e),
            crate::assembly::BlockFail::PromptFile(e) => Self::PromptFile(e),
            crate::assembly::BlockFail::FoldSummary(e) => Self::FoldSummary(e),
            crate::assembly::BlockFail::FoldAnchor(e) => Self::FoldAnchor(e),
        }
    }
}

/// Project the message list to what the model actually reads — its role and its
/// text; every other key is stripped.
pub(crate) fn render_for_model(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let role = m.get("role").cloned().unwrap_or(Value::Null);
            let content = m.get("content").cloned().unwrap_or_else(|| json!(""));
            let mut out = json!({"role": role, "content": content});
            // the image parts of a message whose model is to see them (#622)
            if let Some(images) = crate::images::parts_of(m) {
                out["images"] = Value::Array(images.clone());
            }
            // the agent's cache_mark travels with its message; nothing else does
            if let Some(v) = m.get(crate::caching::CACHE_MARK) {
                out[crate::caching::CACHE_MARK] = v.clone();
            }
            out
        })
        .collect()
}

/// The incoming user row; `kind: "notice"` when it is this layer's own text to
/// its own agent (python `_notice_kind`) — the row otherwise keeps its role as its
/// kind.
#[must_use]
pub fn incoming_row(text: &str, id: i64, parent: Option<i64>, notice: bool) -> Value {
    let mut row = json!({"role": "user", "content": text, "id": id, "parent": parent});
    if notice {
        row["kind"] = json!("notice");
    }
    row
}

/// The incoming row of the turn in flight, with the images the turn attached.
fn incoming_row_of(state: &BackendState, text: &str, id: i64, parent: Option<i64>) -> Value {
    crate::images::stamped(
        incoming_row(text, id, parent, notice_in_flight(state)),
        attached_in_flight(state).as_ref(),
    )
}

/// The images the turn in flight attached.
pub(crate) fn attached_in_flight(state: &BackendState) -> Option<crate::images::Attached> {
    state
        .current_meta
        .lock()
        .expect("current poisoned")
        .as_ref()
        .and_then(|c| c.attached.clone())
}

/// Who sent the turn in flight: the `source` of its incoming row.
fn incoming_source(state: &BackendState) -> String {
    state
        .current_meta
        .lock()
        .expect("current poisoned")
        .as_ref()
        .map(|c| c.origin.clone())
        .unwrap_or_default()
}

/// Whether the turn in flight answers this layer's own text.
fn notice_in_flight(state: &BackendState) -> bool {
    state
        .current_meta
        .lock()
        .expect("current poisoned")
        .as_ref()
        .is_some_and(|c| c.notice)
}

/// One generation: assemble (system + compacted history + user) → project context
/// ONCE (auto-compact / notice / too_small) → ONE provider pass streaming tokens →
/// persist. Runs INLINE in the `send` dispatch — which is itself a kernel task, so
/// `cancel` drops this future (and with it the provider stream).
///
/// Returns the RAW completion text, thinking markers intact: [`crate::verbs::send`]
/// hands it to the router, which reads only the ANSWER — a call written inside
/// thinking never dispatches. The DURABLE turn is the answer.
///
/// The conversation is ALWAYS this agent's own (its memory store): there is no
/// caller-supplied history — a one-off completion is a disposable agent with its
/// own store; context for another agent travels in the text.
#[allow(clippy::too_many_arguments)]
pub async fn run_generation(
    provider: &Arc<dyn Provider>,
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
    cfg: BackendConfig,
    system_prompt: Option<String>,
) -> Result<Generation, GenFailure> {
    // A PROVIDER THAT CANNOT RUN REFUSES THE TURN FIRST — before the owed
    // recall, the assembly and the projection, so the caller reads why it
    // cannot answer rather than a window failure on a model that could never
    // have answered. Python `_run` builds its provider (`_get_provider`, which
    // may raise) before anything else.
    provider.ready().map_err(GenFailure::Provider)?;
    // THE CACHING MODE IS CHECKED FIRST TOO: a record that asks for provider caching
    // on a backend that cannot cache is refused before any row is written, and so
    // before the owed recall below (which writes a view and a recall row).
    let (caching, primer) = crate::caching::settings(&crate::helpers::agent_meta(self_id, kernel), cfg.provider_cache)
        .map_err(|why| GenFailure::Provider(ProviderError::value_error(why)))?;
    if caching.marks() && !cfg.provider_cache {
        return Err(GenFailure::Provider(crate::caching::unsupported(cfg.name)));
    }
    // A RECALL OWED FROM THE LAST FOLD BATCH (or from a boot) runs first, so this
    // turn is assembled over the head the recall chose — python `_recall_due`.
    let due = std::mem::take(&mut *state.recall_due.lock().expect("recall_due poisoned"));
    if due {
        crate::retrieval::context_recall(
            self_id,
            false,
            kernel,
            Some(provider),
            cfg.model_window,
            system_prompt.as_deref(),
            Some(client_id),
        )
        .await?;
    }
    let (mut messages, store_len) = assemble_messages(
        self_id,
        state,
        user_text,
        kernel,
        client_id,
        system_prompt.as_deref(),
        caching,
    )
    .await?;
    // KEEP (#641): a reply that will fold after the answer carries the call that keeps
    // what the model needs of it
    if caching.folds() {
        if let Some(last) = messages.last_mut() {
            let marked = crate::history_view::mark_keep(
                last.get("content").and_then(Value::as_str).unwrap_or(""),
            );
            last["content"] = Value::String(marked);
        }
    }
    // Context-overflow projection: shape what the MODEL sees this turn to fit the
    // window via the agent's `context_strategy`. Done ONCE at turn entry. The
    // window is read off the record EVERY turn, so a model switched to a smaller
    // one (`update_agent`) is fitted — folded — on its very next turn.
    let mut model_messages = match crate::projection::project_context(
        provider,
        self_id,
        state,
        kernel,
        client_id,
        cfg.name,
        cfg.model_window,
        messages,
        store_len,
    )
    .await
    {
        Ok(m) => m,
        // too_small failsafe / unknown-strategy config error — the model is NOT
        // called and the seam's dict IS the reply, python-verbatim: no
        // `status(done)` closes a turn that never opened a provider pass, and no
        // `client_id` is stapled onto a refusal python returns bare.
        Err(crate::projection::ProjectFail::Refusal(e)) => return Err(GenFailure::Projection(e)),
        Err(crate::projection::ProjectFail::Store(e)) => return Err(GenFailure::Store(e)),
        Err(crate::projection::ProjectFail::FoldSummary(e)) => {
            return Err(GenFailure::FoldSummary(e))
        }
        Err(crate::projection::ProjectFail::FoldAnchor(e)) => {
            return Err(GenFailure::FoldAnchor(e))
        }
    };

    // WHAT THIS REQUEST CARRIES, piece by piece, for the assistant row it earns.
    let request_parts = crate::assembly::request_parts(
        &state.request_parts.lock().expect("request_parts poisoned"),
        &model_messages,
    );
    let prev_request = state
        .prev_request
        .lock()
        .expect("prev_request poisoned")
        .clone();
    // a restart keeps counting from the newest stored row
    let seed = if prev_request.is_none() {
        crate::row_attrs::seed_totals(self_id, kernel).await
    } else {
        None
    };

    // THE AGENT'S CACHE MARKS (`cache_mark`): its system message, the primer's end, the
    // last fold's, the tail. Only `provider_cache` places them; the backend forwards.
    if caching.marks() {
        crate::caching::mark(&mut model_messages, primer);
    }
    // ONE provider pass. Every chunk is a content token — the provider knows
    // nothing of the tool protocol; the envelope is text this crate parses after.
    let model_view = render_for_model(&model_messages);
    // A FRESH reading per pass: a provider that reports nothing this time must not
    // leave last pass's number standing.
    provider.clear_prompt_tokens();
    // STREAMING PARTS: the same chunks, split into parts as
    // they are written — animation for the addressee, never action
    // ([`crate::stream_parts`]). Kept on the in-flight entry, where `stop` and
    // `on_cancel` find them. `part_start` carries `re` — the message this turn
    // answers (the kernel task this dispatch runs as).
    let parts = Arc::new(tokio::sync::Mutex::new(PassParts {
        splitter: PartSplitter::new(self_id.as_str()),
        publisher: PartPublisher::new(
            Arc::clone(kernel),
            self_id.clone(),
            client_id.to_string(),
            current_task().map(TaskId::get),
        ),
    }));
    if let Some(c) = state
        .current_meta
        .lock()
        .expect("current poisoned")
        .as_mut()
    {
        c.parts = Some(Arc::clone(&parts));
    }
    // A provider failure ABORTS every open part, then propagates (python's
    // `except Exception: push(stop("error")); raise`).
    // GENERATION TIME — from the request to the stream's end (the `done` detail).
    let gen_started = std::time::Instant::now();
    let mut stream = match provider.chat(&model_view, None).await {
        Ok(stream) => stream,
        Err(e) => return Err(abort_parts(&parts, e).await),
    };
    let mut content_parts: Vec<String> = Vec::new();
    let mut first_text_chunk = true;
    let mut cut_stream = false;
    while let Some(ev) = stream.next().await {
        let ev = match ev {
            Ok(ev) => ev,
            Err(e) => return Err(abort_parts(&parts, e).await),
        };
        match ev {
            ProviderEvent::Token(t) => {
                // THE CALL CHECKER RUNS ON THE STREAM (#499): at the first foreign call
                // tag the generation is cut, the rest is never read, and the turn ends
                // as if the answer had ended there — the router then gives the usual
                // correction. Checked only when this delta can complete a tag or a
                // JSON call head; thinking is excluded as `without_thinking` does.
                let t = if t.contains('>') || t.contains('"') {
                    let mut text = content_parts.concat();
                    text.push_str(&t);
                    match stream_cut(&text) {
                        Some(keep) => {
                            let before = text.len() - t.len();
                            cut_stream = true;
                            if keep >= before {
                                t[..keep - before].to_string()
                            } else {
                                // a tag held back (an open backtick) is now complete
                                // behind earlier deltas: drop their tail too
                                content_parts = vec![text[..keep].to_string()];
                                String::new()
                            }
                        }
                        None => t,
                    }
                } else {
                    t
                };
                if first_text_chunk {
                    first_text_chunk = false;
                    emit_status(kernel, state, self_id, client_id, "streaming", Map::new()).await;
                }
                {
                    let mut cur = state.current_meta.lock().expect("current poisoned");
                    if let Some(c) = cur.as_mut() {
                        c.text_so_far.push_str(&t);
                    }
                }
                to_caller(
                    kernel,
                    self_id,
                    client_id,
                    json!({"type": "token", "text": t, "source": self_id.as_str()}),
                )
                .await;
                {
                    let mut p = parts.lock().await;
                    let evs = p.splitter.feed(&t);
                    p.publisher.push(evs, false).await;
                }
                content_parts.push(t);
                if cut_stream {
                    break;
                }
            }
        }
    }
    // The cut drops the provider stream: the request ends here.
    drop(stream);
    // A block still open when the model stopped generating is CUT: an open call
    // is never dispatched (`extract_tool_calls` skips it) and the turn's row says
    // so.
    // ONLY A SUCCESSFUL GENERATION DISPATCHES: a pass cut with
    // a block open sends nothing, its earlier closed calls included, and the marker
    // lists every call of the pass as not sent.
    let (cut_calls, call_streams) = {
        let mut p = parts.lock().await;
        // A stream the checker cut (#499) is not a stopped generation: its answer
        // ends at the foreign tag and the router corrects it like any finished one.
        let cut = p.splitter.open_call().is_some() && !cut_stream;
        let evs = p.splitter.finish();
        p.publisher.push(evs, true).await;
        let calls: Vec<_> = p
            .splitter
            .parts
            .iter()
            .filter(|x| x.kind == crate::stream_parts::PartKind::Call)
            .cloned()
            .collect();
        (cut.then_some(calls), p.splitter.closed_calls.clone())
    };
    let last_text = content_parts.join("");
    // THE PASS ENDED: the provider's prompt cache is warm from here (`fork_points`).
    *state.last_send_secs.lock().expect("last_send poisoned") = Some(crate::helpers::now_secs());
    // THE ENDPOINT'S OWN COUNT calibrates our chars/4 estimate (python `_calib`).
    if let Some(reported) = provider.report().prompt_tokens.filter(|n| *n > 0) {
        let est = estimate_tokens(&model_view).max(1);
        *state.calib.lock().expect("calib poisoned") = Some(reported as f64 / est as f64);
        *state.reported.lock().expect("reported poisoned") = Some(reported);
    }

    // A DEAD TURN — nothing left once the thinking is stripped — did not answer,
    // whatever the provider's finish reason says (a tiny thinking budget can end
    // the thought with a clean `finish_reason: stop` and an empty answer). The
    // ANSWER is discarded here — never stored, never counted as an answer — and
    // the turn is routed like any that dispatched nothing: the no-call reminder
    // rerolls it (python `_run`). Failing the turn outright would end a long
    // chain on its first dead turn.
    if without_thinking(&last_text).trim().is_empty() {
        // The INCOMING row is kept — only the empty answer is dropped. The turn
        // this one answered may be a reply batch carrying the results of the
        // model's own calls; unstored, the reroll would arrive with those results
        // gone and the model would re-ask for them.
        let in_id = kernel.mint_id();
        let in_parent = incoming_parent(user_text);
        save_history(
            self_id,
            kernel,
            &[crate::row_attrs::with_source(
                incoming_row_of(state, user_text, in_id, in_parent),
                &incoming_source(state),
            )],
        )
        .await?;
        emit_status(kernel, state, self_id, client_id, "done", {
            let mut m = Map::new();
            m.insert("reason".to_string(), json!("dead_turn"));
            m
        })
        .await;
        return Ok(Generation::Dead { text: last_text });
    }

    emit_status(kernel, state, self_id, client_id, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        let window = crate::context::resolve_context_window(
            &crate::helpers::agent_meta(self_id, kernel),
            cfg.model_window,
        );
        m.extend(usage_detail(
            &provider.report(),
            &model_view,
            &last_text,
            gen_started.elapsed().as_secs_f64(),
            window,
        ));
        m
    })
    .await;

    // ROW IDS for this turn: the incoming row (a root, or a delivered reply turn
    // that names its parent on its tag) and the assistant row that answers it.
    //
    // MINTED HERE, AT THE APPEND — not at turn start. The store returns an
    // owner's rows in ID order and the head cache keeps APPEND order; the two
    // agree only while ids are minted as rows are appended. Minted at turn start,
    // a fold or view row saved mid-turn (the projection) took a higher id yet
    // landed first, and after a restart the conversation came back in a
    // different order than the agent had lived it (python, E78).
    let in_id = kernel.mint_id();
    let in_parent = incoming_parent(user_text);
    let message_id = kernel.mint_id();
    // APPEND this send's user + assistant turns to the memory store so context
    // management (compaction/recall) has the FULL conversation next turn.
    let mut next_request = None;
    let new_turns = vec![
        crate::row_attrs::with_source(
            incoming_row_of(state, user_text, in_id, in_parent),
            &incoming_source(state),
        ),
        // STRIPPED — the deliberation is not part of the conversation. The RAW
        // text (thinking included) is what the caller receives; the router parses
        // only the answer, and durable history loses the `<think>` spans, because a
        // reasoning model re-fed its own prior deliberation drowns in it. A pass
        // the model stopped with a block open leaves the CUT MARKER instead, on top
        // of the context.
        {
            let mut row = json!({
                "role": "assistant",
                "content": match &cut_calls {
                    Some(calls) => cut_marker(&last_text, "model stopped generating", calls),
                    None => crate::call_check::repair(&without_thinking(&last_text)),
                },
                "id": message_id,
                "parent": in_id,
            });
            if let Some(m) = row.as_object_mut() {
                m.extend(crate::think::thinking_fields(&last_text));
                // stored in `attributes` and returned by the retrieval verbs, never
                // rendered into a request (`render_for_model` carries role and content
                // only)
                let mut attrs = Map::new();
                let usage = usage_row(&provider.report());
                // WHAT SERVED IT: the provider agent (the chat itself when it names
                // none) and the model it ran, with the pass priced from that
                // provider's record
                let provider_id = crate::helpers::agent_meta(self_id, kernel)
                    .get(crate::remote_provider::PROVIDER_FIELD)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map_or_else(|| self_id.as_str().to_string(), String::from);
                let model = provider.model();
                if let Some(c) = crate::row_attrs::cost(
                    usage.as_ref(),
                    &crate::helpers::agent_meta(&AgentId::from(provider_id.as_str()), kernel),
                    &model,
                ) {
                    attrs.insert("cost".to_string(), c);
                }
                if let Some(u) = usage {
                    attrs.insert("usage".to_string(), u);
                }
                if !model.is_empty() {
                    attrs.insert("model".to_string(), json!(model));
                }
                attrs.insert("provider".to_string(), json!(provider_id));
                let (parts, totals, next) =
                    crate::row_attrs::build(prev_request.as_ref(), seed, message_id, request_parts);
                attrs.insert("parts".to_string(), parts);
                attrs.insert("totals".to_string(), totals);
                m.insert("attributes".to_string(), Value::Object(attrs));
                next_request = Some(next);
            }
            row
        },
    ];
    save_history(self_id, kernel, &new_turns).await?;
    *state.prev_request.lock().expect("prev_request poisoned") = next_request;
    Ok(Generation::Answered {
        text: last_text,
        message_id,
        call_streams,
        cut: cut_calls.is_some(),
    })
}

/// Every open part ABORTS for its watchers (`"error"`), and the provider failure
/// is handed back to propagate.
/// The token figures a finished generation's `status` `done` carries (a chat
/// shows `42 tok/s · 3.1k / 64k ctx`): `prompt_tokens` and
/// `completion_tokens` as the provider REPORTED them — estimated (this layer's
/// own count) when it reported none, and then `prompt_tokens_estimated` /
/// `completion_tokens_estimated` says so — `gen_seconds` (the wall time from the
/// request to the stream's end, thinking included) and `context_window` (the
/// window in force). Python `usage_detail`.
#[must_use]
pub fn usage_detail(
    report: &crate::provider::ProviderReport,
    model_view: &[Value],
    text: &str,
    gen_seconds: f64,
    window: i64,
) -> Map<String, Value> {
    let mut m = Map::new();
    match report.prompt_tokens.filter(|n| *n > 0) {
        Some(n) => {
            m.insert("prompt_tokens".into(), json!(n));
        }
        None => {
            m.insert("prompt_tokens".into(), json!(estimate_tokens(model_view)));
            m.insert("prompt_tokens_estimated".into(), json!(true));
        }
    }
    match report.completion_tokens.filter(|n| *n > 0) {
        Some(n) => {
            m.insert("completion_tokens".into(), json!(n));
        }
        None => {
            // python `max(1, len(text) // 4) if text else 0` — chars, not bytes
            let chars = i64::try_from(text.chars().count()).unwrap_or(i64::MAX);
            let est = if chars == 0 { 0 } else { (chars / 4).max(1) };
            m.insert("completion_tokens".into(), json!(est));
            m.insert("completion_tokens_estimated".into(), json!(true));
        }
    }
    m.insert(
        "gen_seconds".into(),
        json!((gen_seconds * 1000.0).round() / 1000.0),
    );
    m.insert("context_window".into(), json!(window));
    m
}

/// The counts the endpoint reported for the pass, as the assistant row keeps them:
/// `input` (what the cache did not serve or write), `output`, and the cache counts
/// when the endpoint has them. A figure the endpoint did not report is absent, never
/// 0; none reported leaves no `usage` at all.
fn usage_row(report: &crate::provider::ProviderReport) -> Option<Value> {
    let mut u = Map::new();
    let cached = report.cache_read.unwrap_or(0) + report.cache_write.unwrap_or(0);
    if let Some(n) = report.prompt_tokens.filter(|n| *n > 0) {
        u.insert("input".into(), json!((n - cached).max(0)));
    }
    if let Some(n) = report.completion_tokens.filter(|n| *n > 0) {
        u.insert("output".into(), json!(n));
    }
    if let Some(n) = report.cache_read {
        u.insert("cache_read".into(), json!(n));
    }
    if let Some(n) = report.cache_write {
        u.insert("cache_write".into(), json!(n));
    }
    (!u.is_empty()).then_some(Value::Object(u))
}

async fn abort_parts(parts: &tokio::sync::Mutex<PassParts>, e: ProviderError) -> GenFailure {
    let mut p = parts.lock().await;
    let evs = p.splitter.stop("error");
    p.publisher.push(evs, true).await;
    GenFailure::Provider(e)
}

/// The turn a delivered reply answers: `↑N` on the FIRST tag of the incoming
/// text — python `_TAG_LINKS`. Anything else is a ROOT.
pub(crate) fn incoming_parent(text: &str) -> Option<i64> {
    let rest = text.strip_prefix("[reply to ")?;
    let head = &rest[..rest.find(']')?];
    let (_, after) = head.split_once('↑')?;
    let tok: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
    tok.parse::<i64>().ok()
}

#[cfg(test)]
mod usage_tests {
    use super::usage_detail;
    use crate::provider::ProviderReport;
    use serde_json::json;

    /// python `test_the_done_detail_carries_the_token_usage`: reported figures as
    /// reported; none reported → estimated and marked so.
    #[test]
    fn the_done_detail_carries_the_token_usage() {
        let view = vec![json!({"role": "user", "content": "go"})];
        let rep = ProviderReport {
            prompt_tokens: Some(1234),
            completion_tokens: Some(56),
            stop_reason: None,
            ..Default::default()
        };
        let d = usage_detail(&rep, &view, "answer", 1.23456, 32768);
        assert_eq!(
            serde_json::Value::Object(d),
            json!({"prompt_tokens": 1234, "completion_tokens": 56,
                   "gen_seconds": 1.235, "context_window": 32768})
        );
        let d = usage_detail(&ProviderReport::default(), &view, "abcdefgh", 0.5, 8192);
        assert_eq!(d["prompt_tokens_estimated"], json!(true));
        assert_eq!(d["completion_tokens"], json!(2));
        assert_eq!(d["completion_tokens_estimated"], json!(true));
        assert!(d["prompt_tokens"].as_i64().is_some_and(|n| n > 0));
    }
}

/// Where the answer received so far must be cut (#499): the byte length of the raw
/// text to keep, just past its first foreign call tag, or `None`. The check reads the
/// answer without its thinking ([`without_thinking`]); the cut is mapped back to the
/// raw text by its tail, which the last delta wrote.
fn stream_cut(raw: &str) -> Option<usize> {
    let stripped = without_thinking(raw);
    let end = crate::call_check::foreign_cut(&stripped)?;
    let tail = &stripped[end..];
    let body = raw.trim_end();
    Some(if tail.is_empty() {
        body.len()
    } else if body.ends_with(tail) {
        body.len() - tail.len()
    } else {
        raw.len()
    })
}
