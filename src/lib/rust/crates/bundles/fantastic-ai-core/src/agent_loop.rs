//! THE PASS, step by step (#795): an AI agent's pass is a record handed from one
//! message to the next, never a task that waits.
//!
//! 1. [`start`] (woken by [`crate::life::wake`]): the pass takes every unread message
//!    (its TAIL, already stored) and reads what it needs — its history and its own
//!    `reflect` — each as a STEP ([`crate::steps`]: one call, whose settle is the next
//!    step); then it assembles its view, folds it to fit the window, and sends its
//!    request to the model ([`crate::provider::begin`]). The step ENDS there.
//! 2. The request's SETTLE is the next step ([`settled`]): the WHOLE answer is read once
//!    with the type's reader — the stream fed only the live view. An answer whose calls
//!    are malformed or foreign, or that has no call, is re-asked with a second request,
//!    up to [`CALL_RETRIES`] times, with the failed answer and its report; nothing of a
//!    failed attempt is stored or routed. After the last one the harness stops the
//!    pass with the `stop` call. An attempt whose thinking the harness cut at the
//!    chat's cap (`stop_reason` [`THINKING_CUT`], #782) is re-asked ONCE with a short
//!    "answer now" note; a second cut stops it the same way.
//! 3. An ACCEPTED answer is stored as the pass's assistant row (a step), its trace rows
//!    written (steps), the pass ENDS ([`crate::life::end`]), and its calls go out
//!    ([`crate::route::route_reply`]), each through [`fantastic_kernel::Kernel::call`];
//!    each reply is a later message.
//!
//! A failed request (the link, the provider, its deadline) ends the pass with its error
//! row and a `failed` status; a link failure is first retried, visibly, up to the
//! record's `transport_retries`. A cancel of the request ends the pass as stopped.

use crate::constants::CALL_RETRIES;
use crate::context::estimate_tokens;
use crate::events::{emit_status, to_caller, PartPublisher, PassParts};
use crate::memory::MemoryStoreError;
use crate::model_format::cache::View;
use crate::model_format::pass::{Mode, Pass};
use crate::model_format::{Out, Outbound};
use crate::provider::{Provider, ProviderError};
use crate::state::{state_for, Arrival, BackendState, CurrentEntry};
use crate::stream_parts::{cut_marker, PartSplitter};
use crate::think::{without_thinking, THINKING_CUT};
use crate::used::{Counts, Landed, Window};
use async_trait::async_trait;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Per-backend knobs threaded through every pass so a single path serves every
/// backend. The provider is the seam.
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

/// Why a pass produced no answer: each is said in its `failed` status, and the pass
/// ends ([`failed`]).
#[derive(Clone)]
pub enum GenFailure {
    /// The context-projection seam refused (`too_small` failsafe, unknown
    /// strategy): its refusal.
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
            // the helper's steps answer every task request before they end
            crate::assembly::BlockFail::Pending => Self::Store(MemoryStoreError(
                "a helper's task request was left unanswered".to_string(),
            )),
        }
    }
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


/// What one pass is: whose, which, what it reads, and how it asks.
pub(crate) struct Run {
    kernel: Arc<Kernel>,
    self_id: AgentId,
    state: Arc<BackendState>,
    cfg: BackendConfig,
    provider: Arc<dyn Provider>,
    /// The pass's number ([`crate::life::is_current`]).
    epoch: u64,
    /// The messages it reads, in arrival order.
    tail: Vec<Arrival>,
    /// The client id its events carry: its first message's.
    client_id: String,
    /// The row its answer answers: the newest it reads.
    parent: i64,
    /// Link failures retried so far (`transport_retries`).
    retried: u32,
    /// When the pass's ceiling (`send_timeout`) ends: its attempts share it; a link
    /// retry starts it again.
    deadline: std::time::Instant,
    /// The conversation the pass reads, as its history step read it ([`gather`]).
    history: crate::memory::MemResult<Vec<Value>>,
    /// The agent's own `reflect`, as its reflect step read it: the system block's self.
    me: Value,
    /// A test that drives one pass reads how it ended here, and its answer is not routed.
    #[cfg_attr(not(test), allow(dead_code))]
    observe: Observer,
}

#[cfg(test)]
type Observer = Option<tokio::sync::oneshot::Sender<Result<Generation, GenFailure>>>;
#[cfg(not(test))]
type Observer = ();

/// TESTS: how a driven pass ended ([`run_generation`]).
#[cfg(test)]
pub(crate) enum Generation {
    /// Its answer row: the raw text, the row, whether it was cut.
    Answered {
        text: String,
        message_id: i64,
        cut: bool,
    },
    /// The harness stopped it: the reason and the `stop` row.
    Stopped { reason: String, message_id: i64 },
}

/// TESTS: ONE PASS of `self_id` over `user_text`, driven to its end: the message stored,
/// the pass run, how it ended. Its answer is not routed.
#[cfg(test)]
pub(crate) async fn run_generation(
    provider: &Arc<dyn Provider>,
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
    cfg: BackendConfig,
) -> Result<Generation, GenFailure> {
    let m = crate::life::Incoming {
        text: user_text.to_string(),
        client_id: client_id.to_string(),
        origin: client_id.to_string(),
        notice: false,
        attached: None,
    };
    let row_id = crate::life::arrived(kernel, self_id, m, false).await?;
    let epoch = {
        let mut life = state.life.lock().expect("life poisoned");
        life.running = true;
        life.epoch += 1;
        life.epoch
    };
    let p = Arc::clone(provider);
    *state.machine.lock().expect("machine poisoned") = Some(crate::state::Machine {
        provider: Arc::new(move |_| Arc::clone(&p)),
        cfg,
    });
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tail = vec![Arrival {
        row_id,
        client_id: client_id.to_string(),
        origin: client_id.to_string(),
        text: user_text.to_string(),
        send_id: crate::helpers::mint_send_id(),
        at: crate::helpers::now_secs(),
        notice: false,
    }];
    begin_pass(Arc::clone(kernel), self_id.clone(), epoch, tail, Some(tx)).await;
    rx.await.unwrap_or_else(|_| {
        Err(GenFailure::Provider(ProviderError::cancelled("the pass ended unobserved")))
    })
}

/// A driven pass ended: its test is told (tests only).
#[cfg(test)]
fn observed(run: &mut Run, how: Result<Generation, GenFailure>) -> bool {
    match run.observe.take() {
        Some(tx) => {
            let _ = tx.send(how);
            true
        }
        None => false,
    }
}

/// THE WAKE NOTICE: the messages a pass reads arrived while the previous pass was
/// working, so its answer row lies after them; the notice is the request's last
/// message. No imperative: the model decides what to answer.
fn arrived_notice(n: usize) -> String {
    format!("[{n} message(s) arrived while you were working — they are above.]")
}

/// A PASS'S FIRST STEP: take the tail, assemble, send the request; the step ends there.
pub(crate) async fn start(kernel: Arc<Kernel>, self_id: AgentId, epoch: u64, tail: Vec<Arrival>) {
    #[cfg(test)]
    begin_pass(kernel, self_id, epoch, tail, None).await;
    #[cfg(not(test))]
    begin_pass(kernel, self_id, epoch, tail, ()).await;
}

