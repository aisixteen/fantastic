//! Provider — the streaming-chat seam every LLM backend implements.
//!
//! A provider is a PURE RAW-TEXT streamer: it takes plain chat messages
//! (role system/user/assistant/…, text content) and yields content tokens
//! ([`ProviderEvent::Token`]) only. It does NOT do tool-calling — Fantastic
//! NEVER uses a provider's native tool API. The tool contract belongs to this
//! crate: the model writes the envelope as TEXT, the provider streams that text
//! through knowing nothing about it, and [`crate::tool_parse`] +
//! [`crate::route`] read and dispatch it. One convention for every provider, and
//! adding a provider teaches it nothing.
//!
//! The provider holds no agent state either — the queue / lock / history /
//! context management all live in the shared machinery. Its ONE protocol duty is
//! the reasoning channel: read it and wrap it in
//! [`crate::think::THINK_OPEN`]/[`crate::think::THINK_CLOSE`] once per pass, or a
//! reasoning model's whole pass reads as an empty completion.

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// One event off a provider stream, in arrival order.
pub enum ProviderEvent {
    /// A content token (streamed to the caller live).
    Token(String),
}

/// What a provider failure means for the chain: the ONE class every backend maps its
/// own wire errors onto, so the retry policy lives once (`verbs.rs`) and no backend
/// carries its own list of retryable codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// The LINK failed (connect, stall, dropped stream, HTTP 408/429/5xx): the request
    /// was fine, so a retry may succeed (#198). `transport_retries` rerun the pass.
    Transient,
    /// The same request fails the same way every time (the key refused or missing,
    /// the model unknown or unavailable): never retried — the chain
    /// gives up with the error. Carries the refusal code the wire names it by
    /// ([`fantastic_kernel::refusal`]: `unauthorized`, `not_found`, `disabled`).
    Permanent(&'static str),
    /// This pass failed (a bad request, a broken stream): never retried; the pass fails
    /// with its error row, and the next message runs a pass of its own (#795).
    Pass,
}

/// The refusal reason for a provider's READ of its endpoint before a pass (a record
/// field left to the server: model, window, slots): the HTTP status through the one
/// classifier — a permanent one (the key, the path) is its permanent code, so the chain
/// gives up at once; a transient one is `transport_error`; any other answer, or no HTTP
/// status at all (unreachable, an unreadable body), is `bad_arg` on the field.
#[must_use]
pub fn read_reason(status: Option<u16>) -> &'static str {
    match status.map(http_class) {
        Some(ErrorClass::Permanent(code)) => code,
        Some(ErrorClass::Transient) => fantastic_kernel::refusal::TRANSPORT_ERROR,
        _ => fantastic_kernel::refusal::BAD_ARG,
    }
}

/// THE ONE HTTP CLASSIFIER: an endpoint's error status → what it means for the chain.
/// 401/403 = the key, 404 = the model or the path: permanent. 408/429/5xx = the link
/// or the load: transient. Anything else is this request's failure.
#[must_use]
pub fn http_class(status: u16) -> ErrorClass {
    use fantastic_kernel::refusal::{NOT_FOUND, UNAUTHORIZED};
    match status {
        401 | 403 => ErrorClass::Permanent(UNAUTHORIZED),
        404 => ErrorClass::Permanent(NOT_FOUND),
        408 | 429 | 500..=599 => ErrorClass::Transient,
        _ => ErrorClass::Pass,
    }
}

