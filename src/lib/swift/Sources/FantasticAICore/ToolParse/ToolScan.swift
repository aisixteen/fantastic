// The scanner: every envelope in a completion, in DOCUMENT ORDER.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`_scan`,
// `extract_tool_calls`, `call_spans`).
//
// There is NO agentic loop here and NO native provider tool API: the provider
// streams plain text, this module reads it. There is also NO JSON anywhere on
// this path, in either direction. ONE envelope, args inline:
//
//     <call name="send">
//     <target_id>notes</target_id>
//     <type>patch</type>
//     <bytes>…verbatim, to the first </bytes>…
//     </bytes>
//     </call>
//
// The three JSON-era envelopes this module used to accept — the taught
// `<tool_call>{…}</tool_call>`, Claude's `<invoke>`, the gemma special-token
// channel — and both `bytes` cuts are DELETED, not deprecated. A model's trained
// format stopped being the argument the day the measurement went the other way
// (2026-09-09, qwen3.8-27b: JSON 3/12 correct on shapes that carry a document,
// bare child tags 12/12).
//
// SWIFT-SPECIFIC: the scan indexes over `Array(text.unicodeScalars)` rather than
// juggling `String.Index`, so its arithmetic matches python's CODE-POINT string
// indices exactly — the step-over-the-OPENER-only rule is where a hand-port with
// two index spaces silently drifts, and a `[Character]` array is itself a second
// index space. The primitives live in `Scanning.swift`.

import FantasticJSON
import Foundation

/// One envelope the scanner found.
///
/// A `nil` `call` with a `fault` means an envelope WAS opened as a call and its
/// body did not parse — kept, so `malformedCalls` can say so. A `nil` call with
/// no fault is the model MENTIONING `<call` in prose: stepped over, so a stray
/// mention never swallows the real call after it.
struct ScanHit {
    /// The body between `<call` and `</call>`, verbatim.
    let block: String
    /// The parsed call, or nil when the body did not parse.
    let call: ToolCall?
    /// The named fault, when the body was call-shaped and did not parse.
    let fault: String?
}

/// Every envelope found, in document order.
func scanToolEnvelopes(_ text: String) -> [ScanHit] {
    let chars = Array(text.unicodeScalars)
    var out: [ScanHit] = []
    for env in envelopes(chars) {
        // UNTERMINATED (`end == nil`) is not a truncation guess: the envelope is
        // the LAST thing in the completion, so there is nothing after it to
        // misread.
        let hi = env.end ?? chars.count
        let block = scalarText(chars, env.body, hi)
        do {
            guard let call = try parseCall(chars, lo: env.body, hi: hi) else { continue }
            out.append(ScanHit(block: block, call: call, fault: nil))
        } catch let m as Malformed {
            out.append(ScanHit(block: block, call: nil, fault: m.message))
        } catch {
            out.append(ScanHit(block: block, call: nil, fault: "\(error)"))
        }
    }
    return out
}

/// Non-streaming: pull every finalized `<call>` out of a complete string.
func extractToolCalls(_ text: String) -> [ToolCall] {
    scanToolEnvelopes(text).compactMap(\.call)
}

/// Every COMPLETE call envelope in `text`, verbatim, in source order — what a
/// history writer keeps out of a dropped thought span (an action taken is not
/// reasoning). An unterminated opener is not kept: dispatch could not parse it,
/// and neither is a complete envelope that does not parse — a broken call is
/// reasoning too.
func callSpans(_ text: String) -> [String] {
    let chars = Array(text.unicodeScalars)
    var out: [String] = []
    for env in envelopes(chars) {
        guard let end = env.end else { break }
        if let call = try? parseCall(chars, lo: env.body, hi: end), call != nil {
            out.append(scalarText(chars, env.start, end + CLOSE.count))
        }
    }
    return out
}
