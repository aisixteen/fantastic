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

/// A boxed async stream of provider events. `'static` so it can be
/// driven inside a spawned task.
pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, String>>;

/// Streaming chat adapter for one upstream LLM. Built per agent by the
/// backend; the only per-backend seam the shared machinery talks to.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stream a plain-text completion for `messages` (role + text content).
    /// Yields [`ProviderEvent::Token`] content tokens in order, or an error
    /// describing a transport/HTTP failure. NO tools, NO state.
    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String>;

    /// The upstream model id this provider talks to.
    fn model(&self) -> String;
}