/// The class a wire `reason` names — how a failure crosses a `complete` reply
/// ([`ProviderError::reason`] is the other direction). A provider that cannot be
/// reached by name, or refuses the caller, is permanent like a refused key.
#[must_use]
pub fn reason_class(reason: Option<&str>) -> ErrorClass {
    use fantastic_kernel::refusal::{
        DISABLED, NOT_ALLOWED, NOT_FOUND, TRANSPORT_ERROR, UNAUTHORIZED, UNKNOWN_AGENT,
    };
    match reason {
        Some(TRANSPORT_ERROR) => ErrorClass::Transient,
        Some(UNAUTHORIZED) => ErrorClass::Permanent(UNAUTHORIZED),
        Some(NOT_FOUND) => ErrorClass::Permanent(NOT_FOUND),
        Some(DISABLED) => ErrorClass::Permanent(DISABLED),
        Some(UNKNOWN_AGENT) => ErrorClass::Permanent(UNKNOWN_AGENT),
        Some(NOT_ALLOWED) => ErrorClass::Permanent(NOT_ALLOWED),
        _ => ErrorClass::Pass,
    }
}

/// The kind of a request a `cancel` ended ([`ProviderError::cancelled`]).
pub const CANCELLED: &str = "CancelledError";
/// The kind of a request its deadline ended ([`ProviderError::timed_out`]).
pub const TIMED_OUT: &str = "TimeoutError";

/// Why a provider pass failed.
///
/// Every provider failure propagates out of the handler (python's `_send`
/// re-raises), which the kernel settles as `{error: "<Type>: <msg>",
/// raised: true}` — a FAILED execution, not a completed one carrying an error.
#[derive(Clone, Debug)]
pub struct ProviderError {
    /// What `status(failed, reason='error')` carries — python `str(e)`.
    pub message: String,
    /// The python exception class a raise renders as (`RuntimeError`, or
    /// `ValueError` for a record field the backend cannot run without).
    pub kind: &'static str,
    /// What the failure means for the chain: retried, given up on, or a retry pass.
    pub class: ErrorClass,
}

impl ProviderError {
    /// A failure python RAISES: the handler fails and the execution is FAILED.
    pub fn raised(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "RuntimeError",
            class: ErrorClass::Pass,
        }
    }

    /// A failure of the LINK (connect, stall, dropped stream, a transient 5xx):
    /// the request itself was fine, so a retry may succeed (#198).
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "RuntimeError",
            class: ErrorClass::Transient,
        }
    }

    /// A failure no retry can mend, named by its refusal `code` (see [`ErrorClass::Permanent`]).
    pub fn permanent(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            kind: "RuntimeError",
            class: ErrorClass::Permanent(code),
        }
    }

    /// The request was CANCELLED (a `cancel` of its execution): the pass it belonged to
    /// ends, never re-asked ([`Self::is_cancelled`]).
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: CANCELLED,
            class: ErrorClass::Pass,
        }
    }

    /// The request passed its deadline (the per-request ceiling, `secs`).
    #[must_use]
    pub fn timed_out(secs: f64) -> Self {
        Self {
            message: format!("send: timeout after {}s", fantastic_bundle::py_float_repr(secs)),
            kind: TIMED_OUT,
            class: ErrorClass::Pass,
        }
    }

    /// A cancel ended the request.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.kind == CANCELLED
    }

    /// The request's deadline ended it.
    #[must_use]
    pub fn is_timed_out(&self) -> bool {
        self.kind == TIMED_OUT
    }

    /// python's `ValueError` — a connector field the record MUST name (see
    /// [`crate::helpers::required_meta`]).
    pub fn value_error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "ValueError",
            class: ErrorClass::Pass,
        }
    }

    /// This failure, classed by the endpoint's HTTP `status` ([`http_class`]). Every
    /// backend maps an error status through here.
    #[must_use]
    pub fn for_status(mut self, status: u16) -> Self {
        self.class = http_class(status);
        self
    }

    /// A failure of the link: a retry may succeed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        self.class == ErrorClass::Transient
    }

    /// The refusal code a failure no retry can mend carries; `None` for any other.
    #[must_use]
    pub fn permanent_code(&self) -> Option<&'static str> {
        match self.class {
            ErrorClass::Permanent(code) => Some(code),
            _ => None,
        }
    }

    /// The wire `reason` of this failure on a `complete` reply ([`reason_class`]
    /// reads it back): `transport_error` for the link, the code of a permanent one,
    /// none for a pass's own failure.
    #[must_use]
    pub fn reason(&self) -> Option<&'static str> {
        match self.class {
            ErrorClass::Transient => Some(fantastic_kernel::refusal::TRANSPORT_ERROR),
            ErrorClass::Permanent(code) => Some(code),
            ErrorClass::Pass => None,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for ProviderError {
    fn from(message: String) -> Self {
        Self::raised(message)
    }
}

impl From<&str> for ProviderError {
    fn from(message: &str) -> Self {
        Self::raised(message)
    }
}

/// The provider of an agent whose record lacks a field its backend cannot run
/// without: every pass raises the refusal (python `required_meta` raising out of
/// the provider build).
pub struct Unconfigured(pub ProviderError);

#[async_trait]
impl Provider for Unconfigured {
    async fn chat(
        &self,
        _messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        Err(self.0.clone())
    }

    fn model(&self) -> String {
        String::new()
    }
}

/// A boxed async stream of provider events. `'static` so it can be
/// driven inside a spawned task.
pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, ProviderError>>;

/// The read clock a provider's stream runs on: the record's `stream_idle_seconds`, else
/// the default its declaration ([`crate::record::StreamClock`]) states.
pub fn stream_idle_of(meta: &Map<String, Value>) -> Duration {
    Duration::from_secs_f64(
        crate::record::clock::RECORD
            .get(meta, "stream_idle_seconds")
            .and_then(|v| v.as_f64())
            .expect("stream_idle_seconds declares a default"),
    )
}

/// A provider's read clock tripped: nothing arrived within the idle window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stalled {
    /// The idle window that passed, in seconds.
    pub idle_secs: f64,
}

