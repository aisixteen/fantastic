//! THE PASS PATH (#766, spec 1.5): the one place the chat asks its model.
//!
//! A [`Pass`] is built once per pass (in `agent_loop::prepare`) and once per verb that
//! runs a helper outside a turn (`context_recall`, `context_fold`), and is handed down
//! where the provider went before. It holds the chat's provider and state and the model
//! TYPE of the pass, fixed for its life: the type's declaration ([`Decl`]), which the
//! map writes every model-visible text from ([`super::map`]).
//!
//! Two modes:
//! - [`Mode::Turn`], the agent's turn: its view mapped for the type through the
//!   chat's render cache ([`Pass::view`]), one request ([`Pass::request`]), the
//!   endpoint's report read after the stream ended ([`Pass::report`]).
//! - [`Mode::Task`], the harness's helpers (the fold summarizer and `compose`): the
//!   same map, uncached, on each message, because their prompts hold history rows with
//!   calls; one request; no parse, no check, no retry; the result is plain text with the
//!   reasoning stripped ([`Pass::task`]).
//!
//! A HELPER'S REQUEST IS A STEP (#795): a helper is plain code that READS its task
//! answers from the pass's book ([`Pass::task`]); one it has not got yet makes it stop
//! with [`Pending`], and the step that ran it sends that request ([`answer`], a
//! `complete` through [`crate::provider::begin`]). The settle writes the answer in the
//! book and runs the helper again from its start ([`drive`]): every answer it read
//! before is read again, in the same order, so the helper is a pure function of its
//! input and its answers, and nothing waits for a model.
//!
//! The window estimate of a message is counted on the bytes the pass sends of it
//! ([`Pass::tokens`]), so a projection fits what this type is sent.
//!
//! Every request clears the provider's counts first and reads its report after, so a
//! count is never read from a report an earlier request left.
//!
//! Both modes hand their counts to the chat's token accounting ([`crate::used`]): a
//! task pass's go to `task` as it ends; a turn attempt's go to `turn.rejected`
//! ([`Pass::rejected`]) or are held as the accepted one ([`Pass::accepted`]) until the
//! turn's row is stored.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use sha2::{Digest as _, Sha256};

use super::cache::{bound_of, epoch, Ctx, View};
use super::map::Shown;
use super::{trace, Decl};
use crate::provider::{Provider, ProviderError, ProviderReport};
use crate::state::BackendState;

/// Which of the two kinds of model request a pass makes: the `mode` of its trace rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The agent's turn: rendered through the cache, its answer read as calls.
    Turn,
    /// A harness helper: rendered uncached, its answer plain text.
    Task,
}

impl Mode {
    /// The trace value.
    #[must_use]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Task => "task",
        }
    }
}

/// What a task pass gives back.
#[derive(Clone, Debug)]
pub(crate) struct Task {
    /// The answer, reasoning stripped; never parsed.
    pub(crate) text: String,
    /// The endpoint's report of the request.
    pub(crate) report: ProviderReport,
}

/// One chat's model requests of one turn run, or of one verb.
pub(crate) struct Pass {
    /// The chat's provider: the raw-text streamer.
    pub(crate) provider: Arc<dyn Provider>,
    /// The chat's state: its render cache and the model its provider last named.
    pub(crate) state: Arc<BackendState>,
    /// The model type of the pass.
    pub(crate) kind: &'static str,
    /// The type's declaration: its call renderer and reader, markers and sections.
    pub(crate) decl: &'static Decl<'static>,
    /// The chat's own id: the `{self}` of a marker's unfold call.
    pub(crate) own: String,
    /// The model id the type was chosen by ([`super::rules::model_of`]).
    pub(crate) model: Option<String>,
    /// The chat record's `provider_id`.
    pub(crate) provider_id: Option<String>,
    /// The trace events of the pass's requests, both modes, in order: a turn takes
    /// them at its end; a verb's pass drops them (a task pass outside a turn writes no
    /// trace row).
    events: Mutex<trace::Events>,
    /// The window estimate of each message mapped so far, by the digest of what it maps
    /// from: a projection sizes one message many times.
    sizes: Mutex<BTreeMap<[u8; 32], i64>>,
    /// The task answers its helpers read, in the order they asked ([`Pass::task`]).
    book: Mutex<Book>,
}

