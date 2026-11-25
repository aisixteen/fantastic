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
//! [`crate::think::THINK_OPEN`]/[`crate::think::THINK_CLOSE`] once per turn, or a
//! reasoning model's whole turn reads as an empty completion.

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde_json::{Map, Value};
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
    /// was fine, so a retry may succeed (#198). `transport_retries` rerun the turn.
    Transient,
    /// The same request fails the same way every time (the key refused or missing,
    /// the model unknown or unavailable): never retried, never nudged — the chain
    /// gives up with the error. Carries the refusal code the wire names it by
    /// ([`fantastic_kernel::refusal`]: `unauthorized`, `not_found`, `disabled`).
    Permanent(&'static str),
    /// This pass failed (a bad request, a broken stream): the failed turn's own retry
    /// turn (#533) may still answer.
    Turn,
}

/// The refusal reason for a provider's READ of its endpoint before a turn (a record
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
        _ => ErrorClass::Turn,
    }
}

/// The class a wire `reason` names — how a failure crosses a `complete` reply
/// ([`ProviderError::reason`] is the other direction). A provider that cannot be
/// reached by name, or refuses the caller, is permanent like a refused key.
#[must_use]
pub fn reason_class(reason: Option<&str>) -> ErrorClass {
    use fantastic_kernel::refusal::{
        DISABLED, NOT_FOUND, TRANSPORT_ERROR, UNAUTHORIZED, UNKNOWN_AGENT,
    };
    match reason {
        Some(TRANSPORT_ERROR) => ErrorClass::Transient,
        Some(UNAUTHORIZED) => ErrorClass::Permanent(UNAUTHORIZED),
        Some(NOT_FOUND) => ErrorClass::Permanent(NOT_FOUND),
        Some(DISABLED) => ErrorClass::Permanent(DISABLED),
        Some(UNKNOWN_AGENT) => ErrorClass::Permanent(UNKNOWN_AGENT),
        _ => ErrorClass::Turn,
    }
}

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
    /// What the failure means for the chain: retried, given up on, or a retry turn.
    pub class: ErrorClass,
}

impl ProviderError {
    /// A failure python RAISES: the handler fails and the execution is FAILED.
    pub fn raised(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "RuntimeError",
            class: ErrorClass::Turn,
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

    /// python's `ValueError` — a connector field the record MUST name (see
    /// [`crate::helpers::required_meta`]).
    pub fn value_error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "ValueError",
            class: ErrorClass::Turn,
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
    /// none for a turn's own failure.
    #[must_use]
    pub fn reason(&self) -> Option<&'static str> {
        match self.class {
            ErrorClass::Transient => Some(fantastic_kernel::refusal::TRANSPORT_ERROR),
            ErrorClass::Permanent(code) => Some(code),
            ErrorClass::Turn => None,
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
    /// The turn's failure: `stream_stalled`, the seconds and the endpoint. A
    /// raise, so the call ends with an error row and `status(failed, reason='error')`.
    #[must_use]
    pub fn error(&self, name: &str, endpoint: &str) -> ProviderError {
        ProviderError::transport(format!(
            "{name}: stream_stalled — {endpoint} sent no bytes for {}s; this turn is NOT \
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
            (400, ErrorClass::Turn),
            (422, ErrorClass::Turn),
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

    /// Whether this provider can run a pass at all — checked FIRST in a turn
    /// ([`crate::agent_loop::run_generation`]), before the owed recall, the
    /// assembly and the projection. `Err` fails the turn with that refusal,
    /// exactly as a failed pass does. Python `_run` builds its provider before
    /// anything else and a raise there ends the turn; a provider that knows up
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
}

#[cfg(test)]
#[path = "tests_provider_clock.rs"]
mod tests_provider_clock;
