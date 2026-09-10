// A broken envelope must have a VOICE.
//
// Ported from the canonical Python `ai_core/tool_parse.py` (`malformed_calls`,
// `_defang`, `_HINT`).
//
// Exists because a call that cannot be read is the one failure a model cannot
// debug: `extractToolCalls` returns `[]` for it, which is byte-identical to a
// turn that emitted no call at all — and that means "done". Measured on
// gemma-4-12B (2026-08-06): nothing parsed, nothing dispatched, no error came
// back, and the run ended looking finished with the task untouched. Earlier
// errors HAD taught it — it self-corrected out loud — so the fix is not a better
// parser, it is giving this failure the same voice the others already have.

import FantasticJSON
import FantasticKernel
import Foundation

/// The shape appended to every refusal: literal, minimal, copyable. Advice, not
/// a repair — the block is never re-parsed leniently.
let HINT = """
    The shape: <call name="send">
    <target_id>agent id</target_id>
    <type>verb</type>
    <path>a.md</path>
    <bytes>document, verbatim, to the first </bytes>
    </bytes>
    </call> — one child tag per argument, a list is one tag whose children are all <item>, \
    nothing is escaped.
    """

/// One envelope that opened as a call and did not parse.
struct MalformedCall {
    /// The whole reply body, composed HERE so the wording has one owner.
    let error: String
    /// The envelope opener the model used — always `<call`.
    let envelope: String
    /// The block, WHOLE and defanged.
    let yourBlock: String

    /// What the model is told, as PLAIN TEXT — the model↔harness path carries no
    /// JSON in either direction.
    var body: String { "\(error)\n\nyour block:\n\(yourBlock)" }
}

/// Envelopes that opened as a call and did not parse.
func malformedCalls(_ text: String) -> [MalformedCall] {
    scanToolEnvelopes(text).compactMap { hit -> MalformedCall? in
        guard hit.call == nil else { return nil }
        return MalformedCall(
            error: "call NOT dispatched: \(hit.fault ?? ""). Nothing ran. \(HINT)",
            envelope: String(OPEN),
            // BOTH ENDS, never just the head — the fault in a long block is
            // almost never in the first 400 chars. Shown WHOLE, DEFANGED so
            // quoting it cannot arm the scanner again.
            yourBlock: defang(pyStrip(hit.block)))
    }
}

/// Neutralize envelope delimiters so quoted text cannot re-arm the scanner.
///
/// Anything echoed back becomes conversation the model reads, quotes and reasons
/// about — and a quoted `<call` is indistinguishable from an emitted one.
/// Measured 2026-08-06 (on the JSON envelope): echoing a block verbatim made the
/// error self-amplifying, each report seeding the next. Breaking the opener is
/// enough.
func defang(_ text: String) -> String {
    var out = text
    for tag in [OPEN, CLOSE, BYTES_OPEN, BYTES_CLOSE] {
        let s = String(tag)
        out = out.replacingOccurrences(
            of: s,
            with: s.replacingOccurrences(of: "<", with: "\u{2039}")
                .replacingOccurrences(of: ">", with: "\u{203A}"))
    }
    return out
}