async fn begin_pass(
    kernel: Arc<Kernel>,
    self_id: AgentId,
    epoch: u64,
    tail: Vec<Arrival>,
    observe: Observer,
) {
    let state = state_for(&self_id);
    let machine = state.machine.lock().expect("machine poisoned").clone();
    let Some(machine) = machine else {
        eprintln!("  [ai] {self_id}: a pass cannot start: no call to the agent has run yet");
        crate::life::end(&kernel, &self_id, epoch);
        return;
    };
    let client_id = tail
        .first()
        .map_or_else(fantastic_kernel::inquiry::default_client_id, |a| a.client_id.clone());
    let b = Begin {
        kernel: Arc::clone(&kernel),
        self_id: self_id.clone(),
        state,
        machine,
        epoch,
        tail,
        client_id,
        observe,
    };
    // THE REQUEST ENDS WITH A USER MESSAGE: the history is read first (a step), and a
    // tail that arrived while the last pass worked lies before its answer row
    crate::steps::history(&self_id, &kernel, b, |b, rows| b.read(rows)).await;
}

/// A pass starting: what its first steps hand on until its run is built.
struct Begin {
    kernel: Arc<Kernel>,
    self_id: AgentId,
    state: Arc<BackendState>,
    machine: crate::state::Machine,
    epoch: u64,
    tail: Vec<Arrival>,
    client_id: String,
    observe: Observer,
}

impl Begin {
    /// The history read: a tail after the last answer row gets the wake notice after
    /// that row (a step), then the run is built.
    async fn read(self, rows: crate::memory::MemResult<Vec<Value>>) {
        let last_role = rows.as_ref().ok().and_then(|rows| {
            rows.last()
                .and_then(|r| r.get("role"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if last_role.as_deref() != Some("assistant") || self.tail.is_empty() {
            return self.run().await;
        }
        let text = arrived_notice(self.tail.len());
        let m = crate::life::Incoming {
            text: text.clone(),
            client_id: self.client_id.clone(),
            origin: self.self_id.as_str().to_string(),
            notice: true,
            attached: None,
        };
        let (k, me) = (Arc::clone(&self.kernel), self.self_id.clone());
        crate::life::arrive(&k, &me, m, false, self, move |mut b, r| async move {
            if let Ok(row_id) = r {
                b.tail.push(Arrival {
                    row_id,
                    client_id: b.client_id.clone(),
                    origin: b.self_id.as_str().to_string(),
                    text,
                    send_id: crate::helpers::mint_send_id(),
                    at: crate::helpers::now_secs(),
                    notice: true,
                });
            }
            b.run().await;
        })
        .await;
    }

    /// The pass's entry is set, `thinking` said, and its run gathered and asked.
    async fn run(self) {
        let Begin { kernel, self_id, state, machine, epoch, tail, client_id, observe } = self;
        let (Some(first), Some(last)) = (tail.first().cloned(), tail.last().cloned()) else {
            crate::life::end(&kernel, &self_id, epoch);
            return;
        };
        *state.current_meta.lock().expect("current poisoned") = Some(CurrentEntry {
            client_id: client_id.clone(),
            text: last.text.clone(),
            send_id: first.send_id.clone(),
            queued_at: first.at,
            origin: last.origin.clone(),
            started_at: crate::helpers::now_secs(),
            phase: "thinking".to_string(),
            text_so_far: String::new(),
            last_tool: None,
            task_id: None,
            parts: None,
            stopped_by: None,
            notice: tail.iter().all(|a| a.notice),
            abort: Arc::new(tokio::sync::Notify::new()),
            answers: last.row_id,
            request: 0,
        });
        let provider = (machine.provider)(&client_id);
        let run = Run {
            kernel,
            self_id,
            state,
            cfg: machine.cfg,
            provider,
            epoch,
            tail,
            client_id,
            parent: last.row_id,
            retried: 0,
            deadline: ceiling_from_now(),
            history: Ok(Vec::new()),
            me: Value::Null,
            observe,
        };
        emit_status(&run.kernel, &run.state, &run.self_id, &run.client_id, "thinking", Map::new()).await;
        gather(run).await;
    }
}

/// WHAT THE PASS READS, as STEPS (#795): its conversation (the history step) and its own
/// `reflect` (the system block's self), each one call whose settle is the next step;
/// then the pass is assembled and asked. A link retry gathers again.
fn gather(run: Run) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
        crate::steps::history(&me, &k, run, |mut run, rows| async move {
            // a `stop` meanwhile ended the pass: nothing of it goes on
            if !crate::life::is_current(&run.self_id, run.epoch) {
                return;
            }
            run.history = rows;
            let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
            crate::steps::own_reflect(&me, &k, run, |mut run, reflected| async move {
                if !crate::life::is_current(&run.self_id, run.epoch) {
                    return;
                }
                run.me = reflected;
                prepare_and_ask(run).await;
            })
            .await;
        })
        .await;
    })
}

/// The pass's ceiling (`send_timeout`), from now.
fn ceiling_from_now() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs_f64(crate::constants::send_timeout_secs())
}

/// Assemble the pass and send its first request, or fail it: the provider's readiness
/// and the caching mode are checked first, then the pass's type is resolved (a step:
/// [`Pass::begin`]) and the previous pass's type read (a step:
/// [`crate::model_format::trace::previous_type`]); [`Prep`] goes on from there.
fn prepare_and_ask(run: Run) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        // A PROVIDER THAT CANNOT RUN REFUSES THE PASS FIRST, before the owed recall, the
        // assembly and the projection.
        if let Err(e) = run.provider.ready() {
            return failed(run, GenFailure::Provider(e)).await;
        }
        // THE CACHING MODE IS CHECKED FIRST TOO: a record that asks for provider caching
        // on a backend that cannot cache is refused before any row is written.
        let caching = match crate::caching::settings(&crate::helpers::agent_meta(&run.self_id, &run.kernel)) {
            Ok(c) => c,
            Err(why) => return failed(run, GenFailure::Provider(ProviderError::value_error(why))).await,
        };
        // THE PASS PATH (#766): every model request of this pass goes through it, its own
        // and its helpers' (the owed recall, the projection's fold summaries).
        let rows = run.history.clone().unwrap_or_default();
        let (k, me, st, p) = (
            Arc::clone(&run.kernel),
            run.self_id.clone(),
            Arc::clone(&run.state),
            Arc::clone(&run.provider),
        );
        Pass::begin(p, &me, &st, &k, rows, (run, caching), |(run, caching), pass| async move {
            if !crate::life::is_current(&run.self_id, run.epoch) {
                return;
            }
            let (k, me, st) = (Arc::clone(&run.kernel), run.self_id.clone(), Arc::clone(&run.state));
            let prep = Prep { run, caching, pass };
            crate::model_format::trace::previous_type(&me, &k, &st, prep, |prep, from| prep.typed(from)).await;
        })
        .await;
    })
}

