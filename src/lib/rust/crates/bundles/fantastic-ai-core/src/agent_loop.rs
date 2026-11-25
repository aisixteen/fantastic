//! The generation: ONE provider pass per `send` — assemble the prompt,
//! auto-compact to fit the window, stream tokens, persist history (RAW text out
//! to the caller, `<think>`-stripped text into the store). Per-backend knobs ride
//! on [`BackendConfig`].
//!
//! ONE PASS, and still no loop HERE — but that is not the same as "nothing
//! happens". This module generates; [`crate::route::route_reply`] then dispatches
//! whatever the completion named and delivers each reply to its own `reply_to`. A
//! chain therefore advances one pass at a time, each pass woken by a delivered
//! message. There is no loop in the kernel or in a pass: when a pass writes no
//! call, the AI bundle may wake itself with an ordinary message to its own inbox
//! (the nudge, `route.rs`), its own policy built from the bus.
//!
//! INSIDE THE TURN, ATTEMPTS (#766, 1.5): an answer whose calls are malformed or
//! foreign for the pass's model type is re-asked, up to [`CALL_RETRIES`] times, with
//! the failed answer and its report; nothing of a failed attempt is stored or routed.
//! After the last one failed the harness stops the turn with the `stop` call
//! ([`Generation::Stopped`]).

