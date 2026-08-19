// A broken envelope must have a VOICE.
//
// Mirrors the canonical Python suite `ai_core/tests/test_tool_parse.py`
// (`malformed_calls` section). Every fixture was run against the python
// implementation and the expectations copied from what it produced.
//
// ONE deliberate divergence: the JSON fault is named by whichever JSON parser
// the runtime has, so the reason text quotes swift's `JSONScanner`
// ("unterminated string at offset N") where python quotes CPython's
// ("Unterminated string starting at: line 1 column 162"). The HINT that follows
// it is byte-identical across runtimes, because that half is a prompt.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

// Verbatim from gemma-4-12B, probe 05, 2026-08-06 — the call that ended the run.
// The program body is escaped correctly; the `code` value is never closed, so
// every brace after it is string content and the object runs off the end.
private let GEMMA_UNCLOSED_CODE =
    #"{"name":"send","arguments":{"target_id":"kernel_state","payload":"#
    + #"{"type":"create_agent","handler_module":"python_runtime.tools","#
    + #""id":"reading_aggregator","code":"def update_totals():\n"#
    + #"    worker_res = send(\"reading_worker\", {\"type\": \"compute_total\"})\n"#
    + #"    return f\"updated to {worker_res}\"}}"#

@Suite("tool-call parser: malformed reporting")
struct MalformedCallsTests {

    @Test func theUnclosedCodeStringThatEndedTheGemmaRunIsReported() {
        // The regression this exists for. Silence here read as "the model is done"
        // while the task sat untouched, so the assertion is not that we PARSE it —
        // we must not — but that the model is TOLD.
        let text = "<tool_call>" + GEMMA_UNCLOSED_CODE
        #expect(extractToolCalls(text).isEmpty, "a broken call must never dispatch")
        let bad = malformedCalls(text)
        #expect(bad.count == 1)
        let error = bad[0]["error"].asString ?? ""
        #expect(error.contains("NOT dispatched"))
        #expect(error.contains("unterminated string"), "name the JSON fault")
        #expect(error.contains("CLOSED by one unescaped"), "name the fix")
        #expect(
            bad[0]["your_block"].asString?.contains("reading_aggregator") == true,
            "show its own text back")
        #expect(bad[0]["envelope"].asString == TOOL_OPEN)
    }

    @Test func proseWithNoEnvelopeIsSilenceNotAnError() {
        // The distinction the whole change rests on: emitting no call is a
        // legitimate way to stop. Only an envelope that OPENED and failed is a fault.
        #expect(malformedCalls("I have finished wiring the aggregator.").isEmpty)
        #expect(malformedCalls("").isEmpty)
    }

    @Test func aGoodCallBesideABrokenOneReportsOnlyTheBrokenOne() {
        // A partially-read turn dispatched half the model's intent. Which half was
        // lost is not inferable from the replies it got, so the loss is named.
        let text =
            #"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}</tool_call>"#
            + #"<tool_call>{"name":"send","arguments":{"target_id":"n","code":"oops}}"#
        #expect(extractToolCalls(text).map { $0.arguments["target_id"].asString } == ["m"])
        #expect(malformedCalls(text).count == 1)
    }

    @Test func everyEnvelopeGetsTheSameVoice() {
        // `<|tool_call>` was the envelope whose silent drop was documented and
        // unfixed; it must report like the others now. What is pinned here is
        // that opening any of the three and failing is never silence.
        for text in [
            "<|tool_call>call:a.b{unterminated<tool_call|>",
            #"<invoke name="send"><parameter name="arguments">{oops</parameter></invoke>"#,
        ] {
            #expect(extractToolCalls(text).isEmpty, "\(text)")
            let bad = malformedCalls(text)
            #expect(bad.count == 1, "\(text)")
            #expect(bad.first?["error"].asString?.contains("NOT dispatched") == true, "\(text)")
        }
    }

    @Test func theChannelErrorDoesNotLectureAboutJson() {
        // Its body is not JSON. Advice that does not match the grammar the model
        // used teaches the wrong lesson — measured live on 2026-08-06.
        let bad = malformedCalls("<|tool_call>call:a.b{k: 'unclosed}<tool_call|>")[0]
        let error = bad["error"].asString ?? ""
        #expect(error.contains("NOT JSON"))
        #expect(!error.contains(#"\n"#), "the JSON escaping hint must not appear here")
        #expect(bad["envelope"].asString == CHANNEL_OPEN)
    }

    @Test func proseMentioningTheTagIsNotReportedAsABrokenCall() {
        // Measured 2026-08-06: gemma wrote `<tool_call>` inside a sentence and the
        // first version of this feature answered with a lecture about JSON, costing
        // a turn. An opener with no call-shaped body is a MENTION.
        #expect(
            malformedCalls("You send it in a <tool_call> block. The `kernel_state` agent is root.")
                .isEmpty)
    }

    @Test func anUnterminatedButCallShapedBlockStillReports() {
        // The other side of that rule — nemotron-3-ultra-550b omits the closing tag
        // on real calls, so a `{`-shaped body is an attempt however it ends.
        #expect(malformedCalls(#"<tool_call>{"name":"send","arguments":{oops"#).count == 1)
    }

    @Test func proseIsNotAnAttemptEvenWhenALaterCloserTerminatesIt() {
        // A stray opener in a sentence, plus the closing tag of a REAL call further
        // down, forms a "terminated" block out of prose. Requiring the closer let
        // that through and CLOSED A LOOP: the error echoes the block, the model
        // quotes the error, the quotation contains the tag, the next scan finds a
        // fresh opener. Three of run 4's errors on 2026-08-06 were this feeding
        // itself. Shape decides, always.
        let text =
            "My previous call was: `<tool_call>` and it failed. Retrying now."
            + #"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}</tool_call>"#
        #expect(extractToolCalls(text).map { $0.arguments["target_id"].asString } == ["m"])
        #expect(malformedCalls(text).isEmpty, "the prose mention must not be reported")
    }

    @Test func theEchoedBlockCannotReArmTheScanner() {
        // Whatever we echo becomes text the model quotes back. A live `<tool_call>`
        // in it is indistinguishable from an emitted one, so the delimiters are
        // defanged.
        let bad = malformedCalls(#"<tool_call>{"name":"send","note":"see <tool_call> oops"#)[0]
        let echoed = bad["your_block"].asString ?? ""
        #expect(echoed.contains("oops"), "the text must stay legible")
        #expect(!echoed.contains(TOOL_OPEN), "the tag must not survive verbatim")
        #expect(malformedCalls(echoed).isEmpty, "echoing it must not produce a new report")
    }

    @Test func aHugeProgramBodyCannotBlowTheContextExplainingIt() {
        let text =
            #"<tool_call>{"name":"send","arguments":{"code":""#
            + String(repeating: "x", count: 40000)
        #expect((malformedCalls(text)[0]["your_block"].asString ?? "").count <= 400)
    }
}