/// A pass being assembled: what its steps hand on until its first request.
struct Prep {
    run: Run,
    caching: (crate::caching::Caching, Option<i64>),
    pass: Pass,
}

impl Prep {
    /// The previous pass's type is known: a type other than it is traced as
    /// `model_switched` (#766, 1.10), and the calibration measured under it dropped
    /// (#766, Q37); then the prompt file is brought current (steps,
    /// [`crate::prompt_file::refresh`]).
    async fn typed(self, from: Option<String>) {
        if !crate::life::is_current(&self.run.self_id, self.run.epoch) {
            return;
        }
        crate::model_format::trace::note_switch(&self.pass, from);
        clear_calib_on_type_change(&self.run.state, self.pass.kind);
        let (k, me) = (Arc::clone(&self.run.kernel), self.run.self_id.clone());
        crate::prompt_file::refresh(&me, &k, self, Self::filed).await;
    }

    /// The prompt file is current: A RECALL OWED FROM THE LAST FOLD BATCH (or from a
    /// boot) runs first, so this pass is assembled over the head the recall chose; the
    /// history is then read again (a step).
    async fn filed(self) {
        if !crate::life::is_current(&self.run.self_id, self.run.epoch) {
            return;
        }
        let run = &self.run;
        let due = std::mem::take(&mut *run.state.recall_due.lock().expect("recall_due poisoned"));
        if !due {
            let store = run.history.clone();
            return self.assemble(store).await;
        }
        let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
        let (decl, window, client) = (self.pass.decl, run.cfg.model_window, run.client_id.clone());
        let unread: Vec<i64> = run.tail.iter().map(|a| a.row_id).collect();
        let reflected = run.me.clone();
        let Self { run, caching, pass } = self;
        crate::retrieval::context_recall(
            &me,
            false,
            &k,
            Some(pass),
            decl,
            window,
            Some(client),
            unread,
            reflected,
            (run, caching),
            |(run, caching), pass, recalled| async move {
                let Some(pass) = pass else { return };
                if !crate::life::is_current(&run.self_id, run.epoch) {
                    return;
                }
                if let Err(e) = recalled {
                    return failed(run, e.into()).await;
                }
                // a recall rewrote the view and the head: the history is read again
                let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
                crate::steps::history(&me, &k, Self { run, caching, pass }, Self::assemble).await;
            },
        )
        .await;
    }

    /// THE ASSEMBLY over `store`, the history the pass read: the view over it (the tail
    /// is its newest rows), the projection that fits it to the window, and the cache
    /// marks; then the first request.
    async fn assemble(self, store: crate::memory::MemResult<Vec<Value>>) {
        if !crate::life::is_current(&self.run.self_id, self.run.epoch) {
            return;
        }
        let Self { run, caching, pass } = self;
        let (messages, store_len, newest) = match assembled(&run, caching.0, store) {
            Ok(a) => a,
            Err(f) => return failed(run, f).await,
        };
        // Context-overflow projection: shape what the MODEL sees this pass to fit the
        // window (steps: its summaries are task requests, its view row a store write).
        let (k, me, st, client) = (
            Arc::clone(&run.kernel),
            run.self_id.clone(),
            Arc::clone(&run.state),
            run.client_id.clone(),
        );
        let (name, window) = (run.cfg.name, run.cfg.model_window);
        crate::projection::project_context(
            pass,
            &me,
            &st,
            &k,
            &client,
            name,
            window,
            messages,
            store_len,
            (run, caching, newest),
            |(run, caching, newest), pass, projected| async move {
                if !crate::life::is_current(&run.self_id, run.epoch) {
                    return;
                }
                match projected.map_err(projection_failure).map(|m| prepared(&run, pass, caching, m, newest)) {
                    Ok(prep) => ask(Box::new(PassRec::new(run, prep))).await,
                    Err(f) => failed(run, f).await,
                }
            },
        )
        .await;
    }
}

/// What a pass holds from its assembly to its answer.
pub(crate) struct PassRec {
    run: Run,
    /// The pass path of the pass's type; after a type restart, `restarted` is the one
    /// asked.
    pass: Pass,
    restarted: Option<Pass>,
    may_restart: bool,
    model_messages: Vec<Value>,
    meta: Map<String, Value>,
    /// The typed view of the sequence asked now: it holds the pass's pins.
    view: Option<View<'static>>,
    sent: Vec<Value>,
    reask: Vec<Value>,
    errors: Vec<String>,
    window: Option<Window>,
    cut_before: bool,
    reason: Option<String>,
    attempt: usize,
    parts: Arc<tokio::sync::Mutex<PassParts>>,
    gen_started: std::time::Instant,
    prev_request: Option<crate::row_attrs::PrevRequest>,
    seed: Option<Value>,
}

/// What the assembly gives a pass.
struct Prepared {
    pass: Pass,
    model_messages: Vec<Value>,
    meta: Map<String, Value>,
    parts: Arc<tokio::sync::Mutex<PassParts>>,
    prev_request: Option<crate::row_attrs::PrevRequest>,
    seed: Option<Value>,
}

impl PassRec {
    fn new(run: Run, p: Prepared) -> Self {
        let view = p.pass.view(&p.model_messages, &p.meta);
        let sent = view.messages.clone();
        Self {
            run,
            pass: p.pass,
            restarted: None,
            may_restart: true,
            model_messages: p.model_messages,
            meta: p.meta,
            view: Some(view),
            sent,
            reask: Vec::new(),
            errors: Vec::new(),
            window: None,
            cut_before: false,
            reason: None,
            attempt: 1,
            parts: p.parts,
            gen_started: std::time::Instant::now(),
            prev_request: p.prev_request,
            seed: p.seed,
        }
    }

    /// The pass path asked now.
    fn cur(&self) -> &Pass {
        self.restarted.as_ref().unwrap_or(&self.pass)
    }
}

/// The pass's messages assembled over `store`, the history it read: the messages, the
/// store's length behind them, and the counts of the newest row it read.
fn assembled(
    run: &Run,
    caching: crate::caching::Caching,
    store: crate::memory::MemResult<Vec<Value>>,
) -> Result<(Vec<Value>, usize, Option<Value>), GenFailure> {
    let (kernel, self_id, state) = (&run.kernel, &run.self_id, &run.state);
    let store = store?;
    // a restart keeps counting from the newest row the pass read (#766, 1.7)
    let newest_totals = crate::row_attrs::seed_totals(&store);
    crate::used::seed(state, &store);
    let (mut messages, store_len, read) =
        crate::assembly::assemble_read(self_id, state, kernel, caching, store, &run.me)?;
    // WHAT THIS PASS READS IS READ (#795): a message stored after the tail was taken and
    // before the history read is in this request; it wakes no pass after
    crate::life::read_by_pass(self_id, &read);
    // KEEP (#641): a reply that will fold after the answer carries the call that keeps
    // what the model needs of it
    if caching.folds() {
        if let Some(last) = messages.pop() {
            messages.push(crate::history_view::mark_keep(last));
        }
    }
    Ok((messages, store_len, newest_totals))
}

