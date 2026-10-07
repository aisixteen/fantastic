// Thinking: neither DISPATCHED nor PERSISTED — only the answer acts.
//
// Ported from the canonical Python `ai_core.core` (`THINK_OPEN` /
// `THINK_CLOSE` / `without_thinking`, core.py:477-508). Its OWN file because
// the provider TARGETS (FantasticOllamaBackend, FantasticResponsesBackend,
// FantasticVllmBackend) import the markers — they are the only symbols here that
// are `public`, alongside the `ThinkingWrap` folder that keeps the wrapping correct.
//
// A reasoning model streams two things. ollama labels them `message.thinking` /
// `message.content`; vLLM labels them `reasoning_content` / `content`. We must
// read BOTH — a turn spent entirely reasoning is otherwise an empty completion
// that silently ends the chain (gemma4:12b, 2026-08-02). But we must not KEEP
// both: feeding a reasoning model its own prior deliberation back is against
// every vendor's guidance, and it compounds — gemma's 3,800-char turn-1
// deliberation is re-sent in every later prompt, crowding out the task.
//
// Providers wrap reasoning in these markers, and everything downstream reads the
// ANSWER — the text with every span dropped whole: dispatch parses it and history
// persists it. A `<call>` written inside thinking is thinking (owner, 2026-10-07):
// measured over 2,821 e2e turns, of 106 calls inside thinking 39 were copies of an
// answer call (the action ran twice) and 67 were drafts that ran although the model
// never wrote them in its answer. One convention, no provider learns anything about
// the tool protocol.
//
// The `token` events carry the RAW text — markers in — because they are exactly
// what the model streamed; only the DURABLE assistant turn is stripped (see
// `AIBackend.runStream`).

import Foundation

/// Opening marker a provider wraps its reasoning channel in.
public let THINK_OPEN = "<think>"
/// Closing marker a provider wraps its reasoning channel in.
public let THINK_CLOSE = "</think>"

/// The ANSWER: every `<think>…</think>` span dropped whole — what dispatch reads
/// and what goes into durable history. An UNTERMINATED span runs to the end: the
/// rest is thinking. Python `without_thinking`.
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
        else { break }
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
