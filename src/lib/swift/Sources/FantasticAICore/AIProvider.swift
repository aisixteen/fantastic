// AIProvider — the streaming-chat seam every Swift LLM backend
// implements.
//
// Mirrors Rust's `fantastic-ai-core::Provider` and the Python
// `ai_core` provider seam. A provider is a thin streaming adapter:
// it takes the assembled message history + the available tools and
// yields either content tokens or FINALIZED tool-calls. It holds no
// agent state — the history / lock / cancellation all live in the
// shared `AIBackend` machinery.
//
// A provider is a PURE RAW-TEXT streamer: it yields `.token`s only and
// does NOT do tool-calling — Fantastic NEVER uses a provider's native tool
// API. Tool-calling is RAW prompt-and-parse and it belongs to the SHARED
// BACKEND: `Assembly` teaches the `send` envelope in the system prompt, the
// model types the call as ordinary text, `ToolParse` reads it back out of the
// stream and `Routing` dispatches it. A provider learns nothing about any of
// that — there is no `tools=[]`, no `tool_choice`, no native `tool_use` block,
// and no `.toolCall` chunk to yield. The shared machinery consumes all backends
// identically.
//
// IMPORTANT: this module is provider-agnostic and MUST NOT import
// any provider SDK.

import FantasticJSON
import Foundation

/// One event off a provider stream, in arrival order. Providers are pure
/// raw-text streamers, so this carries content tokens only — there is
/// deliberately no tool-call variant: a call arrives as TEXT like everything
/// else, and `ToolParse` reads it out of the completion afterwards.
public enum AIChunk: Sendable {
    /// A content token — streamed to the caller live and accumulated.
    case token(String)
}

/// Streaming chat adapter for one upstream LLM. Built per `send` by the
/// backend (via `makeProvider`); the only per-backend seam the shared
/// machinery talks to.
public protocol AIProvider: Sendable {
    /// The upstream model id this provider talks to.
    var model: String { get }

    /// Stream a plain-text completion for `messages` (already assembled by
    /// the shared core: history snapshot + the new user turn). Returns a
    /// stream yielding `.token`s in order. NO tools — the base class teaches
    /// the `send` tool in the prompt, parses the call from this text stream and
    /// dispatches it. Transport / HTTP failures finish the stream with a thrown error.
    /// There is NO `stop()` hook: a generation IS a kernel task, so cancelling
    /// it (`ps` → `cancel`) drops this stream — the `AsyncThrowingStream`
    /// finishes and its `onTermination` cancels the upstream HTTP task, which is
    /// the only teardown a provider ever needed. Cancellation belongs to the one
    /// primitive, not to a per-provider seam.
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error>
}