/// A projection that produced no view, as the pass's failure.
fn projection_failure(f: crate::projection::ProjectFail) -> GenFailure {
    use crate::projection::ProjectFail;
    match f {
        ProjectFail::Refusal(e) => GenFailure::Projection(e),
        ProjectFail::Store(e) => GenFailure::Store(e),
        ProjectFail::FoldSummary(e) => GenFailure::FoldSummary(e),
        ProjectFail::FoldAnchor(e) => GenFailure::FoldAnchor(e),
        // the projection's steps answer every summary before they end
        ProjectFail::Pending => GenFailure::Store(MemoryStoreError(
            "a helper's task request was left unanswered".to_string(),
        )),
    }
}

/// The pass assembled: its projected view, its counts seeded from the newest row it
/// read, its cache marks and its live parts.
fn prepared(
    run: &Run,
    pass: Pass,
    (caching, primer): (crate::caching::Caching, Option<i64>),
    mut model_messages: Vec<Value>,
    newest_totals: Option<Value>,
) -> Prepared {
    let (kernel, self_id, state) = (&run.kernel, &run.self_id, &run.state);
    // read after the projection: a compaction makes the next request a checkpoint
    let prev_request = state
        .prev_request
        .lock()
        .expect("prev_request poisoned")
        .clone();
    let seed = if prev_request.is_none() { newest_totals } else { None };
    // THE AGENT'S CACHE MARKS (`cache_mark`): only `provider_cache` places them.
    if caching.marks() {
        crate::caching::mark(&mut model_messages, primer);
    }
    let meta = crate::helpers::agent_meta(self_id, kernel);
    // STREAMING PARTS: the live view's chunks, split into parts as they are written —
    // animation for the addressee, never action ([`crate::stream_parts`]). `part_start`
    // carries `re`: the row this pass answers.
    let parts = Arc::new(tokio::sync::Mutex::new(PassParts {
        splitter: PartSplitter::new(self_id.as_str()),
        publisher: PartPublisher::new(
            Arc::clone(kernel),
            self_id.clone(),
            run.client_id.clone(),
            Some(run.parent),
        ),
    }));
    if let Some(c) = state.current_meta.lock().expect("current poisoned").as_mut() {
        c.parts = Some(Arc::clone(&parts));
    }
    Prepared {
        pass,
        model_messages,
        meta,
        parts,
        prev_request,
        seed,
    }
}

/// SEND THE REQUEST of the attempt `rec.attempt`: its step ends here. The settle comes
/// to [`settled`] as a message.
fn ask(rec: Box<PassRec>) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        let mut request = rec.view.as_ref().map(|v| v.messages.clone()).unwrap_or_default();
        request.extend(rec.reask.iter().cloned());
        let (kernel, self_id, state, epoch) = (
            Arc::clone(&rec.run.kernel),
            rec.run.self_id.clone(),
            Arc::clone(&rec.run.state),
            rec.run.epoch,
        );
        let (serial, abort) = {
            let mut cur = state.current_meta.lock().expect("current poisoned");
            match cur.as_mut() {
                Some(c) => {
                    c.request += 1;
                    c.task_id = None;
                    (c.request, Arc::clone(&c.abort))
                }
                None => return,
            }
        };
        let provider = Arc::clone(&rec.cur().provider);
        // A FRESH reading per request: the provider's counts cleared first
        provider.clear_prompt_tokens();
        provider.clear_stop_reason();
        let live = Arc::new(LiveView::new(&rec));
        let feed = Arc::clone(&live);
        let within = rec
            .run
            .deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(std::time::Duration::from_millis(1));
        let deadline = (within, crate::constants::send_timeout_secs());
        let done: crate::provider::Done =
            Box::new(move |r| Box::pin(async move { settled(rec, feed, r).await }));
        // ITS EXECUTION, once it exists (after the request's images, steps): kept for
        // `stop` and `cancel`; a pass that ended meanwhile cancels it at once
        let k = Arc::clone(&kernel);
        let me = self_id.clone();
        let launched: crate::provider::Launched = Box::new(move |pid| {
            if !crate::life::is_current(&me, epoch) {
                fantastic_kernel::spawn_inherit(async move {
                    k.cancel_task(&pid).await;
                });
                return;
            }
            if let Some(c) = state
                .current_meta
                .lock()
                .expect("current poisoned")
                .as_mut()
                .filter(|c| c.request == serial)
            {
                c.task_id = Some(pid.0.to_string());
            }
        });
        crate::provider::begin(
            provider,
            &kernel,
            &self_id,
            request,
            None,
            deadline,
            abort,
            live as Arc<dyn crate::provider::Live>,
            done,
            launched,
        )
        .await;
    })
}

