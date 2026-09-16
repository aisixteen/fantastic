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

/// Why a provider pass failed — and, with it, what the CALLER reads.
///
/// Python's backends install an optional error mapper (`ai_core.build`'s
/// `error_mapper`): nvidia turns an `httpx.HTTPStatusError` into a clean
/// caller-facing sentence, and `send` RETURNS `{error, client_id}`. Every other
/// failure propagates out of the handler, which the kernel settles as
/// `{error: "<Type>: <msg>", raised: true}` — a FAILED execution, not a
/// completed one carrying an error. Rust mints the mapped sentence at the site
/// python's mapper would recognise, so the kind travels as data instead of
/// being read back off the message text.
#[derive(Clone, Debug)]
pub struct ProviderError {
    /// What `status(done, reason='error')` carries — python `str(e)`.
    pub message: String,
    /// The backend MAPS this failure (python's `error_mapper` returning a
    /// string): the caller reads `{error, client_id}` and the handler does not
    /// fail. `false` is python's propagate-and-raise.
    pub mapped: bool,
}

impl ProviderError {
    /// A failure python RAISES: the handler fails and the execution is FAILED.
    pub fn raised(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            mapped: false,
        }
    }

    /// A failure the backend MAPS to a caller-facing sentence (nvidia's 429 /
    /// HTTP status): the turn settles as a reply, not as a raise.
    pub fn mapped(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            mapped: true,
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

/// A boxed async stream of provider events. `'static` so it can be
/// driven inside a spawned task.
pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, ProviderError>>;

/// Streaming chat adapter for one upstream LLM. Built per agent by the
/// backend; the only per-backend seam the shared machinery talks to.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stream a plain-text completion for `messages` (role + text content).
    /// Yields [`ProviderEvent::Token`] content tokens in order, or an error
    /// describing a transport/HTTP failure. NO tools, NO state.
    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, ProviderError>;

    /// The upstream model id this provider talks to.
    fn model(&self) -> String;
}