impl Stalled {
    /// The pass's failure: `stream_stalled`, the seconds and the endpoint. A
    /// raise, so the call ends with an error row and `status(failed, reason='error')`.
    #[must_use]
    pub fn error(&self, name: &str, endpoint: &str) -> ProviderError {
        ProviderError::transport(format!(
            "{name}: stream_stalled — {endpoint} sent no bytes for {}s; this pass is NOT \
             complete",
            self.idle_secs
        ))
    }
}

/// `fut` gets `idle` to make progress: its output, or [`Stalled`] when the window
/// passes first. The caller drops the future — and with it the connection — on
/// `Err`. Used for the response headers and for every read of the body.
pub async fn within<F: std::future::Future>(idle: Duration, fut: F) -> Result<F::Output, Stalled> {
    tokio::time::timeout(idle, fut).await.map_err(|_| Stalled {
        idle_secs: idle.as_secs_f64(),
    })
}

#[cfg(test)]
mod stalled_tests {
    use super::*;

    #[test]
    fn a_stalled_read_is_a_transport_failure() {
        let err = Stalled { idle_secs: 5.0 }.error("x", "http://e");
        assert!(
            err.is_transient(),
            "a stalled read is the link: {}",
            err.message
        );
    }

    /// The shared HTTP classifier every backend maps its statuses through.
    #[test]
    fn http_statuses_are_classed_once() {
        use fantastic_kernel::refusal::{NOT_FOUND, UNAUTHORIZED};
        for (code, want) in [
            (401, ErrorClass::Permanent(UNAUTHORIZED)),
            (403, ErrorClass::Permanent(UNAUTHORIZED)),
            (404, ErrorClass::Permanent(NOT_FOUND)),
            (408, ErrorClass::Transient),
            (429, ErrorClass::Transient),
            (500, ErrorClass::Transient),
            (503, ErrorClass::Transient),
            (529, ErrorClass::Transient),
            (400, ErrorClass::Pass),
            (422, ErrorClass::Pass),
        ] {
            assert_eq!(http_class(code), want, "HTTP {code}");
            assert_eq!(
                ProviderError::raised("x").for_status(code).class,
                want,
                "HTTP {code}"
            );
        }
    }