/// THE REQUEST SETTLED: the answer is read whole, then re-asked, accepted or stopped.
async fn settled(mut rec: Box<PassRec>, live: Arc<LiveView>, r: Result<String, ProviderError>) {
    if !crate::life::is_current(&rec.run.self_id, rec.run.epoch) {
        // a `stop` or a cancel ended this pass already: nothing of it is acted on
        return;
    }
    let raw = match r {
        Ok(raw) => raw,
        // A CANCEL OF THE REQUEST ends the pass as stopped, never re-asked
        Err(e) if e.is_cancelled() => {
            // its pins go first: nothing of the pass outlives its end
            drop(rec.view.take());
            let PassRec { run, .. } = *rec;
            let entry = crate::state::take_current(&run.self_id);
            let Some(entry) = entry else {
                crate::life::end(&run.kernel, &run.self_id, run.epoch);
                return;
            };
            // the stopped pass's rows are a step; the pass ends once they are written
            let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
            crate::verbs::close_stopped(&me, entry, &k, run, |run, r| async move {
                if let Err(e) = r {
                    eprintln!("  [ai] {}: the stopped pass's rows: {e}", run.self_id);
                }
                crate::life::end(&run.kernel, &run.self_id, run.epoch);
            })
            .await;
            return;
        }
        Err(e) => {
            let f = abort_parts(&rec.parts, e).await;
            drop(rec.view.take());
            let PassRec { run, .. } = *rec;
            return failed(run, f).await;
        }
    };
    live.finish().await;
    // THE ANSWER IS READ WHOLE, from the settle: the stream fed only the live view
    let mut reader = rec.cur().decl.reader.outbound();
    let mut read = Read::default();
    read.take(reader.feed(&raw));
    if read.bad.is_none() {
        read.take(reader.finish());
    }
    let attempt = rec.attempt;
    for (added, removed) in std::mem::take(&mut read.repaired) {
        let mut fields = Map::new();
        fields.insert("attempt".into(), json!(attempt));
        if !added.is_empty() {
            fields.insert("added".into(), json!(added));
        }
        if !removed.is_empty() {
            fields.insert("removed".into(), json!(removed));
        }
        let kind = rec.cur().kind;
        rec.cur().events().push(Mode::Turn.name(), kind, "repaired", fields);
    }
    let report = rec.cur().report(Mode::Turn);
    if attempt == 1 {
        rec.window = Window::of(&report);
    }
    // THE THINKING CUT (#782): the model is re-asked ONCE for its answer, a second cut
    // stops the pass
    let thought_cut = read.bad.is_none() && report.stop_reason.as_deref() == Some(THINKING_CUT);
    if thought_cut {
        read.bad = Some(thinking_note(crate::model_format::NOTICE_THINKING_CUT, &rec.meta));
    }
    // AN ANSWER WITH NO CALL (#795, Q3): re-asked like a malformed one. A cut answer (a
    // call still open at its end) is the cut path, never re-asked.
    if read.bad.is_none() && !read.open && !crate::route::acts(&read.text) {
        read.bad = Some((rec.cur().decl.texts)(crate::model_format::Section::NoCall).to_string());
    }
    let Some(why) = read.bad.take() else {
        // THE ACCEPTED ATTEMPT: its view's chain is kept, its pins go
        if let Some(view) = rec.view.take() {
            let mut events = rec.cur().events();
            view.finish(&mut events, Mode::Turn.name());
        }
        let accepted = rec.cur().accepted(&report);
        return answered(rec, read.text, report, accepted, read.open).await;
    };
    // a thrown-away attempt: its counts go to the next row's `turn.rejected`
    rec.cur().rejected(&report);
    let error = first_line(&why).to_string();
    let mut fields = Map::new();
    fields.insert("attempt".into(), json!(attempt));
    fields.insert("error".into(), json!(error));
    fields.extend(retry_counts(&report));
    let kind = rec.cur().kind;
    rec.cur().events().push(Mode::Turn.name(), kind, "retry", fields);
    rec.errors.push(error.clone());
    // A REPLY NAMING A MODEL OF ANOTHER TYPE (Q26): the sequence restarts once under
    // the new type, with the same view mapped for it
    if rec.may_restart {
        let model = rec.cur().provider.model();
        if let Some(next) = rec.cur().retyped(&model) {
            discard(&rec.run, &rec.parts, Some((attempt, &error))).await;
            drop(rec.view.take());
            let mut fields = Map::new();
            fields.insert("from".into(), json!(rec.pass.kind));
            fields.insert("to".into(), json!(next.kind));
            fields.insert("model".into(), json!(next.model.clone().unwrap_or_default()));
            next.events().push(Mode::Turn.name(), next.kind, "model_switched", fields);
            let view = next.view(&rec.model_messages, &rec.meta);
            rec.sent = view.messages.clone();
            rec.view = Some(view);
            rec.restarted = Some(next);
            rec.may_restart = false;
            rec.reask.clear();
            rec.errors.clear();
            rec.window = None;
            rec.cut_before = false;
            rec.reason = None;
            rec.attempt = 1;
            return ask(rec).await;
        }
    }
    if thought_cut && rec.cut_before {
        rec.reason = Some(thinking_note(crate::model_format::STOP_REASON_THINKING, &rec.meta));
    }
    rec.cut_before |= thought_cut;
    if attempt > CALL_RETRIES || rec.reason.is_some() {
        discard(&rec.run, &rec.parts, None).await;
        drop(rec.view.take());
        return stopped(rec).await;
    }
    discard(&rec.run, &rec.parts, Some((attempt, &error))).await;
    // the re-ask pair holds the answer up to where it went wrong: what follows a `Bad`
    // was read unseen (Q32)
    let answer = without_thinking(
        live.upto_bad(&raw)
            .unwrap_or_else(|| upto_bad(rec.cur().decl.reader, &raw)),
    );
    // a cut pass or a thinking-only answer is mostly thinking: an empty answer is no
    // message
    if !answer.trim().is_empty() {
        rec.reask.push(json!({"role": "assistant", "content": answer}));
    }
    rec.reask.push(json!({"role": "user", "content": why}));
    rec.attempt += 1;
    ask(rec).await;
}

/// `raw` up to the piece where `reader` reads a `Bad`, fed a char at a time as a stream
/// would feed it; the whole of it when none comes.
fn upto_bad<'a>(reader: &dyn crate::model_format::Reader, raw: &'a str) -> &'a str {
    let mut out = reader.outbound();
    for (i, c) in raw.char_indices() {
        let end = i + c.len_utf8();
        if out.feed(&raw[i..end]).iter().any(|o| matches!(o, Out::Bad { .. })) {
            return &raw[..end];
        }
    }
    raw
}

/// One answer as its reader tells it.
#[derive(Default)]
struct Read {
    text: String,
    bad: Option<String>,
    open: bool,
    repaired: Vec<(String, String)>,
}

impl Read {
    /// The reader's output, in order: the first `Bad` ends what is taken.
    fn take(&mut self, outs: Vec<Out>) {
        for out in outs {
            if self.bad.is_some() {
                return;
            }
            match out {
                Out::Text(t) | Out::Call { text: t } => self.text.push_str(&t),
                Out::Bad { report } => self.bad = Some(report),
                Out::Open { .. } => self.open = true,
                Out::Repaired { added, removed } => self.repaired.push((added, removed)),
            }
        }
    }
}

/// The sequence ended: the provider's cache is warm, and the calibration and the type
/// are the ones of the sequence the row comes from. The window, what was sent and the
/// request's parts.
fn closing(rec: &PassRec) -> (Option<Window>, Vec<Value>, Vec<Value>) {
    let state = &rec.run.state;
    *state.last_send_secs.lock().expect("last_send poisoned") = Some(crate::helpers::now_secs());
    let request_parts = crate::assembly::request_parts(rec.cur().decl, &rec.model_messages, &rec.sent);
    if rec.cur().kind != rec.pass.kind {
        *state.calib.lock().expect("calib poisoned") = None;
    }
    *state.last_type.lock().expect("last_type poisoned") = Some(rec.cur().kind);
    calibrate(state, rec.window, &rec.sent);
    (rec.window, rec.sent.clone(), request_parts)
}