use crate::assembly::assemble_messages;
use crate::constants::CALL_RETRIES;
use crate::context::estimate_tokens;
use crate::events::{emit_status, to_caller, PartPublisher, PassParts};
use crate::memory::{save_history, MemoryStoreError};
use crate::model_format::pass::{Mode, Pass};
use crate::model_format::Out;
use crate::provider::{Provider, ProviderError, ProviderEvent};
use crate::state::BackendState;
use crate::stream_parts::{cut_marker, PartSplitter};
use crate::think::without_thinking;
use crate::used::{Accepted, Counts, Landed, Window};
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
    /// THE HARNESS STOPPED THE TURN (#766, 1.8): `1 + CALL_RETRIES` attempts in a row
    /// wrote malformed calls, so the model is not asked again. Stored: the incoming row
    /// and the assistant row `message_id`, whose content is the `stop` call with
    /// `reason`. Nothing is routed; the chain ends through the give-up.
    Stopped {
        /// The harness's reason (`res/<type>/stop_reason.txt`, the attempt count).
        reason: String,
        /// The assistant row this turn was stored as.
        message_id: i64,
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

/// Project the message list to what the model actually reads — its role, and its
/// text under the head of its visible attributes ([`crate::row_attrs::VISIBLE`]);
/// every other key is stripped.
pub(crate) fn render_for_model(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let role = m.get("role").cloned().unwrap_or(Value::Null);
            let content = match m.get("content") {
                Some(Value::String(_)) | None => json!(crate::row_attrs::model_text(m)),
                Some(other) => other.clone(),
            };
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
    let (caching, primer) = crate::caching::settings(&crate::helpers::agent_meta(self_id, kernel))
        .map_err(|why| GenFailure::Provider(ProviderError::value_error(why)))?;
    // THE PASS PATH (#766): every model request of this run goes through it, the turn's
    // and its helpers' (the owed recall, the projection's fold summaries). Its model
    // type is fixed for the run.
    let pass = Pass::new(Arc::clone(provider), self_id, state, kernel).await;
    // A TYPE OTHER THAN THE LAST TURN'S (#766, 1.10): traced as `model_switched`
    crate::model_format::trace::note_switch(self_id, kernel, state, &pass).await;
    // THE CALIBRATION BELONGS TO THE TYPE IT WAS MEASURED UNDER (#766, Q37): a pass of
    // another type than the last finished one is projected at 1.0.
    clear_calib_on_type_change(state, pass.kind);
    // A RECALL OWED FROM THE LAST FOLD BATCH (or from a boot) runs first, so this
    // turn is assembled over the head the recall chose — python `_recall_due`.
    let due = std::mem::take(&mut *state.recall_due.lock().expect("recall_due poisoned"));
    if due {
        crate::retrieval::context_recall(
            self_id,
            false,
            kernel,
            Some(&pass),
            cfg.model_window,
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
        caching,
    )
    .await?;
    // THE INCOMING MESSAGE IS WRITTEN NOW: the model reads this `ts` at its head, and
    // its row keeps the same one, so the next request renders it byte for byte the same
    // (and its UTC offset, #766 1.13, set once here beside the `ts`)
    let incoming_ts = json!(crate::helpers::now_secs());
    let incoming_offset = crate::row_attrs::offset_at(&incoming_ts);
    let stamp_incoming = |m: &mut Value| {
        m["attributes"]["ts"] = incoming_ts.clone();
        if let Some(off) = &incoming_offset {
            m["attributes"][crate::row_attrs::UTC_OFFSET] = off.clone();
        }
    };
    if let Some(last) = messages.last_mut() {
        stamp_incoming(last);
    }
    // the incoming row as stored, whether the pass answers or dies: the `ts` above
    let incoming_row = |id: i64, parent: Option<i64>| {
        let mut row = incoming_row_of(state, user_text, id, parent);
        stamp_incoming(&mut row);
        crate::row_attrs::with_source(row, &incoming_source(state))
    };
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
        &pass,
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
    // a restart keeps counting from the newest stored row: the request totals, and the
    // token counts (#766, 1.7)
    let seed = if prev_request.is_none() {
        crate::row_attrs::seed_totals(self_id, kernel).await
    } else {
        None
    };
    crate::used::seed(self_id, state, kernel).await;

    // THE AGENT'S CACHE MARKS (`cache_mark`): its system message, the primer's end, the
    // last fold's, the tail. Only `provider_cache` places them; the backend forwards.
    if caching.marks() {
        crate::caching::mark(&mut model_messages, primer);
    }
    // `model_view` is the INTERNAL view: the estimate and the calibration read it. Each
    // sequence of attempts sends it rendered for its model type (`attempts`).
    let model_view = render_for_model(&model_messages);
    let meta = crate::helpers::agent_meta(self_id, kernel);
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
    // GENERATION TIME — from the first request to the accepted stream's end (the `done`
    // detail).
    let gen_started = std::time::Instant::now();
    let caller = Caller {
        kernel,
        state,
        self_id,
        client_id,
        parts: &parts,
    };
    let view_of = ViewOf {
        model_messages: &model_messages,
        model_view: &model_view,
        request_parts: &request_parts,
        meta: &meta,
    };
    // THE ATTEMPTS (#766, 1.5 step 4): one sequence under the pass's type; a failed
    // attempt whose reply named a model of another type restarts it once, under that
    // type, with a view rendered for it (Q26).
    let mut restarted: Option<Pass> = None;
    let seq = match attempts(&pass, true, &view_of, &caller).await? {
        Seq::Restart(next) => {
            let next = restarted.insert(*next);
            let mut fields = Map::new();
            fields.insert("from".into(), json!(pass.kind));
            fields.insert("to".into(), json!(next.kind));
            fields.insert(
                "model".into(),
                json!(next.model.clone().unwrap_or_default()),
            );
            next.events()
                .push(Mode::Turn.name(), next.kind, "model_switched", fields);
            // the system block's `send` how-to is the type's (Q16): the restarted
            // request carries the new type's, so no request mixes two types' forms
            let messages = with_howto(&model_messages, pass.kind, next.kind);
            let view = render_for_model(&messages);
            let view_of = ViewOf {
                model_messages: &messages,
                model_view: &view,
                ..view_of
            };
            attempts(next, false, &view_of, &caller).await?
        }
        other => other,
    };
    let cur: &Pass = restarted.as_ref().unwrap_or(&pass);
    // THE PASS ENDED: the provider's prompt cache is warm from here (`fork_points`).
    *state.last_send_secs.lock().expect("last_send poisoned") = Some(crate::helpers::now_secs());
    // THE ENDPOINT'S OWN COUNT calibrates our chars/4 estimate (python `_calib`), from
    // the window: attempt 1 of the sequence the row comes from, the request the view was
    // built for (#766, 1.7). After a type restart the old type's ratio goes first.
    let window = match &seq {
        Seq::Accepted { window, .. } | Seq::Stopped { window, .. } => *window,
        Seq::Restart(_) => None,
    };
    if cur.kind != pass.kind {
        *state.calib.lock().expect("calib poisoned") = None;
    }
    *state.last_type.lock().expect("last_type poisoned") = Some(cur.kind);
    calibrate(state, window, &model_view);
    let (last_text, report, accepted, open) = match seq {
        Seq::Accepted {
            text,
            report,
            accepted,
            open,
            ..
        } => (text, report, accepted, open),
        // THE HARNESS STOP (1.8): every attempt was malformed. The model is not asked
        // again; the turn's call is the `stop`, its reason worded by the harness. Its
        // row's `used` has nothing accepted: every attempt is in `turn.rejected`.
        Seq::Stopped { errors, .. } => {
            let reason = crate::model_format::fill(
                crate::model_format::of(cur.kind).stop_reason,
                &[("n", &errors.len().to_string())],
            );
            let mut args = Map::new();
            args.insert("reason".to_string(), json!(reason));
            let content = crate::tool_parse::render_call("stop", &args);
            let mut fields = Map::new();
            fields.insert("reason".into(), json!(reason));
            fields.insert("errors".into(), json!(errors));
            cur.events()
                .push(Mode::Turn.name(), cur.kind, "stop", fields);
            let in_id = kernel.mint_id();
            let in_parent = incoming_parent(user_text);
            let message_id = kernel.mint_id();
            let mut row = json!({
                "role": "assistant",
                "content": content,
                "id": message_id,
                "parent": in_id,
            });
            let landing = crate::used::land(state, None);
            let next = assistant_attributes(
                &mut row,
                self_id,
                kernel,
                provider,
                &landing.landed,
                window,
                prev_request.as_ref(),
                seed,
                message_id,
                request_parts,
            );
            save_history(self_id, kernel, &[incoming_row(in_id, in_parent), row]).await?;
            landing.stored();
            *state.prev_request.lock().expect("prev_request poisoned") = Some(next);
            // THE TURN'S TRACE ROWS, once its rows are stored (#766, 1.10)
            let events = crate::model_format::trace::of_turn(&pass, restarted.as_ref());
            crate::model_format::trace::write(
                self_id,
                kernel,
                state,
                client_id,
                Some(message_id),
                events,
            )
            .await;
            return Ok(Generation::Stopped { reason, message_id });
        }
        Seq::Restart(_) => unreachable!("a restarted sequence does not restart again"),
    };
    // A block still open when the model stopped generating is CUT: an open call
    // is never dispatched (`extract_tool_calls` skips it) and the turn's row says
    // so.
    // ONLY A SUCCESSFUL GENERATION DISPATCHES: a pass cut with
    // a block open sends nothing, its earlier closed calls included, and the marker
    // lists every call of the pass as not sent.
    let (cut_calls, call_streams) = {
        let mut p = parts.lock().await;
        let cut = open || p.splitter.open_call().is_some();
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
        save_history(self_id, kernel, &[incoming_row(in_id, in_parent)]).await?;
        // it stored no assistant row: its attempt's counts wait in `turn.rejected`
        drop(accepted);
        emit_status(kernel, state, self_id, client_id, "stalled", {
            let mut m = Map::new();
            m.insert("reason".to_string(), json!("dead_turn"));
            m
        })
        .await;
        // its trace rows name no row: it stored none
        let events = crate::model_format::trace::of_turn(&pass, restarted.as_ref());
        crate::model_format::trace::write(self_id, kernel, state, client_id, None, events).await;
        return Ok(Generation::Dead { text: last_text });
    }

    emit_status(kernel, state, self_id, client_id, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        let context_window = crate::context::resolve_context_window(&meta, cfg.model_window);
        m.extend(usage_detail(
            window.map(Window::sum),
            Counts::of(&report).map(|c| c.completion),
            &model_view,
            &last_text,
            gen_started.elapsed().as_secs_f64(),
            context_window,
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
    //
    // STRIPPED — the deliberation is not part of the conversation. The RAW
    // text (thinking included) is what the caller receives; the router parses
    // only the answer, and durable history loses the `<think>` spans, because a
    // reasoning model re-fed its own prior deliberation drowns in it. A pass
    // the model stopped with a block open leaves the CUT MARKER instead, on top
    // of the context.
    let mut row = json!({
        "role": "assistant",
        "content": match &cut_calls {
            Some(calls) => cut_marker(&last_text, "model stopped generating", calls),
            None => crate::call_check::repair(&without_thinking(&last_text)),
        },
        "id": message_id,
        "parent": in_id,
    });
    // A `stop` THE MODEL WROTE (1.8) is traced with its reason and no `errors`
    if cut_calls.is_none() {
        crate::model_format::trace::note_model_stop(
            cur,
            row["content"].as_str().unwrap_or_default(),
        );
    }
    if let Some(m) = row.as_object_mut() {
        m.extend(crate::think::thinking_fields(&last_text));
    }
    let landing = crate::used::land(state, Some(accepted));
    let next = assistant_attributes(
        &mut row,
        self_id,
        kernel,
        provider,
        &landing.landed,
        window,
        prev_request.as_ref(),
        seed,
        message_id,
        request_parts,
    );
    save_history(self_id, kernel, &[incoming_row(in_id, in_parent), row]).await?;
    landing.stored();
    *state.prev_request.lock().expect("prev_request poisoned") = Some(next);
    // THE TURN'S TRACE ROWS (#766, 1.10), its helpers' included, written once its rows
    // are stored, in one append under the chat's key
    let events = crate::model_format::trace::of_turn(&pass, restarted.as_ref());
    crate::model_format::trace::write(self_id, kernel, state, client_id, Some(message_id), events)
        .await;
    Ok(Generation::Answered {
        text: last_text,
        message_id,
        call_streams,
        cut: cut_calls.is_some(),
    })
}

/// The assistant row's `attributes`, stored and returned by the retrieval verbs, never
/// rendered into a request (`render_for_model` renders only the declared visible ones,
/// [`crate::row_attrs::VISIBLE`]): what served it (the provider agent, the chat itself
/// when it names none, and the model it ran), its token counts (`used`, #766 1.7), the
/// `window` of the request its view was built for when the endpoint reported one, the
/// price of `used.pass.total`, and the request's `parts` and `totals`. What the chat
/// remembers of the request is handed back.
#[allow(clippy::too_many_arguments)]
fn assistant_attributes(
    row: &mut Value,
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    provider: &Arc<dyn Provider>,
    used: &Landed,
    window: Option<Window>,
    prev_request: Option<&crate::row_attrs::PrevRequest>,
    seed: Option<Value>,
    message_id: i64,
    request_parts: Vec<Value>,
) -> crate::row_attrs::PrevRequest {
    let mut attrs = Map::new();
    let provider_id = crate::helpers::agent_meta(self_id, kernel)
        .get(crate::remote_provider::PROVIDER_FIELD)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map_or_else(|| self_id.as_str().to_string(), String::from);
    let model = provider.model();
    if let Some(c) = crate::row_attrs::cost(
        Some(&used.pass.total),
        &crate::helpers::agent_meta(&AgentId::from(provider_id.as_str()), kernel),
        &model,
    ) {
        attrs.insert("cost".to_string(), c);
    }
    attrs.insert("used".to_string(), used.to_value());
    if let Some(w) = window {
        attrs.insert("window".to_string(), w.to_value());
    }
    if !model.is_empty() {
        attrs.insert("model".to_string(), json!(model));
    }
    attrs.insert("provider".to_string(), json!(provider_id));
    let (parts, totals, next) =
        crate::row_attrs::build(prev_request, seed, message_id, request_parts);
    attrs.insert("parts".to_string(), parts);
    attrs.insert("totals".to_string(), totals);
    if let Some(m) = row.as_object_mut() {
        m.insert("attributes".to_string(), Value::Object(attrs));
    }
    next
}

/// Where an attempt's output goes as it streams: the caller's `token` events, the
/// in-flight entry's `text_so_far` and the pass's parts.
struct Caller<'a> {
    kernel: &'a Arc<Kernel>,
    state: &'a Arc<BackendState>,
    self_id: &'a AgentId,
    client_id: &'a str,
    parts: &'a Arc<tokio::sync::Mutex<PassParts>>,
}

impl Caller<'_> {
    /// One piece of an attempt's internal text, sent on today's path: the first one of
    /// an attempt opens its `streaming` status.
    async fn send(&self, t: &str, first: &mut bool) {
        if *first {
            *first = false;
            emit_status(
                self.kernel,
                self.state,
                self.self_id,
                self.client_id,
                "streaming",
                Map::new(),
            )
            .await;
        }
        if t.is_empty() {
            return;
        }
        {
            let mut cur = self.state.current_meta.lock().expect("current poisoned");
            if let Some(c) = cur.as_mut() {
                c.text_so_far.push_str(t);
            }
        }
        to_caller(
            self.kernel,
            self.self_id,
            self.client_id,
            json!({"type": "token", "text": t, "source": self.self_id.as_str()}),
        )
        .await;
        let mut p = self.parts.lock().await;
        let evs = p.splitter.feed(t);
        p.publisher.push(evs, false).await;
    }

    /// A FAILED ATTEMPT IS MARKED, THEN ITS PARTS GO (Q40): the `retrying` status
    /// `{attempt, of, error, kind: "call"}` first, when another attempt follows; then
    /// every open part aborts with the reason `malformed`, the attempt's text leaves the
    /// in-flight entry and a new splitter starts. Nothing streamed is taken back.
    async fn discard(&self, retry: Option<(usize, &str)>) {
        if let Some((attempt, error)) = retry {
            let mut detail = Map::new();
            detail.insert("attempt".to_string(), json!(attempt));
            detail.insert("of".to_string(), json!(CALL_RETRIES));
            detail.insert("error".to_string(), json!(error));
            detail.insert("kind".to_string(), json!("call"));
            emit_status(
                self.kernel,
                self.state,
                self.self_id,
                self.client_id,
                "retrying",
                detail,
            )
            .await;
        }
        {
            let mut p = self.parts.lock().await;
            // the deltas still batched reach their watchers first: an abort drops them
            p.publisher.push(Vec::new(), true).await;
            let evs = p.splitter.stop("malformed");
            p.publisher.push(evs, true).await;
            p.splitter = PartSplitter::new(self.self_id.as_str());
        }
        let mut cur = self.state.current_meta.lock().expect("current poisoned");
        if let Some(c) = cur.as_mut() {
            c.text_so_far.clear();
        }
    }
}

/// `messages` with the `send` how-to of type `from` in each system message swapped, by
/// exact match, for type `to`'s (the assembly put the pass type's how-to there, Q16).
fn with_howto(messages: &[Value], from: &str, to: &str) -> Vec<Value> {
    let (was, now) = (
        crate::model_format::of(from).howto_send,
        crate::model_format::of(to).howto_send,
    );
    messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if m["role"] == json!("system") {
                if let Some(text) = m.get("content").and_then(Value::as_str) {
                    let swapped = text.replace(was, now);
                    m["content"] = Value::String(swapped);
                }
            }
            m
        })
        .collect()
}

/// What a sequence of attempts renders its view from.
struct ViewOf<'a> {
    model_messages: &'a [Value],
    model_view: &'a [Value],
    request_parts: &'a [Value],
    meta: &'a Map<String, Value>,
}