    /// A class survives the `complete` wire: `reason` out, `reason_class` back in.
    #[test]
    fn a_class_crosses_the_wire_whole() {
        use fantastic_kernel::refusal::{DISABLED, NOT_FOUND, UNAUTHORIZED};
        for e in [
            ProviderError::transport("x"),
            ProviderError::raised("x"),
            ProviderError::permanent("x", UNAUTHORIZED),
            ProviderError::permanent("x", NOT_FOUND),
            ProviderError::permanent("x", DISABLED),
        ] {
            assert_eq!(reason_class(e.reason()), e.class, "{:?}", e.class);
        }
    }
}

/// What the endpoint REPORTED about the last pass — python's provider attributes
/// `last_prompt_tokens` / `last_stop_reason`. Shared with the stream a provider
/// hands out, which fills it in as the endpoint reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderReport {
    /// The endpoint's own count of the last prompt (calibrates the estimate), or
    /// `None` when it reported none.
    pub prompt_tokens: Option<i64>,
    /// The endpoint's count of the last pass's OUTPUT tokens, or `None` when it
    /// reported none (python `last_completion_tokens`).
    pub completion_tokens: Option<i64>,
    /// Why the last stream ended — `"max_tokens"` when it stopped AT the output
    /// bound (a side call's budget).
    pub stop_reason: Option<String>,
    /// Prompt tokens the endpoint served from its cache, or `None` when it has
    /// no such count (never 0 for "unknown").
    pub cache_read: Option<i64>,
    /// Prompt tokens the endpoint wrote to its cache, or `None` when it has no
    /// such count.
    pub cache_write: Option<i64>,
}

/// The shared cell a provider and its streams write the report into.
pub type ReportCell = std::sync::Arc<std::sync::Mutex<ProviderReport>>;