/// THE HARNESS STOP (1.8): every attempt was malformed, or a second thinking cut. The
/// model is not asked again; the pass's row is the `stop` call, its reason worded by the
/// harness, and the give-up says so.
async fn stopped(rec: Box<PassRec>) {
    let (window, _sent, request_parts) = closing(&rec);
    let reason = rec.reason.clone().unwrap_or_else(|| {
        crate::model_format::fill(
            (rec.cur().decl.texts)(crate::model_format::Section::StopReason),
            &[("n", &rec.errors.len().to_string())],
        )
    });
    let mut args = Map::new();
    args.insert("reason".to_string(), json!(reason));
    let content = crate::tool_parse::render_call("stop", &args);
    let mut fields = Map::new();
    fields.insert("reason".into(), json!(reason));
    fields.insert("errors".into(), json!(rec.errors));
    let kind = rec.cur().kind;
    rec.cur().events().push(Mode::Turn.name(), kind, "stop", fields);
    let run = &rec.run;
    let message_id = run.kernel.mint_id();
    let mut row = json!({
        "role": "assistant",
        "content": content,
        "id": message_id,
        "parent": run.parent,
    });
    let landing = crate::used::land(&run.state, None);
    let next = assistant_attributes(
        &mut row,
        &run.self_id,
        &run.kernel,
        &run.provider,
        &landing.landed,
        window,
        rec.prev_request.as_ref(),
        rec.seed.clone(),
        message_id,
        request_parts,
    );
    // THE ROW IS WRITTEN AS A STEP (#795): the pass ends once the store took it
    let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
    crate::steps::append(&me, &k, &[row], (rec, landing, next), move |(rec, landing, next), r| async move {
        if let Err(e) = r {
            let PassRec { run, .. } = *rec;
            return failed(run, GenFailure::Store(e)).await;
        }
        landing.stored();
        *rec.run.state.prev_request.lock().expect("prev_request poisoned") = Some(next);
        // THE PASS'S TRACE ROWS (#766, 1.10), as steps; the give-up once they are written
        let events = crate::model_format::trace::of_turn(&rec.pass, rec.restarted.as_ref());
        let (me, k, state, cid) = (
            rec.run.self_id.clone(),
            Arc::clone(&rec.run.kernel),
            Arc::clone(&rec.run.state),
            rec.run.client_id.clone(),
        );
        crate::model_format::trace::write(&me, &k, &state, &cid, Some(message_id), events, rec, move |rec| async move {
            #[cfg(test)]
            {
                let mut rec = rec;
                let how = Generation::Stopped {
                    reason: reason.clone(),
                    message_id,
                };
                if observed(&mut rec.run, Ok(how)) {
                    crate::life::end(&rec.run.kernel, &rec.run.self_id, rec.run.epoch);
                    return;
                }
                give_up_and_end(&rec.run, &reason).await;
            }
            #[cfg(not(test))]
            give_up_and_end(&rec.run, &reason).await;
        })
        .await;
    })
    .await;
}

/// THE PASS GIVES UP (a harness stop, a failure no retry mends): its entry goes first,
/// so the `failed/gave_up` status is the pass's last word, then the inquiry it read is
/// told, then the pass ends.
async fn give_up_and_end(run: &Run, reason: &str) {
    let _ = crate::state::take_current(&run.self_id);
    crate::route::give_up(&run.kernel, &run.self_id, &run.client_id, reason).await;
    crate::life::end(&run.kernel, &run.self_id, run.epoch);
}

/// THE ACCEPTED ANSWER: stored as the pass's assistant row, the pass ends, and its calls
/// go out.
async fn answered(
    rec: Box<PassRec>,
    last_text: String,
    report: crate::provider::ProviderReport,
    accepted: crate::used::Accepted,
    open: bool,
) {
    let (window, sent, request_parts) = closing(&rec);
    let run = &rec.run;
    // A block still open when the model stopped generating is CUT: nothing of the pass
    // dispatches, its closed calls included, and the marker lists every call as not sent.
    let (cut_calls, call_streams) = {
        let mut p = rec.parts.lock().await;
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
    emit_status(&run.kernel, &run.state, &run.self_id, &run.client_id, "done", {
        let mut m = Map::new();
        m.insert("reason".to_string(), json!("ok"));
        let context_window = crate::context::resolve_context_window(&rec.meta, run.cfg.model_window);
        m.extend(usage_detail(
            window.map(Window::sum),
            Counts::of(&report).map(|c| c.completion),
            &sent,
            &last_text,
            rec.gen_started.elapsed().as_secs_f64(),
            context_window,
        ));
        m
    })
    .await;
    // THE ROW ID IS MINTED AT THE APPEND: ids agree with the store's order only while
    // they are minted as rows are appended.
    let message_id = run.kernel.mint_id();
    // STRIPPED: the deliberation is not part of the conversation (its spans ride the
    // row's thinking fields). A pass cut with a block open leaves the CUT MARKER.
    let mut row = json!({
        "role": "assistant",
        "content": match &cut_calls {
            Some(calls) => cut_marker(&last_text, "model stopped generating", calls),
            None => crate::call_check::repair(&without_thinking(&last_text)),
        },
        "id": message_id,
        "parent": run.parent,
    });
    // A `stop` THE MODEL WROTE (1.8) is traced with its reason and no `errors`
    if cut_calls.is_none() {
        crate::model_format::trace::note_model_stop(rec.cur(), row["content"].as_str().unwrap_or_default());
    }
    if let Some(m) = row.as_object_mut() {
        m.extend(crate::think::thinking_fields(&last_text));
    }
    let landing = crate::used::land(&run.state, Some(accepted));
    let next = assistant_attributes(
        &mut row,
        &run.self_id,
        &run.kernel,
        &run.provider,
        &landing.landed,
        window,
        rec.prev_request.as_ref(),
        rec.seed.clone(),
        message_id,
        request_parts,
    );
    // THE ROW IS WRITTEN AS A STEP (#795): the pass ends, and its calls go out, once the
    // store took it
    let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
    let done = Answered { last_text, message_id, call_streams, cut: cut_calls.is_some() };
    crate::steps::append(&me, &k, &[row], (rec, landing, next, done), move |(rec, landing, next, done), r| async move {
        if let Err(e) = r {
            let PassRec { run, .. } = *rec;
            return failed(run, GenFailure::Store(e)).await;
        }
        landing.stored();
        *rec.run.state.prev_request.lock().expect("prev_request poisoned") = Some(next);
        stored(rec, done).await;
    })
    .await;
}

/// The accepted answer, as its row write hands it on.
struct Answered {
    last_text: String,
    message_id: i64,
    call_streams: Vec<(String, String)>,
    cut: bool,
}

/// THE ANSWER ROW IS STORED: the pass's trace rows (steps), then the route.
async fn stored(rec: Box<PassRec>, done: Answered) {
    // THE PASS'S TRACE ROWS (#766, 1.10), its helpers' included, once its rows are stored
    let events = crate::model_format::trace::of_turn(&rec.pass, rec.restarted.as_ref());
    let (me, k, state, cid) = (
        rec.run.self_id.clone(),
        Arc::clone(&rec.run.kernel),
        Arc::clone(&rec.run.state),
        rec.run.client_id.clone(),
    );
    let row = done.message_id;
    crate::model_format::trace::write(&me, &k, &state, &cid, Some(row), events, (rec, done), |(rec, done)| traced(rec, done))
        .await;
}

/// The trace rows are written: the pass ends, and its calls go out.
async fn traced(rec: Box<PassRec>, done: Answered) {
    let Answered { last_text, message_id, call_streams, cut } = done;
    // a driven pass (tests) is told, and its answer is not routed
    #[cfg(test)]
    let rec = {
        let mut rec = rec;
        let how = Generation::Answered {
            text: last_text.clone(),
            message_id,
            cut,
        };
        if observed(&mut rec.run, Ok(how)) {
            crate::life::end(&rec.run.kernel, &rec.run.self_id, rec.run.epoch);
            return;
        }
        rec
    };
    route(rec, last_text, message_id, call_streams, cut)
}

/// THE PASS ENDS with its row written, and its calls go out: the route is owed work
/// until they are.
fn route(rec: Box<PassRec>, last_text: String, message_id: i64, call_streams: Vec<(String, String)>, cut: bool) {
    let run = &rec.run;
    let mut clients: Vec<String> = run.tail.iter().map(|a| a.client_id.clone()).collect();
    clients.dedup();
    crate::life::owe(&run.self_id, 1);
    crate::life::end(&run.kernel, &run.self_id, run.epoch);
    let route = crate::route::Routed {
        text: last_text,
        client_id: run.client_id.clone(),
        clients,
        send_timeout: crate::route::route_deadline(&run.kernel),
        model_window: run.cfg.model_window,
        message_id: Some(message_id),
        call_streams,
        cut,
    };
    let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
    run.kernel.spawn_task(&run.self_id, "route_reply", async move {
        crate::route::route_reply(me.clone(), Arc::clone(&k), route).await;
        crate::life::owe(&me, -1);
    });
}

/// THE PASS FAILED: said in the status, its error row stored (#177), an inquiry it
/// read told; a link failure is retried first, visibly, up to the record's
/// `transport_retries` (#198, Q4). The pass ends.
fn failed(run: Run, f: GenFailure) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        #[cfg(test)]
        let mut run = run;
        #[cfg(test)]
        {
            let retries = crate::record::ai::RECORD
                .get(&crate::helpers::agent_meta(&run.self_id, &run.kernel), "transport_retries")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as u32;
            let retried = matches!(&f, GenFailure::Provider(pe) if pe.is_transient() && run.retried < retries);
            if !retried && observed(&mut run, Err(f.clone())) {
                crate::life::end(&run.kernel, &run.self_id, run.epoch);
                return;
            }
        }
        let (kernel, self_id, state) = (Arc::clone(&run.kernel), run.self_id.clone(), Arc::clone(&run.state));
        let detail = |reason: &str, error: &str| {
            let mut m = Map::new();
            m.insert("reason".to_string(), json!(reason));
            m.insert("error".to_string(), json!(error));
            m
        };
        let why = match f {
            GenFailure::Provider(pe) => {
                let retries = crate::record::ai::RECORD
                    .get(&crate::helpers::agent_meta(&self_id, &kernel), "transport_retries")
                    .and_then(|v| v.as_i64())
                    .expect("transport_retries declares its default") as u32;
                if pe.is_transient() && run.retried < retries {
                    return retry(run, pe.message).await;
                }
                // a timeout's detail is its `reason` and the auto-fills, nothing else
                let d = if pe.is_timed_out() {
                    let mut m = Map::new();
                    m.insert("reason".to_string(), json!("timeout"));
                    m
                } else {
                    detail("error", &pe.message)
                };
                emit_status(&kernel, &state, &self_id, &run.client_id, "failed", d).await;
                // A PROVIDER FAILURE NO RETRY MENDS gives up at once, its error the reason
                let permanent = pe.permanent_code().is_some();
                // A FAILED PASS stores its own error row (#177), as a step; a REFUSED
                // RECORD (`ValueError`: the record cannot run) never opened a pass and
                // stores nothing.
                if pe.kind != "ValueError" {
                    let row = failed_row(&run, &pe.message);
                    crate::steps::append(&self_id, &kernel, &[row], run, move |run, r| async move {
                        if let Err(e) = r {
                            eprintln!("  [ai] {}: the pass's error row could not be stored: {e}", run.self_id);
                        }
                        told(run, pe.message, permanent).await;
                    })
                    .await;
                    return;
                }
                return told(run, pe.message, permanent).await;
            }
            // THE SEAM'S OWN WORDS: the projection refused (too small a window, an
            // unknown strategy)
            GenFailure::Projection(refusal) => {
                let e = fantastic_kernel::refusal::error_text(&refusal).unwrap_or_else(|| refusal.to_string());
                emit_status(&kernel, &state, &self_id, &run.client_id, "failed", detail("error", &e)).await;
                e
            }
            GenFailure::Store(e) => {
                emit_status(&kernel, &state, &self_id, &run.client_id, "failed", detail("error", &e.0)).await;
                e.0
            }
            GenFailure::PromptFile(e) => {
                emit_status(&kernel, &state, &self_id, &run.client_id, "failed", detail("error", &e.0)).await;
                e.0
            }
            GenFailure::FoldSummary(e) => {
                emit_status(&kernel, &state, &self_id, &run.client_id, "failed", detail("error", &e.0)).await;
                e.0
            }
            GenFailure::FoldAnchor(e) => {
                emit_status(&kernel, &state, &self_id, &run.client_id, "failed", detail("error", &e.0)).await;
                e.0
            }
        };
        told(run, why, false).await;
    })
}