/// A HELPER STOPPED for a task answer it has not got yet ([`Pass::task`]): the step that
/// ran it sends the request ([`answer`]) and runs it again once it settled ([`drive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pending;

/// One task request: the messages in the type's form and the output bound.
#[derive(Debug, Clone, PartialEq)]
struct Asked {
    sent: Vec<Value>,
    max_tokens: Option<i64>,
}

/// The task answers of a pass, in ask order: each request with its answer (`None`: the
/// request failed), the next to read, and the request the helper stopped at.
#[derive(Debug, Default)]
struct Book {
    answered: Vec<(Asked, Option<Task>)>,
    cursor: usize,
    need: Option<Asked>,
}

impl Pass {
    /// THE PASS OF `self_id`'S NEXT REQUEST, as a STEP (#795): its type from the model id
    /// ([`super::rules::model_of`], whose `models` ask is a call of its own;
    /// [`super::rules::type_of`]), `rows` the history the pass read; `next` gets it.
    pub(crate) async fn begin<R, F, Fut>(
        provider: Arc<dyn Provider>,
        self_id: &AgentId,
        state: &Arc<BackendState>,
        kernel: &Arc<Kernel>,
        rows: Vec<Value>,
        rec: R,
        next: F,
    ) where
        R: Send + 'static,
        F: FnOnce(R, Self) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let meta = crate::helpers::agent_meta(self_id, kernel);
        let (st, me, m) = (Arc::clone(state), self_id.clone(), meta.clone());
        super::rules::model_of(kernel, &meta, state, rows, rec, move |rec, model| async move {
            let pass = Self::with_model(provider, &me, &st, &m, model);
            next(rec, pass).await;
        })
        .await;
    }

    /// TESTS: [`Self::begin`] driven to its end over the chat's history.
    #[cfg(test)]
    pub(crate) async fn new(
        provider: Arc<dyn Provider>,
        self_id: &AgentId,
        state: &Arc<BackendState>,
        kernel: &Arc<Kernel>,
    ) -> Self {
        let rows = crate::memory::load_history(self_id, kernel, false).await.unwrap_or_default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        Self::begin(provider, self_id, state, kernel, rows, tx, |tx, p| async move {
            let _ = tx.send(p);
        })
        .await;
        rx.await.expect("the pass step ran")
    }

    /// The pass of `self_id`'s next request for the model id `model`.
    pub(crate) fn with_model(
        provider: Arc<dyn Provider>,
        self_id: &AgentId,
        state: &Arc<BackendState>,
        meta: &Map<String, Value>,
        model: Option<String>,
    ) -> Self {
        let kind = super::rules::type_of(
            super::rules::shipped(),
            model.as_deref().unwrap_or_default(),
        );
        let provider_id = meta
            .get(crate::remote_provider::PROVIDER_FIELD)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Self::of(
            provider,
            Arc::clone(state),
            kind,
            model,
            provider_id,
            self_id.as_str(),
        )
    }

    /// A pass of the registered type `kind` for the chat `own`.
    fn of(
        provider: Arc<dyn Provider>,
        state: Arc<BackendState>,
        kind: &str,
        model: Option<String>,
        provider_id: Option<String>,
        own: &str,
    ) -> Self {
        let decl = super::of(kind);
        Self {
            provider,
            state,
            kind: decl.name,
            decl,
            own: own.to_string(),
            model,
            provider_id,
            events: Mutex::new(trace::Events::default()),
            sizes: Mutex::new(BTreeMap::new()),
            book: Mutex::new(Book::default()),
        }
    }

    /// A `default` pass over `provider` on a state of its own (tests).
    #[cfg(test)]
    pub(crate) fn for_test(provider: Arc<dyn Provider>) -> Self {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let own = format!("pass_test_{n}");
        let state = crate::state::state_for(&AgentId::from(own.as_str()));
        Self::of(provider, state, super::DEFAULT, None, None, &own)
    }

    /// A pass of type `kind` over `provider` on `state`, for model `model` (tests).
    #[cfg(test)]
    pub(crate) fn typed_for_test(
        provider: Arc<dyn Provider>,
        state: Arc<BackendState>,
        kind: &str,
        model: Option<&str>,
    ) -> Self {
        Self::of(
            provider,
            state,
            kind,
            model.map(str::to_string),
            Some("p".to_string()),
            "ai",
        )
    }

    /// The trace events so far, to add to ([`View::finish`]) or read.
    pub(crate) fn events(&self) -> MutexGuard<'_, trace::Events> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The trace events so far, taken.
    pub(crate) fn take_events(&self) -> trace::Events {
        std::mem::take(&mut *self.events())
    }

    /// THE TYPE RESTART (Q26, step 4): the same chat's pass for `model`, the model a
    /// failed attempt's reply named, when its type is not this pass's. The events so far
    /// move to it, so the turn takes them all at its end. `None`: no model named, or one
    /// of this pass's own type.
    pub(crate) fn retyped(&self, model: &str) -> Option<Self> {
        if model.is_empty() {
            return None;
        }
        let kind = super::rules::type_of(super::rules::shipped(), model);
        if kind == self.kind {
            return None;
        }
        let next = Self::of(
            Arc::clone(&self.provider),
            Arc::clone(&self.state),
            kind,
            Some(model.to_string()),
            self.provider_id.clone(),
            &self.own,
        );
        let moved = self.take_events();
        *next.events() = moved;
        Some(next)
    }

    /// THE TURN'S VIEW (step 3): `messages` in flight (their pieces, their row `id`, the
    /// form each took) mapped for the type through the chat's render cache; the bound is
    /// read from `meta`, the chat's record, at every pass. The [`View`] holds the pass's
    /// pins until it is finished or dropped.
    pub(crate) fn view(&self, messages: &[Value], meta: &Map<String, Value>) -> View<'static> {
        let shown: Vec<Shown> = messages.iter().map(Shown::of).collect();
        let last_send = *self
            .state
            .last_send_secs
            .lock()
            .expect("last_send poisoned");
        let ctx = Ctx {
            epoch: epoch(),
            bound: bound_of(meta),
            model: self.model.as_deref(),
            provider_id: self.provider_id.as_deref(),
            last_send,
        };
        self.state
            .render_cache
            .view_shared(self.decl, &self.own, &shown, &ctx)
    }

    /// THE WINDOW ESTIMATE of `message` in flight: [`crate::context::estimate_one`] of
    /// the bytes this pass sends of it ([`super::map::message`]).
    #[must_use]
    pub(crate) fn tokens(&self, message: &Value) -> i64 {
        let shown = Shown::of(message);
        let mut h = Sha256::new();
        h.update(super::map::canonical(&shown));
        h.update(
            serde_json::to_vec(&(&shown.role, &shown.images, &shown.cache_mark))
                .unwrap_or_default(),
        );
        let key: [u8; 32] = h.finalize().into();
        let mut sizes = self
            .sizes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = sizes.get(&key) {
            return *n;
        }
        let n = crate::context::estimate_one(&super::map::message(self.decl, &self.own, &shown));
        sizes.insert(key, n);
        n
    }

    /// [`Pass::tokens`] summed over `messages`.
    #[must_use]
    pub(crate) fn tokens_all(&self, messages: &[Value]) -> i64 {
        messages.iter().map(|m| self.tokens(m)).sum()
    }

    /// THE REPORT READ, after a request's stream ended: the endpoint's report. When it
    /// holds cache counts, a `prefix_cache` event carries them as reported, the one
    /// measured value about the endpoint's cache; nothing else reads them as a
    /// prediction. In a turn, the model the provider names is kept as the chat's
    /// `model_seen` (spec 1.3).
    pub(crate) fn report(&self, mode: Mode) -> ProviderReport {
        let report = self.provider.report();
        if report.cache_read.is_some() || report.cache_write.is_some() {
            let mut fields = Map::new();
            if let Some(n) = report.cache_read {
                fields.insert("cache_read".into(), json!(n));
            }
            if let Some(n) = report.cache_write {
                fields.insert("cache_write".into(), json!(n));
            }
            self.events()
                .push(mode.name(), self.kind, "prefix_cache", fields);
        }
        if mode == Mode::Turn {
            super::rules::note_seen(&self.state, &self.provider.model());
        }
        report
    }

    /// A turn attempt the chat threw away: its counts wait for the next row's
    /// `turn.rejected` ([`crate::used::reject`]).
    pub(crate) fn rejected(&self, report: &ProviderReport) {
        crate::used::reject(&self.state, report);
    }

    /// The turn attempt the chat took: its counts held until its row is stored
    /// ([`crate::used::Accepted`]).
    #[must_use]
    pub(crate) fn accepted(&self, report: &ProviderReport) -> crate::used::Accepted {
        crate::used::Accepted::hold(&self.state, report)
    }

    /// A TASK PASS (step 7), READ FROM THE BOOK (#795): each message mapped for the type
    /// with no cache (one stored text each), the answer whole, reasoning stripped; never
    /// parsed, so a call form in it is text. `Ok(None)`: the request failed or its stream
    /// did. [`Pending`]: this request has no answer yet — the helper stops, and the step
    /// that runs it sends the request ([`answer`]) and runs it again ([`drive`]).
    pub(crate) fn task(&self, messages: &[Value], max_tokens: Option<i64>) -> Result<Option<Task>, Pending> {
        let asked = Asked {
            sent: super::map::messages(self.decl, &self.own, messages),
            max_tokens,
        };
        let mut b = self.book.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let i = b.cursor;
        match b.answered.get(i) {
            Some((a, t)) if *a == asked => {
                let t = t.clone();
                b.cursor += 1;
                Ok(t)
            }
            _ => {
                // a helper that asks something else from here on: its later answers are not its
                b.answered.truncate(i);
                b.need = Some(asked);
                Err(Pending)
            }
        }
    }

    /// The helper runs again from its start: it reads its answers from the first.
    fn replay(&self) {
        let mut b = self.book.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        b.cursor = 0;
        b.need = None;
    }

    /// The request the helper stopped at, taken.
    fn take_need(&self) -> Option<Asked> {
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .need
            .take()
    }

    /// A task request settled: its answer (or `None`) is written in the book, its counts
    /// go to the next stored row's `task` ([`crate::used::task`]).
    fn answered(&self, asked: Asked, read: Result<String, ProviderError>) {
        let task = read.ok().map(|text| {
            let report = self.report(Mode::Task);
            crate::used::task(&self.state, &report);
            Task {
                text: crate::think::without_thinking(&text),
                report,
            }
        });
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .answered
            .push((asked, task));
    }
}