/// Streaming chat adapter for one upstream LLM. Built per agent by the
/// backend; the only per-backend seam the shared machinery talks to.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stream a plain-text completion for `messages` (role + text content).
    /// Yields [`ProviderEvent::Token`] content tokens in order, or an error
    /// describing a transport/HTTP failure. NO tools, NO state.
    ///
    /// `max_tokens` bounds THIS call's output (a side call's budget — a fold
    /// summary); `None` keeps the provider's own bound. A stream that stopped at
    /// the bound reports `stop_reason = "max_tokens"`.
    async fn chat(
        &self,
        messages: &[Value],
        max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError>;

    /// The upstream model id this provider talks to.
    fn model(&self) -> String;

    /// Whether this provider can run a pass at all — checked FIRST in a pass
    /// ([`crate::agent_loop::start`]), before the owed recall, the
    /// assembly and the projection. `Err` fails the pass with that refusal,
    /// exactly as a failed pass does. Python `_run` builds its provider before
    /// anything else and a raise there ends the pass; a provider that knows up
    /// front it cannot answer (foundation_models_backend: the on-device model
    /// is unavailable) says so here. Default: ready.
    fn ready(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    /// What the endpoint reported about the last pass. A provider that reads no
    /// usage reports nothing — `None` everywhere, which leaves the calibration at
    /// 1.0 and never marks a summary cut.
    fn report(&self) -> ProviderReport {
        ProviderReport::default()
    }

    /// A FRESH prompt-size reading per pass: a provider that reports nothing this
    /// time must not leave last pass's number standing. Python
    /// `provider.last_prompt_tokens = None`.
    fn clear_prompt_tokens(&self) {}

    /// Forget the last stop reason before a bounded side call. Python
    /// `provider.last_stop_reason = None`.
    fn clear_stop_reason(&self) {}

    /// Whether this provider's SERVER enforces a pass's `thinking_token_budget` (#782):
    /// it ends the thinking itself and the model goes on into its answer. Default: it
    /// cannot, and the harness cuts the pass's thinking on the stream
    /// ([`crate::remote_provider::complete`]).
    fn enforces_thinking_budget(&self) -> bool {
        false
    }

    /// The provider AGENT this one reaches through the kernel, when it is one: its
    /// request goes out as a `complete` call ([`begin`]). `None`: an in-process
    /// provider, whose stream is read as its own I/O.
    fn remote(&self) -> Option<&crate::remote_provider::RemoteProvider> {
        None
    }
}

/// What runs with a request's settle: the whole raw answer (thinking markers intact),
/// or why there is none. It runs as a message to the chat; nothing waits for it.
pub type Done = Box<
    dyn FnOnce(Result<String, ProviderError>) -> futures_util::future::BoxFuture<'static, ()>
        + Send,
>;

/// Where a request's tokens go WHILE it streams: the live view only (#795). Nothing
/// here is ever parsed or routed: the answer is read from the settle ([`Done`]).
#[async_trait]
pub trait Live: Send + Sync {
    /// One token, in order.
    async fn token(&self, t: &str);
}

/// What runs once a request's execution exists: its id, for `stop` and `cancel`.
pub type Launched = Box<dyn FnOnce(fantastic_kernel::TaskId) + Send>;

/// ONE REQUEST AS A MESSAGE (#795): `messages` go out and this returns at once; `done`
/// gets the whole answer when the request settles. A provider agent is called through
/// the kernel ([`crate::remote_provider::RemoteProvider::begin`]: the request's images
/// first, each a step); an in-process provider's stream is read in a task of `owner`'s,
/// its own I/O, watched by `abort`. `deadline` is how long the request may take (what
/// is left of its pass's ceiling, `ceiling` seconds whole): past it, `done` gets the
/// timeout. `launched` gets the execution the request runs as, once it exists, for
/// `stop` and `cancel` (never, when it was refused before one existed: `done` has its
/// error).
#[allow(clippy::too_many_arguments)]
pub async fn begin(
    provider: Arc<dyn Provider>,
    kernel: &Arc<fantastic_kernel::Kernel>,
    owner: &fantastic_kernel::AgentId,
    messages: Vec<Value>,
    max_tokens: Option<i64>,
    deadline: (Duration, f64),
    abort: Arc<tokio::sync::Notify>,
    live: Arc<dyn Live>,
    done: Done,
    launched: Launched,
) {
    if let Some(remote) = provider.remote() {
        return remote.begin(messages, max_tokens, deadline, live, done, launched).await;
    }
    let done = DoneOnce(Some(done), fantastic_kernel::Inherited::capture());
    let pid = kernel.spawn_task(owner, "complete", async move {
        let mut done = done;
        let read = async {
            let mut stream = provider.chat(&messages, max_tokens).await?;
            let mut text = String::new();
            while let Some(ev) = futures_util::StreamExt::next(&mut stream).await {
                let ProviderEvent::Token(t) = ev?;
                live.token(&t).await;
                text.push_str(&t);
            }
            Ok(text)
        };
        let (within, ceiling) = deadline;
        let read = tokio::select! {
            biased;
            // STOPPED: the stream is dropped, and the stop closed the pass already
            _ = abort.notified() => return,
            r = tokio::time::timeout(within, read) => {
                r.unwrap_or_else(|_| Err(ProviderError::timed_out(ceiling)))
            }
        };
        if let Some(done) = done.0.take() {
            done(read).await;
        }
    });
    launched(pid);
}

/// A request's [`Done`] that runs once: when the task reading it is dropped first (a
/// `cancel` of it, a stop), it runs with the cancel, so the pass it belongs to is never
/// left waiting for a settle that cannot come.
/// It runs as whoever sent the request (captured when it was sent).
struct DoneOnce(Option<Done>, fantastic_kernel::Inherited);

impl Drop for DoneOnce {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            if let Ok(h) = tokio::runtime::Handle::try_current() {
                let task = self
                    .1
                    .clone()
                    .scope(done(Err(ProviderError::cancelled("the request was cancelled"))));
                h.spawn(task);
            }
        }
    }
}

#[cfg(test)]
#[path = "tests_provider_clock.rs"]
mod tests_provider_clock;