/// How a sequence of attempts ended.
enum Seq {
    /// An attempt with no `Bad`: its internal text (reasoning intact), its report, its
    /// counts held for the row, whether the stream ended inside a call (`Open`), and the
    /// sequence's window (attempt 1's).
    Accepted {
        text: String,
        report: crate::provider::ProviderReport,
        accepted: Accepted,
        open: bool,
        window: Option<Window>,
    },
    /// Every attempt was malformed: each one's report, first line, in order (their
    /// counts are in `used_pending`), and the sequence's window (attempt 1's).
    Stopped {
        errors: Vec<String>,
        window: Option<Window>,
    },
    /// A failed attempt's reply named a model of another type: the pass for it.
    Restart(Box<Pass>),
}

/// A report's first line: the `error` of a `retrying` status and of the trace.
fn first_line(report: &str) -> &str {
    report.lines().next().unwrap_or_default()
}

/// ONE SEQUENCE OF ATTEMPTS under `pass`'s type (1.5 step 4): its view rendered through
/// the chat's cache, then up to `1 + CALL_RETRIES` requests. Each token goes through the
/// type's reader; text and calls go on today's path as internal text, the model's raw
/// text is kept for the re-ask. At a `Bad` nothing more of the attempt is forwarded and
/// its stream is read to its end, unseen (Q32), so its report exists; the next attempt
/// carries every failed attempt's raw text (no thinking) and report as a re-ask pair.
/// `may_restart`: a failed attempt whose reply names a model of another type ends the
/// sequence for a restart under that type (once per turn). A transport error aborts the
/// parts and propagates: the turn is rerun whole (`verbs.rs`), from attempt 1.
async fn attempts(
    pass: &Pass,
    may_restart: bool,
    view_of: &ViewOf<'_>,
    caller: &Caller<'_>,
) -> Result<Seq, GenFailure> {
    // THE TYPED VIEW (#766): each message's content rendered for the type through the
    // chat's render cache; role, images and cache marks carried unchanged. It holds the
    // pass's pins until the sequence is accepted (finished) or ends otherwise (dropped).
    let typed_view = pass.view(
        view_of.model_messages,
        view_of.model_view,
        view_of.request_parts,
        view_of.meta,
    );
    let mut reask: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut window: Option<Window> = None;
    for attempt in 1..=1 + CALL_RETRIES {
        let mut request = typed_view.messages.clone();
        request.extend(reask.iter().cloned());
        // A FRESH reading per request: it clears the provider's counts first, so a
        // provider that reports nothing this time leaves no number of an earlier one.
        let mut stream = match pass.request(&request, None).await {
            Ok(stream) => stream,
            Err(e) => return Err(abort_parts(caller.parts, e).await),
        };
        let mut reader = pass.reader.outbound();
        let mut raw = String::new();
        let mut text = String::new();
        let mut bad: Option<String> = None;
        let mut open = false;
        let mut first = true;
        let mut repaired: Vec<String> = Vec::new();
        while let Some(ev) = stream.next().await {
            let ProviderEvent::Token(t) = match ev {
                Ok(ev) => ev,
                Err(e) => return Err(abort_parts(caller.parts, e).await),
            };
            if bad.is_some() {
                // READ TO ITS END, UNSEEN (Q32): the endpoint's report then exists.
                continue;
            }
            raw.push_str(&t);
            let outs = reader.feed(&t);
            take(
                outs,
                &mut text,
                &mut bad,
                &mut open,
                &mut first,
                &mut repaired,
                caller,
            )
            .await;
        }
        drop(stream);
        if bad.is_none() {
            let outs = reader.finish();
            take(
                outs,
                &mut text,
                &mut bad,
                &mut open,
                &mut first,
                &mut repaired,
                caller,
            )
            .await;
        }
        for added in repaired {
            let mut fields = Map::new();
            fields.insert("attempt".into(), json!(attempt));
            fields.insert("added".into(), json!(added));
            pass.events()
                .push(Mode::Turn.name(), pass.kind, "repaired", fields);
        }
        let report = pass.report(Mode::Turn);
        if attempt == 1 {
            window = Window::of(&report);
        }
        let Some(why) = bad else {
            // THE ACCEPTED ATTEMPT: calls, prose, reasoning or nothing. The pass
            // finished: its view's chain is kept, its pins go.
            typed_view.finish(&mut pass.events(), Mode::Turn.name());
            let accepted = pass.accepted(&report);
            return Ok(Seq::Accepted {
                text,
                report,
                accepted,
                open,
                window,
            });
        };
        // a thrown-away attempt: its counts go to the next row's `turn.rejected`
        pass.rejected(&report);
        let error = first_line(&why).to_string();
        let mut fields = Map::new();
        fields.insert("attempt".into(), json!(attempt));
        fields.insert("error".into(), json!(error));
        fields.extend(retry_counts(&report));
        pass.events()
            .push(Mode::Turn.name(), pass.kind, "retry", fields);
        errors.push(error);
        // A REPLY NAMING A MODEL OF ANOTHER TYPE (Q26): no re-ask pair from it, its raw
        // text is in this type's form; the sequence restarts once under the new type.
        if may_restart {
            if let Some(next) = pass.retyped(&pass.provider.model()) {
                caller
                    .discard(Some((attempt, errors.last().map_or("", String::as_str))))
                    .await;
                drop(typed_view);
                return Ok(Seq::Restart(Box::new(next)));
            }
        }
        if attempt <= CALL_RETRIES {
            caller
                .discard(Some((attempt, errors.last().map_or("", String::as_str))))
                .await;
            reask.push(json!({"role": "assistant", "content": without_thinking(&raw)}));
            reask.push(json!({"role": "user", "content": why}));
        } else {
            caller.discard(None).await;
        }
    }
    drop(typed_view);
    Ok(Seq::Stopped { errors, window })
}

