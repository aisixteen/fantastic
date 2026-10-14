// A broken envelope must have a VOICE. Mirrors the `malformed_calls` half of
// `bundled_agents/ai/ai_core/tests/test_tool_parse.py` and rust's
// `tool_parse/tests_malformed.rs`.
//
// Why the module exists at all: a call that cannot be read returns `[]` from
// `extractToolCalls`, which is byte-identical to a turn that emitted no call —
// and that means "done": nothing parsed,
// nothing dispatched, no error, and the run ends looking finished.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

@Suite("tool-call parser: malformed reporting")
struct MalformedCallsTests {

    /// The advice is a PROMPT, not a diagnostic: a model reads it and retries
    /// from it, and every runtime must say the same words. Pinned as LITERAL
    /// TEXT so "tidying" a sentence shows up as a failing test rather than as
    /// silent cross-runtime drift. Byte-for-byte python's `_HINT`.
    @Test func theHintIsVerbatim() {
        #expect(
            HINT == "The shape: <call name=\"send\">\n"
                + "<target_id>agent id</target_id>\n"
                + "<type>verb</type>\n"
                + "<path>a.md</path>\n"
                + "<bytes>document, verbatim, to the first </bytes>\n"
                + "</bytes>\n"
                + "</call> — one child tag per argument, a list is one tag whose children "
                + "are all <item>, nothing is escaped.")
    }

    /// The three fields are python's dict, and the composed sentence is
    /// `Malformed.swift`'s: `error` is the whole reply body so the wording has
    /// ONE owner.
    @Test func theReportIsThreeFieldsAndOneComposedSentence() {
        let text =
            #"<call name="send"><target_id>n</target_id><path>a</path><path>b</path></call>"#
        let bad = malformedCalls(text)
        #expect(bad.count == 1)
        #expect(bad.first?.envelope == "<call")
        #expect(
            bad.first?.error == "call NOT dispatched: duplicate `<path>` — a list is one tag "
                + "whose children are all `<item>`: `<path><item>…</item><item>…</item></path>`. "
                + "Nothing ran. \(HINT)")
        // The block is what sits BETWEEN the opener and the closer, whole and
        // defanged — neither delimiter is part of it.
        #expect(
            bad.first?.yourBlock
                == #"name="send"><target_id>n</target_id><path>a</path><path>b</path>"#)
    }

    /// PLAIN TEXT, not JSON — the model↔harness path carries none in either
    /// direction, and a JSON-dumped refusal shows doubled backslashes.
    @Test func theBodyDeliveredToTheModelIsPlainText() {
        let bad = malformedCalls(#"<call name="send"><type>reflect</type></call>"#)
        let body = bad[0].body
        #expect(body.hasPrefix("call NOT dispatched: a send names its target"))
        #expect(body.contains("\n\nyour block:\n"))
        #expect(!body.hasPrefix("{"))
    }

    /// Two broken envelopes are two reports, in document order — a batch of
    /// calls is answered as a batch, and a model that broke two must see both.
    @Test func everyBrokenEnvelopeGetsItsOwnReport() {
        let text =
            #"<call name="send"><type>reflect</type></call>"# + "\n"
            + #"<call name="nope"><target_id>n</target_id></call>"#
        let bad = malformedCalls(text)
        #expect(bad.count == 2)
        #expect(bad.first?.error.contains("a send names its target") == true)
        #expect(bad.last?.error.contains("`nope` is not a tool") == true)
    }

    /// A turn that parsed is silent here: nothing to report, nothing to nudge.
    @Test func aGoodTurnReportsNothing() {
        let text = #"<call name="send"><target_id>n</target_id><type>reflect</type></call>"#
        #expect(extractToolCalls(text).count == 1)
        #expect(malformedCalls(text).isEmpty)
    }

    /// Emitting no call is a legitimate way to stop. Only an envelope that
    /// OPENED and failed is a fault.
    @Test func proseWithNoEnvelopeIsSilenceNotAnError() {
        #expect(malformedCalls("I have finished wiring the aggregator.").isEmpty)
        #expect(malformedCalls("").isEmpty)
    }

    /// Whatever we echo becomes text the model quotes back. A live `<call` in it
    /// is indistinguishable from an emitted one, so the FOUR envelope delimiters
    /// (`<call`, `</call>`, `<bytes>`, `</bytes>`) are broken and the echoed
    /// block cannot re-arm the scanner. Only those four — the argument tags stay
    /// verbatim, because a model that cannot read its own block back cannot fix
    /// it.
    @Test func theEchoedBlockCannotReArmTheScanner() {
        let bad = malformedCalls(
            #"<call name="send"><target_id>notes</target_id><type>write</type>"#
                + "<bytes>x</bytes><bytes>y</bytes></call>")[0]
        #expect(bad.yourBlock.contains("target_id"), "the text must stay legible")
        #expect(!bad.yourBlock.contains("<call"), "the opener must not survive verbatim")
        #expect(!bad.yourBlock.contains("<bytes>"), "nor the verbatim opener")
        #expect(malformedCalls(bad.yourBlock).isEmpty, "echoing it must not produce a new report")
    }
}
