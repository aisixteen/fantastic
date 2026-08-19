// Thinking: PARSED, not PERSISTED — the reasoning-channel convention.
//
// Ported from the canonical Python `ai_core.core` (`THINK_OPEN` /
// `THINK_CLOSE` / `without_thinking`, core.py:477-508). Its OWN file because
// the provider TARGETS (FantasticOllamaBackend, FantasticNvidiaNimBackend)
// import the markers — they are the only symbols here that are `public`,
// alongside the `ThinkingWrap` folder that keeps the wrapping correct.
//
// A reasoning model streams two things. ollama labels them `message.thinking` /
// `message.content`; NIM labels them `reasoning_content` / `content`. We must
// read BOTH — a turn spent entirely reasoning is otherwise an empty completion
// that silently ends the chain (gemma4:12b, 2026-08-02). But we must not KEEP
// both: feeding a reasoning model its own prior deliberation back is against
// every vendor's guidance, and it compounds — gemma's 3,800-char turn-1
// deliberation is re-sent in every later prompt, crowding out the task.
//
// So the two uses split. Providers wrap reasoning in these markers; the FULL
// text is parsed (a tool call the model wrote mid-thought still dispatches) and
// the STRIPPED text is persisted and shown as the answer. One convention, no
// provider learns anything about the tool protocol.
//
// SWIFT-SPECIFIC: the `done` event's `accumulated` (and the `status` verb's
// `text_so_far`) stay RAW — with the markers in. That field is contractually the
// concatenation of the `token` deltas the client already received, asserted
// across AIBackendTests and consumed by TUI/CLI/web; stripping it there would
// make two truths out of one stream. Only the DURABLE assistant turn is
// stripped (see `AIBackend.runStream`).

import Foundation

/// Opening marker a provider wraps its reasoning channel in.
public let THINK_OPEN = "<think>"
/// Closing marker a provider wraps its reasoning channel in.
public let THINK_CLOSE = "</think>"

/// Drop every `<think>…</think>` span — what goes into durable history.
///
/// Mirrors Python `without_thinking` exactly, including the unterminated case:
/// a stream cut mid-thought drops its tail rather than persisting a half-thought
/// (and rather than leaking an unbalanced marker into the conversation).
func withoutThinking(_ text: String) -> String {
    var out = ""
    var i = text.startIndex
    while true {
        guard let open = text.range(of: THINK_OPEN, range: i..<text.endIndex) else {
            out += text[i...]
            break
        }
        out += text[i..<open.lowerBound]
        guard let close = text.range(of: THINK_CLOSE, range: open.lowerBound..<text.endIndex)
        else {
            break  // unterminated (stream cut mid-thought) — drop the tail
        }
        i = close.upperBound
    }
    return out.trimmingCharacters(in: .whitespacesAndNewlines)
}

/// Folds one turn's TWO channels (reasoning + content) into the ONE wrapped
/// text stream ai-core parses, opening and closing the markers **once per
/// turn** — not per chunk, or the text would be a confetti of tags.
///
/// Python keeps this as a bare `thinking` bool inside each provider's async
/// generator. Swift's providers decode a line at a time through a PURE
/// function (so the wire is unit-testable at all), so the bool has to be an
/// explicit, shared piece of state — one implementation, two providers, and the
/// once-per-turn invariant is testable on its own.
public struct ThinkingWrap: Sendable {
    private var open = false

    public init() {}

    /// Text from the reasoning channel. Emits `THINK_OPEN` before the first
    /// piece; subsequent pieces ride bare.
    public mutating func reasoning(_ text: String) -> [String] {
        if text.isEmpty { return [] }
        if open { return [text] }
        open = true
        return [THINK_OPEN, text]
    }

    /// Text from the content channel. Closes an open reasoning span first —
    /// content starting IS the end of the thought.
    public mutating func content(_ text: String) -> [String] {
        if text.isEmpty { return [] }
        if !open { return [text] }
        open = false
        return [THINK_CLOSE, text]
    }

    /// The stream ended. Closes a span the model never closed itself (it spent
    /// the whole turn reasoning) so the text stays balanced for the parser.
    public mutating func finish() -> [String] {
        if !open { return [] }
        open = false
        return [THINK_CLOSE]
    }
}
