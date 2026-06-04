//! Provider — the streaming-chat seam every LLM backend implements.
//!
//! A provider is a thin streaming adapter: it takes OpenAI-style
//! messages + the universal `send` tool and yields either content
//! tokens or FINALIZED tool-calls. It holds no agent state — the
//! queue / lock / history all live in [`crate::state`] /
//! [`crate::agent_loop`].
//!
//! Each backend's `chat()` FINALIZES tool-calls before yielding: the
//! ollama provider yields one `ToolCall` per NDJSON chunk (arguments
//! already parsed); the NIM provider aggregates per-index SSE argument
//! fragments internally and yields one finalized `ToolCall` once the
//! stream ends. The shared loop consumes both identically.

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
    /// Stream a completion for `messages` (OpenAI-style) given the
    /// available `tools` (the universal `send` tool). Returns a stream
    /// that yields tokens + finalized tool-calls in order, or an error
    /// describing a transport/HTTP failure.
    async fn chat(&self, messages: &[Value], tools: &[Value]) -> Result<ProviderStream, String>;

    /// The upstream model id this provider talks to.
    fn model(&self) -> String;
}