/// Where a task request's tokens go: nowhere — its answer is read from the settle.
struct Unseen;

#[async_trait::async_trait]
impl crate::provider::Live for Unseen {
    async fn token(&self, _t: &str) {}
}

/// THE HELPER'S REQUEST, as a STEP (#795): the request `pass`'s helper stopped at goes
/// out as one `complete` ([`crate::provider::begin`], `owner`'s), and its settle writes
/// the answer in the book and runs `next` with `rec` and the pass. A pass whose helper
/// stopped at nothing runs `next` at once.
pub(crate) async fn answer<R, F, Fut>(kernel: &Arc<Kernel>, owner: &AgentId, pass: Pass, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Pass) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Some(asked) = pass.take_need() else {
        return next(rec, pass).await;
    };
    let provider = Arc::clone(&pass.provider);
    // A FRESH reading per request: the provider's counts cleared first
    provider.clear_prompt_tokens();
    provider.clear_stop_reason();
    let sent = asked.sent.clone();
    let max_tokens = asked.max_tokens;
    let done: crate::provider::Done = Box::new(move |read| {
        Box::pin(async move {
            pass.answered(asked, read);
            next(rec, pass).await;
        })
    });
    let secs = crate::constants::send_timeout_secs();
    crate::provider::begin(
        provider,
        kernel,
        owner,
        sent,
        max_tokens,
        (std::time::Duration::from_secs_f64(secs), secs),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(Unseen),
        done,
        Box::new(|_| {}),
    )
    .await;
}

