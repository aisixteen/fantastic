//! THE PASS PATH (#766, spec 1.5): the one place the chat asks its model.
//!
//! A [`Pass`] is built once per turn run (in `run_generation`) and once per verb that
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
//! The window estimate of a message is counted on the bytes the pass sends of it
//! ([`Pass::tokens`]), so a projection fits what this type is sent.
//!
//! Every request clears the provider's counts first and reads its report after, so a
//! count is never read from a report an earlier request left. `pass.rs` holds the only
//! `provider.chat` call of the crate outside tests.
//!
//! Both modes hand their counts to the chat's token accounting ([`crate::used`]): a
//! task pass's go to `task` as it ends; a turn attempt's go to `turn.rejected`
//! ([`Pass::rejected`]) or are held as the accepted one ([`Pass::accepted`]) until the
//! turn's row is stored.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use sha2::{Digest as _, Sha256};

use super::cache::{bound_of, epoch, Ctx, View};
use super::map::Shown;
use super::{trace, Decl};
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream};
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
#[derive(Debug)]
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
}

impl Pass {
    /// The pass of `self_id`'s next request: its type from the model id
    /// ([`super::rules::model_of`], [`super::rules::type_of`]).
    pub(crate) async fn new(
        provider: Arc<dyn Provider>,
        self_id: &AgentId,
        state: &Arc<BackendState>,
        kernel: &Arc<Kernel>,
    ) -> Self {
        let meta = crate::helpers::agent_meta(self_id, kernel);
        let model = super::rules::model_of(self_id, kernel, &meta, state).await;
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
    pub(crate) fn view(&self, messages: &[Value], meta: &Map<String, Value>) -> View<'_> {
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
            .view(self.decl, &self.own, &shown, &ctx)
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

    /// THE ONE MODEL REQUEST: the provider's counts and stop reason cleared, then
    /// `messages` (already in the type's form) sent. `max_tokens` bounds this call's
    /// output; `None` keeps the provider's own bound.
    ///
    /// # Errors
    /// The provider refused or failed to open the stream.
    pub(crate) async fn request(
        &self,
        messages: &[Value],
        max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.provider.clear_prompt_tokens();
        self.provider.clear_stop_reason();
        self.provider.chat(messages, max_tokens).await
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

    /// A TASK PASS (step 7): each message mapped for the type with no cache (one stored
    /// text each), one request, the answer read whole, reasoning stripped; never parsed,
    /// so a call form in it is text. `None`: the request failed or its stream did.
    pub(crate) async fn task(&self, messages: &[Value], max_tokens: Option<i64>) -> Option<Task> {
        let sent = super::map::messages(self.decl, &self.own, messages);
        let mut stream = self.request(&sent, max_tokens).await.ok()?;
        let mut parts: Vec<String> = Vec::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ProviderEvent::Token(t)) => parts.push(t),
                Err(_) => return None,
            }
        }
        drop(stream);
        let report = self.report(Mode::Task);
        // its counts wait for the next stored row's `task`
        crate::used::task(&self.state, &report);
        Some(Task {
            text: crate::think::without_thinking(&parts.join("")),
            report,
        })
    }
}
