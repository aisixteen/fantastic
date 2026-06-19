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
// Each backend's `chat()` FINALIZES tool-calls before yielding: the
// ollama provider would yield one `.toolCall` per NDJSON chunk; the
// NIM provider aggregates per-index SSE argument fragments internally
// and yields finalized `.toolCall`s once the stream ends; the FM
// provider streams cumulative on-device snapshots as `.token`s. The
// shared loop consumes all three identically.
//
// IMPORTANT: this module is provider-agnostic and MUST NOT import
// any provider SDK. In particular it MUST NOT import FoundationModels
// — all Apple-FM gating (`#if canImport(FoundationModels)`,
// `#available`) lives ONLY in the FM adapter target.

import FantasticJSON
import Foundation

/// One event off a provider stream, in arrival order.
public enum AIChunk: Sendable {
    /// A content token — streamed to the caller live and accumulated.
    case token(String)
    /// One completed tool-call, already finalized (arguments parsed /
    /// fragments joined). The JSON is the OpenAI-style tool-call object
    /// `{id, type:"function", function:{name, arguments}}` so the
    /// shared machinery can splice it straight into the persisted
    /// assistant turn without re-shaping.
    case toolCall(JSON)
}

/// Streaming chat adapter for one upstream LLM. Built per `send` by the
/// backend (via `makeProvider`); the only per-backend seam the shared
/// machinery talks to.
public protocol AIProvider: Sendable {
    /// The upstream model id this provider talks to.
    var model: String { get }

    /// Stream a completion for `messages` (already assembled by the
    /// shared core: history snapshot + the new user turn) given the
    /// available `tools`. Returns a stream yielding tokens + finalized
    /// tool-calls in order. Transport / HTTP failures are surfaced by
    /// having the stream finish with a thrown error.
    func chat(messages: [JSON], tools: [JSON]) -> AsyncThrowingStream<AIChunk, Error>

    /// Cooperative stop hook. Called when an `interrupt` targets an
    /// in-flight stream. Providers that can proactively tear down an
    /// upstream connection do so here; pure-polling providers (FM,
    /// which polls the cancel epoch each snapshot) may no-op.
    func stop() async
}

extension AIProvider {
    public func stop() async {}
}