/// The calibration (`calib`, `reported`) from a sequence's window: the prompt the
/// endpoint counted over our estimate of the internal view. No window (no report) leaves
/// both as they were.
fn calibrate(state: &BackendState, window: Option<Window>, model_view: &[Value]) {
    let Some(reported) = window.map(Window::sum).filter(|n| *n > 0) else {
        return;
    };
    let est = estimate_tokens(model_view).max(1);
    *state.calib.lock().expect("calib poisoned") = Some(reported as f64 / est as f64);
    *state.reported.lock().expect("reported poisoned") = Some(reported);
}

/// `calib` cleared when `kind` is not the type of the chat's last finished turn pass
/// (Q37): the projection then runs at 1.0. A model switch inside one type keeps it.
fn clear_calib_on_type_change(state: &BackendState, kind: &str) {
    let last = *state.last_type.lock().expect("last_type poisoned");
    if last.is_some_and(|k| k != kind) {
        *state.calib.lock().expect("calib poisoned") = None;
    }
}

/// An attempt's reader output, taken in order: text and calls sent on, the first `Bad`
/// ends what is forwarded, an `Open` noted, a repair's closers kept for its trace event.
async fn take(
    outs: Vec<Out>,
    text: &mut String,
    bad: &mut Option<String>,
    open: &mut bool,
    first: &mut bool,
    repaired: &mut Vec<String>,
    caller: &Caller<'_>,
) {
    for out in outs {
        if bad.is_some() {
            return;
        }
        match out {
            Out::Text(t) | Out::Call { text: t } => {
                caller.send(&t, first).await;
                text.push_str(&t);
            }
            Out::Bad { report } => *bad = Some(report),
            Out::Open { .. } => *open = true,
            Out::Repaired { added } => repaired.push(added),
        }
    }
}

