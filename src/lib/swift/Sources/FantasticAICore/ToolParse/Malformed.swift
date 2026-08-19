// Giving a BROKEN envelope a voice.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`_defang`,
// `_looks_like_an_attempt`, `malformed_calls`).

import FantasticJSON
import Foundation

/// Envelopes that opened and did not parse, as `{error, envelope, your_block}`.
///
/// Exists because a call that cannot be read is the one failure a model cannot
/// debug: `extractToolCalls` returns `[]` for it, which is byte-identical to a
/// turn that emitted no call at all — and that means "done". Measured on
/// gemma-4-12B (2026-08-06): its last pass left the `code` string unclosed,
/// nothing parsed, nothing dispatched, no error came back, and the run ended
/// looking finished with the task untouched. Five earlier `no target_id` errors
/// had TAUGHT it — it self-corrected out loud — so the fix is not a better
/// parser, it is giving this failure the same voice the others already have.
///
/// Returned as `JSON` because the routing caller delivers each entry to the model
/// verbatim as a serialized body; the key order is python's.
func malformedCalls(_ text: String) -> [JSON] {
    var out: [JSON] = []
    for hit in scanToolEnvelopes(text) {
        if hit.call != nil { continue }
        if !looksLikeAnAttempt(envelope: hit.envelope, block: hit.block) {
            // An opener whose body is not call-shaped is the model TALKING about
            // the envelope, not using it. Reporting those cost gemma-4-12B a turn
            // on 2026-08-06: it wrote the tag inside a sentence and got an error
            // about JSON.
            //
            // `hit.terminated` is NOT the test, though the first version of this
            // guard used it. A stray opener in prose plus the closing tag of a
            // REAL call further down the completion makes a "terminated" block
            // out of a sentence, so requiring the closer let prose through — and
            // then the loop closed: this error echoes `your_block`, the model
            // quotes the error while reasoning about it, its quotation contains
            // the tag, and the next scan finds a fresh opener. Three of run 4's
            // errors were that loop feeding itself, not the model. Shape decides,
            // always.
            continue
        }
        let body = hit.block.trimmingCharacters(in: .whitespacesAndNewlines)
        var reason = "the body did not parse"
        if hit.envelope.opener == TOOL_OPEN {
            do {
                _ = try JSON.parse(body)
            } catch {
                reason = "invalid JSON — \(describeParseFailure(error))"
            }
        }
        out.append(
            .object([
                // The whole reply body, composed HERE so the wording has one
                // owner. The routing caller delivers this verbatim; it does not
                // assemble a second version of the same sentence.
                "error": .string(
                    "tool_call NOT dispatched: \(reason). Nothing ran. " + hit.envelope.hint),
                "envelope": .string(hit.envelope.opener),
                // Enough to locate the break, bounded so a huge program body
                // cannot itself blow the context it is being explained in, and
                // DEFANGED so quoting this error cannot arm the scanner again.
                "your_block": .string(defang(String(body.prefix(400)))),
            ]))
    }
    return out
}

/// Did the model MEAN this opener as a call, or is the tag just a word in a
/// sentence?
///
/// Asked of EVERY unparsed block, terminated or not — see the comment in
/// `malformedCalls` for why the closing tag cannot be the test.
func looksLikeAnAttempt(envelope: Envelope, block: String) -> Bool {
    let body = block.trimmingCharacters(in: .whitespacesAndNewlines)
    if envelope.opener == CHANNEL_OPEN { return matchChannelHead(Array(body)) != nil }
    if envelope.opener == INVOKE_OPEN {
        return body.hasPrefix("name=") || body.contains("<parameter")
    }
    return body.hasPrefix("{")
}

/// Neutralize envelope delimiters so quoted text cannot re-arm the scanner.
///
/// Anything we echo back becomes conversation the model reads, quotes and reasons
/// about — and a quoted `<tool_call>` is indistinguishable from an emitted one.
/// Measured 2026-08-06: echoing a block verbatim made this error self-amplifying,
/// each report seeding the next. Breaking the opener is enough; the text stays
/// perfectly legible, which is the point of showing it at all.
func defang(_ text: String) -> String {
    var out = text
    for tag in [TOOL_OPEN, TOOL_CLOSE, CHANNEL_OPEN, CHANNEL_CLOSE, INVOKE_OPEN, INVOKE_CLOSE] {
        let broken = tag.replacingOccurrences(of: "<", with: "\u{2039}")
            .replacingOccurrences(of: ">", with: "\u{203A}")
        out = out.replacingOccurrences(of: tag, with: broken)
    }
    return out
}

/// The message inside a `JSON.SerializationError`, so the model is told WHICH
/// JSON fault broke its call (python passes `json.JSONDecodeError` through the
/// same way). `JSON.parse` throws only this error type; anything else is
/// described rather than mis-labelled as a parse fault.
private func describeParseFailure(_ error: Error) -> String {
    guard let e = error as? JSON.SerializationError else { return String(describing: error) }
    switch e {
    case .parseFailure(let m): return m
    case .unsupportedNumber(let m): return "unsupported number \(m)"
    case .unsupportedRoot(let m): return "unsupported root \(m)"
    }
}
