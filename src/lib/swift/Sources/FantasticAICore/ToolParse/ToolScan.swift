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
// No other envelope is accepted — no `<tool_call>{…}</tool_call>`, no
// `<invoke>`, no special-token channel: JSON fails on shapes that carry a
// document, bare child tags do not.
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
    /// The envelope reached its `</call>`.
    let terminated: Bool
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
            out.append(ScanHit(block: block, call: call, fault: nil, terminated: env.end != nil))
        } catch let m as Malformed {
            out.append(ScanHit(block: block, call: nil, fault: m.message, terminated: env.end != nil))
        } catch {
            out.append(ScanHit(block: block, call: nil, fault: "\(error)", terminated: env.end != nil))
        }
    }
    return out
}

/// Whether one envelope body (` name="…">…`, the text after `<call`) is a call
/// `extractToolCalls` would dispatch — the streaming splitter asks it at
/// `</call>`. Python `parses`.
func parses(_ block: String) -> Bool {
    let chars = Array(block.unicodeScalars)
    do {
        return try parseCall(chars, lo: 0, hi: chars.count) != nil
    } catch {
        // `parseCall` throws only `Malformed` — python's `except _Malformed`.
        return false
    }
}

/// Every dispatchable call with the BODY it was written as (the text after its
/// `name="…">`), in document order — what links a call to the stream part it was
/// previewed as. Python `terminated_calls`.
func terminatedCalls(_ text: String) -> [(call: ToolCall, body: String)] {
    var out: [(call: ToolCall, body: String)] = []
    for hit in scanToolEnvelopes(text) {
        guard let call = hit.call, hit.terminated else { continue }
        // `block[block.index(">") + 1 :]` — a parsed call's head always holds its `>`.
        let block = Array(hit.block.unicodeScalars)
        let gt = findScalars(block, [">"], from: 0)!
        out.append((call, scalarText(block, gt + 1, block.count)))
    }
    return out
}

/// Non-streaming: pull every finalized `<call>` out of a complete string. A call
/// with no `</call>` is NOT a call: the model stopped
/// generating before it closed, so nothing is dispatched — the turn's cut marker
/// names it as not sent (`cutMarker`).
func extractToolCalls(_ text: String) -> [ToolCall] {
    scanToolEnvelopes(text).compactMap { $0.terminated ? $0.call : nil }
}