/// A failed attempt's counts as the endpoint reported them, for its `retry` trace row:
/// `prompt` (what the cache did not serve or write), `completion`, `cache_read`,
/// `cache_write`. None when it reported nothing.
fn retry_counts(report: &crate::provider::ProviderReport) -> Map<String, Value> {
    let mut m = Map::new();
    let Some(c) = Counts::of(report) else {
        return m;
    };
    m.insert("prompt".into(), json!(c.prompt));
    m.insert("completion".into(), json!(c.completion));
    m.insert("cache_read".into(), json!(c.cache_read));
    m.insert("cache_write".into(), json!(c.cache_write));
    m
}

/// The token figures a finished generation's `status` `done` carries (a chat
/// shows `42 tok/s · 3.1k / 64k ctx`): `prompt_tokens` and
/// `completion_tokens` as the endpoint REPORTED them (#766, 1.7: `prompt` is the
/// window's sum, the prompt of attempt 1 of the sequence the answer came from;
/// `completion` is the accepted attempt's) — estimated (this layer's own count)
/// when it reported none, and then `prompt_tokens_estimated` /
/// `completion_tokens_estimated` says so — `gen_seconds` (the wall time from the
/// request to the stream's end, thinking included) and `context_window` (the
/// window in force). Python `usage_detail`.
#[must_use]
pub fn usage_detail(
    prompt: Option<i64>,
    completion: Option<i64>,
    model_view: &[Value],
    text: &str,
    gen_seconds: f64,
    window: i64,
) -> Map<String, Value> {
    let mut m = Map::new();
    match prompt.filter(|n| *n > 0) {
        Some(n) => {
            m.insert("prompt_tokens".into(), json!(n));
        }
        None => {
            m.insert("prompt_tokens".into(), json!(estimate_tokens(model_view)));
            m.insert("prompt_tokens_estimated".into(), json!(true));
        }
    }
    match completion.filter(|n| *n > 0) {
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

/// Every open part ABORTS for its watchers (`"error"`), and the provider failure
/// is handed back to propagate.
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
    use serde_json::json;

    /// python `test_the_done_detail_carries_the_token_usage`: reported figures as
    /// reported; none reported → estimated and marked so.
    #[test]
    fn the_done_detail_carries_the_token_usage() {
        let view = vec![json!({"role": "user", "content": "go"})];
        let d = usage_detail(Some(1234), Some(56), &view, "answer", 1.23456, 32768);
        assert_eq!(
            serde_json::Value::Object(d),
            json!({"prompt_tokens": 1234, "completion_tokens": 56,
                   "gen_seconds": 1.235, "context_window": 32768})
        );
        let d = usage_detail(None, None, &view, "abcdefgh", 0.5, 8192);
        assert_eq!(d["prompt_tokens_estimated"], json!(true));
        assert_eq!(d["completion_tokens"], json!(2));
        assert_eq!(d["completion_tokens_estimated"], json!(true));
        assert!(d["prompt_tokens"].as_i64().is_some_and(|n| n > 0));
    }
}
