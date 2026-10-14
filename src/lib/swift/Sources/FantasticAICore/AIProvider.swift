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

    /// The same stream, with `maxTokens` bounding THIS call's output (a side
    /// call's budget — a fold summary); `nil` keeps the provider's own bound. A
    /// stream that stopped AT the bound sets `usage.lastStopReason =
    /// "max_tokens"`. Python `Provider.chat(messages, max_tokens=None)`.
    func chat(messages: [JSON], maxTokens: Int?) -> AsyncThrowingStream<AIChunk, Error>

    /// What the endpoint REPORTED about the last pass — the prompt size it
    /// counted (calibrates the chars/4 estimate) and why the stream ended. `nil`
    /// for a provider whose endpoint reports nothing. Python
    /// `Provider.last_prompt_tokens` / `last_stop_reason`.
    var usage: AIUsage? { get }
}

extension AIProvider {
    /// A provider with no output bound of its own answers the unbounded stream.
    public func chat(messages: [JSON], maxTokens: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages)
    }

    /// Reports nothing.
    public var usage: AIUsage? { nil }
}

/// The endpoint's own report of the last pass, set by the provider as its stream
/// runs. A reference, because a provider is a `Sendable` value built per send and
/// the core reads the report after the stream ends. Python sets attributes on the
/// provider object.
public final class AIUsage: @unchecked Sendable {
    private let lock = NSLock()
    private var _promptTokens: Int?
    private var _completionTokens: Int?
    private var _stopReason: String?

    public init() {}

    /// The endpoint's own count of the last prompt, or `nil`.
    public var lastPromptTokens: Int? {
        get { lock.withLock { _promptTokens } }
        set { lock.withLock { _promptTokens = newValue } }
    }

    /// The endpoint's count of the last pass's OUTPUT tokens, or `nil` (python
    /// `last_completion_tokens`).
    public var lastCompletionTokens: Int? {
        get { lock.withLock { _completionTokens } }
        set { lock.withLock { _completionTokens = newValue } }
    }

    /// Why the last stream ended — `"max_tokens"` when it stopped AT the output
    /// bound.
    public var lastStopReason: String? {
        get { lock.withLock { _stopReason } }
        set { lock.withLock { _stopReason = newValue } }
    }
}

/// The token figures a finished generation's `status` `done` carries (a chat
/// shows `42 tok/s · 3.1k / 64k ctx`): `prompt_tokens` and
/// `completion_tokens` as the provider REPORTED them — estimated (this layer's own
/// count) when it reported none, and then `prompt_tokens_estimated` /
/// `completion_tokens_estimated` says so — `gen_seconds` (the wall time from the
/// request to the stream's end, thinking included) and `context_window` (the window
/// in force). Python `usage_detail`.
public func usageDetail(
    _ usage: AIUsage?, _ modelView: [JSON], _ text: String, _ genSeconds: Double, _ window: Int64
) -> [(String, JSON)] {
    var out: [(String, JSON)] = []
    if let n = usage?.lastPromptTokens, n > 0 {
        out.append(("prompt_tokens", .integer(Int64(n))))
    } else {
        out.append(("prompt_tokens", .integer(AIContext.estimateTokens(modelView))))
        out.append(("prompt_tokens_estimated", .bool(true)))
    }
    if let n = usage?.lastCompletionTokens, n > 0 {
        out.append(("completion_tokens", .integer(Int64(n))))
    } else {
        // python `max(1, len(text) // 4) if text else 0` — code points
        let chars = text.unicodeScalars.count
        out.append(("completion_tokens", .integer(Int64(chars == 0 ? 0 : max(1, chars / 4)))))
        out.append(("completion_tokens_estimated", .bool(true)))
    }
    out.append(("gen_seconds", .double((genSeconds * 1000).rounded() / 1000)))
    out.append(("context_window", .integer(window)))
    return out
}