/// THE FAILURE IS SAID (its status, its error row): a failure no retry mends gives up;
/// else an inquiry this pass read tells its asker (#722) — no answer comes from it —
/// and the pass ends.
async fn told(run: Run, why: String, permanent: bool) {
    if permanent {
        return give_up_and_end(&run, &why).await;
    }
    let (kernel, self_id) = (&run.kernel, &run.self_id);
    let mut body = Map::new();
    body.insert(
        "error".to_string(),
        json!(format!("{}'s turn failed: {why}", self_id.as_str())),
    );
    let mut clients: Vec<String> = run.tail.iter().map(|a| a.client_id.clone()).collect();
    clients.dedup();
    for c in clients {
        crate::route::close_inquiry(kernel, self_id.as_str(), &c, body.clone()).await;
    }
    crate::life::end(kernel, self_id, run.epoch);
}

/// A LINK FAILURE, RETRIED (#198 part 2): a visible notice row, the `retrying` status,
/// a short backoff, then the pass assembled and asked again.
async fn retry(mut run: Run, message: String) {
    let retries = crate::record::ai::RECORD
        .get(&crate::helpers::agent_meta(&run.self_id, &run.kernel), "transport_retries")
        .and_then(|v| v.as_i64())
        .expect("transport_retries declares its default") as u32;
    run.retried += 1;
    let attempt = run.retried;
    let row = json!({
        "role": "user",
        "parent": Value::Null,
        "kind": "notice",
        "content": format!(
            "[retry {attempt}/{retries}] the link dropped ({message}) — retrying the turn"
        ),
    });
    // the visible notice row is a step; the retry goes on once the store took it
    let (k, me) = (Arc::clone(&run.kernel), run.self_id.clone());
    crate::steps::append(&me, &k, &[row], run, move |run, _| async move {
        retry_after(run, retries, message).await;
    })
    .await;
}

/// The retry's notice is stored: `retrying` said, a short backoff, then the pass
/// gathered and asked again.
async fn retry_after(mut run: Run, retries: u32, message: String) {
    let attempt = run.retried;
    let mut detail = Map::new();
    detail.insert("attempt".to_string(), json!(attempt));
    detail.insert("of".to_string(), json!(retries));
    detail.insert("error".to_string(), json!(message));
    emit_status(&run.kernel, &run.state, &run.self_id, &run.client_id, "retrying", detail).await;
    // Backoff 2 s, 4 s, 8 s …; tests use a few milliseconds. A timer, not a wait on
    // anyone: a `stop` meanwhile ends the pass, and nothing is asked again.
    let wait = if cfg!(test) {
        std::time::Duration::from_millis(5)
    } else {
        std::time::Duration::from_secs(1u64 << attempt)
    };
    tokio::time::sleep(wait).await;
    if !crate::life::is_current(&run.self_id, run.epoch) {
        return;
    }
    run.deadline = ceiling_from_now();
    gather(run).await;
}

