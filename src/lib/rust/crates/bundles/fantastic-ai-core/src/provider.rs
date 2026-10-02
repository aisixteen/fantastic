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
use serde_json::Value;

/// One event off a provider stream, in arrival order.
pub enum ProviderEvent {
    /// A content token (streamed to the caller live).
    Token(String),
}

/// Why a provider pass failed.
///
/// Every provider failure propagates out of the handler (python's `_send`
/// re-raises), which the kernel settles as `{error: "<Type>: <msg>",
/// raised: true}` — a FAILED execution, not a completed one carrying an error.
#[derive(Clone, Debug)]
pub struct ProviderError {
    /// What `status(done, reason='error')` carries — python `str(e)`.
    pub message: String,
    /// The python exception class a raise renders as (`RuntimeError`, or
    /// `ValueError` for a record field the backend cannot run without).
    pub kind: &'static str,
}

impl ProviderError {
    /// A failure python RAISES: the handler fails and the execution is FAILED.
    pub fn raised(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "RuntimeError",
        }
    }

    /// python's `ValueError` — a connector field the record MUST name (see
    /// [`crate::helpers::required_meta`]).
    pub fn value_error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "ValueError",
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

/// What the endpoint REPORTED about the last pass — python's provider attributes
/// `last_prompt_tokens` / `last_stop_reason`. Shared with the stream a provider
/// hands out, which fills it in as the endpoint reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderReport {
    /// The endpoint's own count of the last prompt (calibrates the estimate), or
    /// `None` when it reported none.
    pub prompt_tokens: Option<i64>,
    /// Why the last stream ended — `"max_tokens"` when it stopped AT the output
    /// bound (a side call's budget).
    pub stop_reason: Option<String>,
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
