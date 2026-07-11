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
// API. This backend runs ONE provider pass per `send` (no tool loop); a
// caller that wants tool-calling teaches its own convention via
// `system_prompt` and drives the loop itself, parsing the completion with
// the shared `extractToolCalls`. The shared machinery consumes all backends
// identically.
//
// IMPORTANT: this module is provider-agnostic and MUST NOT import
// any provider SDK. In particular it MUST NOT import FoundationModels
// — all Apple-FM gating (`#if canImport(FoundationModels)`,
// `#available`) lives ONLY in the FM adapter target.

import FantasticJSON
import Foundation

/// One event off a provider stream, in arrival order. Providers are pure
/// raw-text streamers, so this carries content tokens only — there is no
/// tool-call variant (this backend does not run a tool loop; a caller that
/// wants tool-calling parses the completed text with `extractToolCalls`).
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
    /// the `send` tool in the prompt and parses the call from this text
    /// stream. Transport / HTTP failures finish the stream with a thrown error.
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error>

    /// Cooperative stop hook. Called when an `interrupt` targets an
    /// in-flight stream. Providers that can proactively tear down an
    /// upstream connection do so here; pure-polling providers (FM,
    /// which polls the cancel epoch each snapshot) may no-op.
    func stop() async
}

extension AIProvider {
    public func stop() async {}
}
