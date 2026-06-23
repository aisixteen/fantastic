//! Provider — the streaming-chat seam every LLM backend implements.
//!
//! A provider is a PURE RAW-TEXT streamer: it takes plain chat messages
//! (role system/user/assistant, text content) and yields content tokens
//! ([`ProviderEvent::Token`]) only. It does NOT do tool-calling — Fantastic
//! NEVER uses a provider's native tool API. Tool-calling is owned by the base
//! class ([`crate::tool_parse`] parses the `<tool_call>` envelope out of this
//! text stream). The provider holds no agent state — the queue / lock / history
//! all live in [`crate::state`] / [`crate::agent_loop`].
//!
//! `ProviderEvent::ToolCall` exists only as the parser's output type (and a
//! test pass-through); a real backend `chat()` yields only `Token`.

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde_json::Value;

/// One event off a provider stream, in arrival order.
pub enum ProviderEvent {
    /// A content token (streamed to the caller live).
    Token(String),
    /// One completed tool-call — `args` is already a parsed JSON object
    /// (even for wire formats that stream argument fragments).
    ToolCall {
        /// The provider-assigned (or minted) call id.
        id: String,
        /// The function name (always `send` in this substrate).
        name: String,
        /// The parsed arguments object (`{}` on parse failure).
        args: Value,
    },
}

/// A boxed async stream of provider events. `'static` so it can be
/// driven inside a spawned task.
pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, String>>;

/// Streaming chat adapter for one upstream LLM. Built per agent by the
/// backend; the only per-backend seam the shared machinery talks to.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stream a plain-text completion for `messages` (role
    /// system/user/assistant, text content). Yields `ProviderEvent::Token`
    /// content tokens in order, or an error describing a transport/HTTP
    /// failure. NO tools — the base class teaches the `send` tool in the
    /// prompt and parses the call from this text stream.
    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String>;

    /// The upstream model id this provider talks to.
    fn model(&self) -> String;
}
