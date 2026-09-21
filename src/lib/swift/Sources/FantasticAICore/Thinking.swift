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
// The `token` events carry the RAW text — markers in — because they are exactly
// what the model streamed; only the DURABLE assistant turn is stripped (see
// `AIBackend.runStream`).

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
            // Unterminated (stream cut mid-thought) — drop the tail, but keep
            // any envelope it already contained: it was dispatched.
            out += keptEnvelopes(String(text[open.lowerBound...]))
            break
        }
        // KEEP THE ENVELOPES, DROP THE REASONING. A `<call name=…>` is an
        // INVOCATION wherever it is written, thinking included — it is parsed
        // out of the full completion and dispatched. Stripping the whole span
        // meant dispatch read the FULL text while history read the STRIPPED
        // text, so a call made inside a thought RAN, returned a reply, and left
        // no record. The model then saw an effect with no cause ("I sent a
        // reflect to file_bridge but the reply came back from exec_bridge" —
        // correct, given a record we had silently edited).
        out += keptEnvelopes(String(text[open.lowerBound..<close.lowerBound]))
        i = close.upperBound
    }
    return out.trimmingCharacters(in: .whitespacesAndNewlines)
}

/// Every complete tool-call envelope inside `span`, in order, one per line.
///
/// The ACTION taken survives a dropped thought; the reasoning does not. An
/// envelope that opened and never closed is NOT kept, and neither is one that is
/// complete but does not parse: dispatch could not have run it, so it was
/// reasoning too. `callSpans` is the one place that judgement lives.
private func keptEnvelopes(_ span: String) -> String {
    let found = callSpans(span)
    if found.isEmpty { return "" }
    return "\n" + found.joined(separator: "\n") + "\n"
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