/// A HELPER RUN TO ITS END, as STEPS (#795): `helper` runs over the pass; each time it
/// stops for a task answer ([`Pending`]), that request is sent ([`answer`]) and the
/// helper runs again, from its start, when it settled. `next` gets the helper's result.
pub(crate) fn drive<R, T, H, F, Fut>(
    kernel: Arc<Kernel>,
    owner: AgentId,
    pass: Pass,
    rec: R,
    helper: H,
    next: F,
) -> futures_util::future::BoxFuture<'static, ()>
where
    R: Send + 'static,
    T: Send + 'static,
    H: Fn(&Pass, &mut R) -> Result<T, Pending> + Send + Sync + 'static,
    F: FnOnce(R, Pass, T) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    Box::pin(async move {
        pass.replay();
        let mut rec = rec;
        match helper(&pass, &mut rec) {
            Ok(t) => next(rec, pass, t).await,
            Err(Pending) => {
                let (k, me) = (Arc::clone(&kernel), owner.clone());
                answer(&k, &me, pass, rec, move |rec, pass| drive(kernel, owner, pass, rec, helper, next)).await;
            }
        }
    })
}

/// TESTS: `helper` run to its end over `pass`, its task requests read whole here (an
/// in-process provider's stream; a provider agent's request through
/// [`crate::provider::begin`], its settle awaited by the test).
#[cfg(test)]
pub(crate) async fn settled<T>(pass: &Pass, kernel: Option<&Arc<Kernel>>, helper: impl Fn(&Pass) -> Result<T, Pending>) -> T {
    pass.replay();
    loop {
        if let Ok(t) = helper(pass) {
            return t;
        }
        let asked = pass.take_need().expect("a pending helper names its request");
        pass.provider.clear_prompt_tokens();
        pass.provider.clear_stop_reason();
        let read = if pass.provider.remote().is_some() {
            let kernel = kernel.expect("a provider agent is reached through a kernel");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let done: crate::provider::Done = Box::new(move |r| {
                Box::pin(async move {
                    let _ = tx.send(r);
                })
            });
            let secs = crate::constants::send_timeout_secs();
            crate::provider::begin(
                Arc::clone(&pass.provider),
                kernel,
                &AgentId::from(pass.own.as_str()),
                asked.sent.clone(),
                asked.max_tokens,
                (std::time::Duration::from_secs_f64(secs), secs),
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(Unseen),
                done,
                Box::new(|_| {}),
            )
            .await;
            rx.await.unwrap_or_else(|_| Err(ProviderError::cancelled("the request was dropped")))
        } else {
            async {
                let mut stream = pass.provider.chat(&asked.sent, asked.max_tokens).await?;
                let mut text = String::new();
                while let Some(ev) = futures_util::StreamExt::next(&mut stream).await {
                    let crate::provider::ProviderEvent::Token(t) = ev?;
                    text.push_str(&t);
                }
                Ok::<String, ProviderError>(text)
            }
            .await
        };
        pass.answered(asked, read);
        pass.replay();
    }
}
