// The scanner: every envelope in a completion, in DOCUMENT ORDER.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`_scan`,
// `extract_tool_calls`).
//
// SWIFT-SPECIFIC: the scan indexes over `Array(text)` (`[Character]`) rather
// than juggling `String.Index`, so its arithmetic matches python's string
// indices exactly — the step-over-the-OPENER-only rule below is where a
// hand-port with two index spaces silently drifts. Byte offsets exist in exactly
// one place in this module, inside `parseOne`'s `parsePrefix` call. The scanning
// primitives it shares with the body grammars live in `Scanning.swift`.

import FantasticJSON
import Foundation

/// One envelope the scanner found.
///
/// A `nil` `call` means an envelope WAS opened and its body did not parse. The
/// scanner keeps those instead of dropping them so a caller can tell the two
/// outcomes apart — see `malformedCalls`.
struct ScanHit {
    /// Which envelope opened (carries its closer, parser and hint).
    let envelope: Envelope
    /// The body between the delimiters, verbatim.
    let block: String
    /// The parsed call, or nil when the body did not parse.
    let call: ToolCall?
    /// Whether the closing tag was present — what separates a real attempt from
    /// a model merely MENTIONING the tag in prose.
    let terminated: Bool
}

/// Every envelope found, in document order.
///
/// Splitting the scan out of `extractToolCalls` keeps ONE implementation of the
/// envelope precedence rules; two copies would drift the moment a fourth
/// envelope arrives.
func scanToolEnvelopes(_ text: String) -> [ScanHit] {
    let chars = Array(text)
    var out: [ScanHit] = []
    var i = 0
    while i < chars.count {
        // Whichever envelope comes first, in document order. Channel before
        // invoke before taught on a tie (the table's order), preserving python's
        // precedence.
        var first: (pos: Int, envelope: Envelope)?
        for envelope in TOOL_ENVELOPES {
            guard let pos = findChars(chars, envelope.openerChars, from: i) else { continue }
            if first == nil || pos < first!.pos { first = (pos, envelope) }
        }
        guard let (pos, envelope) = first else { break }
        let bodyStart = pos + envelope.openerChars.count
        let end = findChars(chars, envelope.closerChars, from: bodyStart)
        // UNTERMINATED (`end == nil`) is not a truncation guess: the envelope is
        // the LAST thing in the completion, so there is nothing after it to
        // misread. Observed live from nvidia/nemotron-3-ultra-550b, which emits a
        // complete, valid call object and simply stops without the closing tag.
        // Dropping it loses an unambiguous instruction.
        let block = String(chars[bodyStart..<(end ?? chars.count)])
        let call = envelope.parse(block)
        if call == nil, !looksLikeAnAttempt(envelope: envelope, block: block) {
            // A MENTION, not a call — the model wrote the tag inside a sentence.
            // Step over the OPENER ONLY. Consuming through `closer` would swallow
            // whatever follows, and what follows is often the real call: a stray
            // opener in prose pairs with the closing tag of the genuine call
            // further down, and the genuine call is then never seen. That
            // silently cost real instructions long before malformed reporting
            // existed — and it was still costing them here until this port.
            i = bodyStart
            continue
        }
        out.append(ScanHit(envelope: envelope, block: block, call: call, terminated: end != nil))
        guard let end else { break }
        i = end + envelope.closerChars.count
    }
    return out
}

/// Non-streaming: pull every finalized call out of a complete string. Used by
/// `Routing.routeReply` on every completion (the dispatch path) and by the
/// durable-history reader (`AIBackend.deriveReaction`).
func extractToolCalls(_ text: String) -> [ToolCall] {
    scanToolEnvelopes(text).compactMap { $0.call }
}