/// The error row of a pass that ends in a failure (#177), after the rows it read, so
/// the model reads the failure on its next pass and the operator sees it.
fn failed_row(run: &Run, reason: &str) -> Value {
    json!({
        "role": "user",
        "kind": "error",
        "content": format!(
            "[turn failed] {reason}. No answer was stored, and nothing this turn meant \
             to send was dispatched."
        ),
        "id": run.kernel.mint_id(),
        "parent": run.parent,
    })
}

/// A FAILED ATTEMPT IS MARKED, THEN ITS PARTS GO (Q40): the `retrying` status
/// `{attempt, of, error, kind: "call"}` first, when another attempt follows; then every
/// open part aborts with the reason `malformed`, the attempt's text leaves the in-flight
/// entry and a new splitter starts. Nothing streamed is taken back.
async fn discard(run: &Run, parts: &tokio::sync::Mutex<PassParts>, retry: Option<(usize, &str)>) {
    if let Some((attempt, error)) = retry {
        let mut detail = Map::new();
        detail.insert("attempt".to_string(), json!(attempt));
        detail.insert("of".to_string(), json!(CALL_RETRIES));
        detail.insert("error".to_string(), json!(error));
        detail.insert("kind".to_string(), json!("call"));
        emit_status(&run.kernel, &run.state, &run.self_id, &run.client_id, "retrying", detail).await;
    }
    {
        let mut p = parts.lock().await;
        // the deltas still batched reach their watchers first: an abort drops them
        p.publisher.push(Vec::new(), true).await;
        let evs = p.splitter.stop("malformed");
        p.publisher.push(evs, true).await;
        p.splitter = PartSplitter::new(run.self_id.as_str());
    }
    if let Some(c) = run.state.current_meta.lock().expect("current poisoned").as_mut() {
        c.text_so_far.clear();
    }
}

/// THE LIVE VIEW of one attempt (#795): its tokens through the type's reader, out as
/// the caller's `token` events, the in-flight entry's `text_so_far` and the pass's parts.
/// Display only: what runs is read from the settle. At a `Bad` nothing more is shown.
struct LiveView {
    kernel: Arc<Kernel>,
    state: Arc<BackendState>,
    self_id: AgentId,
    client_id: String,
    parts: Arc<tokio::sync::Mutex<PassParts>>,
    reader: tokio::sync::Mutex<(Box<dyn Outbound>, bool, bool)>,
    /// The raw text the live view was fed, and its length at the end of the token its
    /// reader read a `Bad` in: the re-ask pair's answer, when the settle's text agrees.
    seen: std::sync::Mutex<(String, Option<usize>)>,
}

impl LiveView {
    /// The answer up to the end of the token the live reader read a `Bad` in, when the
    /// settle's whole text begins with what the view was fed.
    fn upto_bad<'a>(&self, raw: &'a str) -> Option<&'a str> {
        let seen = self.seen.lock().expect("seen poisoned");
        let at = seen.1?;
        raw.starts_with(&seen.0[..at]).then(|| &raw[..at])
    }

    fn new(rec: &PassRec) -> Self {
        Self {
            kernel: Arc::clone(&rec.run.kernel),
            state: Arc::clone(&rec.run.state),
            self_id: rec.run.self_id.clone(),
            client_id: rec.run.client_id.clone(),
            parts: Arc::clone(&rec.parts),
            // (the reader, the first piece is still to come, a `Bad` was read)
            reader: tokio::sync::Mutex::new((rec.cur().decl.reader.outbound(), true, false)),
            seen: std::sync::Mutex::new((String::new(), None)),
        }
    }

    /// The reader's output, shown in order until a `Bad`.
    async fn show(&self, outs: Vec<Out>, first: &mut bool, bad: &mut bool) {
        for out in outs {
            if *bad {
                return;
            }
            match out {
                Out::Text(t) | Out::Call { text: t } => self.send(&t, first).await,
                Out::Bad { .. } => *bad = true,
                Out::Open { .. } | Out::Repaired { .. } => {}
            }
        }
    }

    /// What the reader still holds at the stream's end.
    async fn finish(&self) {
        let mut g = self.reader.lock().await;
        let (reader, first, bad) = &mut *g;
        if *bad {
            return;
        }
        let outs = reader.finish();
        self.show(outs, first, bad).await;
    }

    /// One piece of the attempt's internal text: the first one opens its `streaming`
    /// status.
    async fn send(&self, t: &str, first: &mut bool) {
        if *first {
            *first = false;
            emit_status(&self.kernel, &self.state, &self.self_id, &self.client_id, "streaming", Map::new()).await;
        }
        if t.is_empty() {
            return;
        }
        if let Some(c) = self.state.current_meta.lock().expect("current poisoned").as_mut() {
            c.text_so_far.push_str(t);
        }
        to_caller(
            &self.kernel,
            &self.self_id,
            &self.client_id,
            json!({"type": "token", "text": t, "source": self.self_id.as_str()}),
        )
        .await;
        let mut p = self.parts.lock().await;
        let evs = p.splitter.feed(t);
        p.publisher.push(evs, false).await;
    }
}

#[async_trait]
impl crate::provider::Live for LiveView {
    async fn token(&self, t: &str) {
        let mut g = self.reader.lock().await;
        let (reader, first, bad) = &mut *g;
        if *bad {
            return;
        }
        self.seen.lock().expect("seen poisoned").0.push_str(t);
        let outs = reader.feed(t);
        self.show(outs, first, bad).await;
        if *bad {
            let mut seen = self.seen.lock().expect("seen poisoned");
            seen.1 = Some(seen.0.len());
        }
    }
}

/// The assistant row's `attributes`, stored and returned by the retrieval verbs, never
/// shown to the model (a request carries a row's content only, [`crate::model_format::map`]): what served it (the provider agent, the chat itself
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

/// A report's first line: the `error` of a `retrying` status and of the trace.
fn first_line(report: &str) -> &str {
    report.lines().next().unwrap_or_default()
}

/// `template` (the re-ask after a thinking cut, [`crate::think::THINKING_CUT`], or the
/// stop after a second one) naming the cap the chat's pass carried
/// ([`crate::record::knobs_of`]).
fn thinking_note(template: &str, meta: &Map<String, Value>) -> String {
    let budget = crate::record::knobs_of(meta)
        .get("thinking_token_budget")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    crate::model_format::fill(template, &[("budget", &budget.to_string())])
}

/// The calibration (`calib`, `reported`) from a sequence's window: the prompt the
/// endpoint counted over our estimate of the view it was sent (`sent`, mapped). No window
/// (no report) leaves both as they were.
fn calibrate(state: &BackendState, window: Option<Window>, sent: &[Value]) {
    let Some(reported) = window.map(Window::sum).filter(|n| *n > 0) else {
        return;
    };
    let est = estimate_tokens(sent).max(1);
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
    sent: &[Value],
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
            m.insert("prompt_tokens".into(), json!(estimate_tokens(sent)));
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
/// is handed back.
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
