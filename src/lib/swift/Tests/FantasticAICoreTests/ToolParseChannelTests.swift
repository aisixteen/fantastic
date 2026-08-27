// The special-token channel envelope: `<|tool_call>call:<name>{…}<tool_call|>`.
//
// Mirrors the canonical Python suite `ai_core/tests/test_tool_parse.py` (its
// channel sections). Every fixture here was run through the python
// implementation and its expectations copied from what python actually
// produced — including object KEY ORDER, which is why the assertions compare
// serialized bytes.
//
// The fixtures that carry a DATE are verbatim model output from a run that
// died on that shape. They are the reason this grammar exists, so they are
// pinned rather than paraphrased.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

@Suite("tool-call parser: the special-token channel envelope")
struct ToolParseChannelTests {

    @Test func theVerbatimCallThatDiedAtPassOne() {
        // VERBATIM from the 2026-08-04 run that died at pass 1: gemma-4-12B-it
        // planned correctly, picked the right agent and verb, and wrapped it in
        // its OWN template's shape. `<|tool_call>` is not `<tool_call>` — the pipe
        // breaks the match — so zero calls were extracted and the chain ended with
        // the model never getting a reply.
        let calls = extractToolCalls(
            "Let's start by listing the agents."
                + "<|tool_call>call:kernel_state.list_agents{}<tool_call|>")
        #expect(calls.count == 1)
        #expect(calls[0].name == "send")
        #expect(
            calls[0].arguments.serialize()
                == #"{"target_id":"kernel_state","payload":{"type":"list_agents"}}"#)
    }

    @Test func stringsUseTheirOwnTokenAndMayContainCommas() {
        // The documented shape (ai.google.dev). Strings wear `<|"|>`, NOT `"` —
        // and a comma inside one must not split the argument list, which is
        // exactly what a `split(",")` or a JSON parse would get wrong.
        let calls = extractToolCalls(
            #"<|tool_call>call:weather.get{location:<|"|>Tokyo, JP<|"|>}<tool_call|>"#)
        #expect(
            calls[0].arguments.serialize()
                == #"{"target_id":"weather","payload":{"type":"get","location":"Tokyo, JP"}}"#)
    }

    @Test func argsLandInThePayloadBesideType() {
        // `call:<target>.<verb>{…}` IS the universal envelope written differently:
        // the braces are payload fields alongside `type`, not a second argument
        // slot.
        let calls = extractToolCalls(
            #"<|tool_call>call:assets.write{path:<|"|>a.txt<|"|>,content:<|"|>x<|"|>}<tool_call|>"#)
        #expect(
            calls[0].arguments.serialize()
                == #"{"target_id":"assets","payload":{"type":"write","path":"a.txt","content":"x"}}"#
        )
    }

    @Test func bareScalarsAndNestedObjects() {
        // Numbers, booleans and nested objects are written BARE in this grammar.
        // Also covers the uncompressed shape — the model naming our taught `send`
        // tool directly. `3` must stay an integer and `0.5` a double, or the
        // payload the agent receives is not the one the model wrote.
        let calls = extractToolCalls(
            #"<|tool_call>call:send{target_id:<|"|>m<|"|>,"#
                + #"payload:{type:<|"|>write<|"|>,count:3,ratio:0.5,ok:true,note:null}}<tool_call|>"#
        )
        #expect(calls[0].name == "send")
        #expect(
            calls[0].arguments.serialize()
                == #"{"target_id":"m","payload":{"type":"write","count":3,"ratio":0.5,"ok":true,"note":null}}"#
        )
    }

    @Test func nestedArraysCarryEveryValueKind() {
        let calls = extractToolCalls(
            #"<|tool_call>call:a.b{list:[1,<|"|>two<|"|>,{k:3},[4]],empty:[]}<tool_call|>"#)
        #expect(
            calls[0].arguments["payload"].serialize()
                == #"{"type":"b","list":[1,"two",{"k":3},[4]],"empty":[]}"#)
    }

    @Test func aChannelEnvelopeWithJsonQuotedArgs() {
        // VERBATIM from the 88-pass run of 2026-08-04. The envelope is the channel
        // one, the arguments are JSON — models mix the two. Reading the value as a
        // bare token kept its quote characters, so `id` never arrived and all THREE
        // posture-changing calls came back `{"error": "update_agent: id required"}`.
        let calls = extractToolCalls(
            "<|tool_call>call:kernel_state.update_agent"
                + #"{"id":"assets","ingress_rule":"allow_all_ingress"}<tool_call|>"#)
        #expect(
            calls[0].arguments.serialize()
                == #"{"target_id":"kernel_state","payload":{"type":"update_agent","id":"assets","ingress_rule":"allow_all_ingress"}}"#
        )
    }

    @Test func argsCarryingTargetIdAreAPlainSend() {
        // VERBATIM from the same run. `target_id` in the args means the model wrote
        // canonical `send` arguments and the dotted prefix is a namespace it
        // invented, not a target. Reading the prefix as the target produced
        // `{"error": "kernel_state: unknown type 'send'"}` eight times.
        let calls = extractToolCalls(
            "<|tool_call>call:kernel_state.send"
                + #"{"payload":{"type":"reflect"},"target_id":"assets"}<tool_call|>"#)
        #expect(calls[0].name == "send")
        // The args pass through VERBATIM, key order included — nothing is rebuilt.
        #expect(
            calls[0].arguments.serialize()
                == #"{"payload":{"type":"reflect"},"target_id":"assets"}"#
        )
    }

    @Test func targetIdWithoutPayloadKeepsTheVerb() {
        // VERBATIM from the passing run of 2026-08-04.
        // `call:X.reflect{"target_id":"Y"}` means "reflect Y". An earlier revision
        // saw `target_id` and emitted a send with NO payload; routing dispatched
        // `None` and the handler raised `'NoneType' object has no attribute 'get'`
        // three times in one run.
        let calls = extractToolCalls(
            #"<|tool_call>call:kernel_state.reflect{"target_id":"transcripts"}<tool_call|>"#)
        #expect(
            calls[0].arguments.serialize()
                == #"{"target_id":"transcripts","payload":{"type":"reflect"}}"#)
    }

    @Test func neitherPayloadNorVerbIsNotACall() {
        // `call:send{"target_id":"x"}` names no verb anywhere, so there is no call
        // to make — and we do not invent one. (It IS reported: the body parsed as a
        // `call:` head, so the model is told rather than met with silence.)
        let text = #"<|tool_call>call:send{"target_id":"x"}<tool_call|>"#
        #expect(extractToolCalls(text).isEmpty)
        #expect(malformedCalls(text).count == 1)
    }

    @Test func malformedIsDroppedNotRepaired() {
        // A parser that guesses what the model meant is measuring its own guess.
        #expect(extractToolCalls("<|tool_call>call:a.b{unterminated<tool_call|>").isEmpty)
        #expect(extractToolCalls(#"<|tool_call>call:a.b{k:<|"|>no close}<tool_call|>"#).isEmpty)
        // No `call:` head at all: not an attempt, so not even a report — the model
        // was talking, not calling.
        #expect(extractToolCalls("<|tool_call>garbage<tool_call|>").isEmpty)
        #expect(malformedCalls("<|tool_call>garbage<tool_call|>").isEmpty)
    }

    @Test func anInlinePayloadWithTargetIdIsASend() {
        // Measured 2026-08-06: `call:kernel_state{type:'reflect',
        // target_id:'readings'}` means "reflect readings". The verb was previously
        // looked for ONLY in a dotted name, so this complete instruction was
        // discarded.
        let call = extractToolCalls(
            "<|tool_call>call:kernel_state{type: 'reflect', target_id: 'readings'}<tool_call|>")[0]
        #expect(
            call.arguments.serialize() == #"{"target_id":"readings","payload":{"type":"reflect"}}"#)
    }

    @Test func anUndottedNameWithAnInlineTypeTargetsTheName() {
        // `call:kernel_state{type:'reflect'}` is `send([Item('kernel_state', {…})])[0].result`. It
        // used to arrive with no `target_id` and earn an error for a clear request.
        let call = extractToolCalls("<|tool_call>call:kernel_state{type:'reflect'}<tool_call|>")[0]
        #expect(call.name == "send")
        #expect(
            call.arguments.serialize()
                == #"{"target_id":"kernel_state","payload":{"type":"reflect"}}"#)
    }

    @Test func replyToStaysRoutingAndNeverEntersThePayload() {
        let call = extractToolCalls(
            "<|tool_call>call:totals{type: 'set', key: 'k', reply_to: 'self'}<tool_call|>")[0]
        #expect(
            call.arguments.serialize()
                == #"{"target_id":"totals","payload":{"type":"set","key":"k"},"reply_to":"self"}"#)
    }

    @Test func anExplicitPayloadStillWinsOverAnInlineType() {
        // The 2026-08-04 shape must not regress: args carrying both `target_id` and
        // `payload` pass through verbatim, stray top-level `type` and all.
        let call = extractToolCalls(
            #"<|tool_call>call:kernel_state.send{"payload":{"type":"reflect"},"#
                + #""target_id":"kernel_state"}<tool_call|>"#)[0]
        #expect(call.arguments["target_id"].asString == "kernel_state")
        #expect(call.arguments["payload"].serialize() == #"{"type":"reflect"}"#)
    }

    @Test func anUndottedNameWithNoTypeKeepsTheNameAsTheTool() {
        // The documented google shape, which names a TOOL rather than an agent: no
        // `target_id`, no `type`, so there is nothing to translate and the call is
        // handed on as written. Guessing a target here is what the `no target_id`
        // error exists to teach instead.
        let call = extractToolCalls(
            #"<|tool_call>call:get_current_weather{location:<|"|>Tokyo, JP<|"|>}<tool_call|>"#)[0]
        #expect(call.name == "get_current_weather")
        #expect(call.arguments.serialize() == #"{"location":"Tokyo, JP"}"#)
    }
}

// Verbatim from gemma-4-12B, probe 05 re-run, 2026-08-06 — a completely correct
// intent that the parser threw away because the strings were single-quoted. It was
// the model's last act; it had nothing left to fix and the run stalled.
private let GEMMA_SINGLE_QUOTED =
    "<|tool_call>call:kernel_state{type: 'send', target_id: 'kernel', "
    + "payload: {type: 'reflect', readme: 'index'}, reply_to: 'self'}<tool_call|>"

@Suite("tool-call parser: channel strings, both quote styles")
struct ToolParseChannelStringTests {

    @Test func singleQuotedChannelArgsParse() {
        let calls = extractToolCalls(GEMMA_SINGLE_QUOTED)
        #expect(calls.count == 1, "a single-quoted body is a shape models really emit")
        let args = calls[0].arguments
        #expect(args["target_id"].asString == "kernel")
        #expect(args["payload"].serialize() == #"{"type":"reflect","readme":"index"}"#)
        #expect(args["reply_to"].asString == "self")
        #expect(malformedCalls(GEMMA_SINGLE_QUOTED).isEmpty)
    }

    @Test func bothQuoteStylesYieldTheSameCall() {
        // Which quote a model reaches for is a fact about its training, not about
        // our grammar, so it must not change the result.
        let double = GEMMA_SINGLE_QUOTED.replacingOccurrences(of: "'", with: "\"")
        #expect(
            extractToolCalls(double)[0].arguments
                == extractToolCalls(GEMMA_SINGLE_QUOTED)[0].arguments)
    }

    @Test func aMismatchedQuotePairIsAnErrorNotAGuess() {
        // Opened with one style and closed with the other: obviously a mistake, so
        // it must not parse — and must be REPORTED rather than dropped.
        let text = "<|tool_call>call:a.b{k: 'value\"}<tool_call|>"
        #expect(extractToolCalls(text).isEmpty)
        #expect(malformedCalls(text).count == 1)
    }

    @Test func quotedStringsMayContainStructuralCharacters() {
        // The bare-token reader stops at `,}]`; a quoted string must not.
        // (`call:a.b` is DOTTED, so `b` becomes the payload's verb — hence `type`.)
        let call = extractToolCalls("<|tool_call>call:a.b{k: 'a,b}c[d', j: 2}<tool_call|>")[0]
        #expect(call.arguments["payload"].serialize() == #"{"type":"b","k":"a,b}c[d","j":2}"#)
    }

    @Test func theOtherQuoteStyleSurvivesInsideAString() {
        let payload = extractToolCalls(
            "<|tool_call>call:a.b{k: 'it\"s', m: \"it's\"}<tool_call|>")[0].arguments["payload"]
        #expect(payload["k"].asString == "it\"s")
        #expect(payload["m"].asString == "it's")
    }

    @Test func escapesDecodeTheSameUnderEitherQuote() {
        // `\n` is a newline and `\\` a single backslash under both styles; each
        // style also escapes its OWN quote. One shared escape table, so a model
        // does not get a different string back for picking a different quote.
        let cases: [(String, String)] = [
            (#"{k: 'a\nb\\c\'d'}"#, "a\nb\\c'd"),
            (#"{k: "a\nb\\c\"d"}"#, "a\nb\\c\"d"),
        ]
        for (body, want) in cases {
            let call = extractToolCalls("<|tool_call>call:a.b\(body)<tool_call|>")[0]
            #expect(call.arguments["payload"]["k"].asString == want, "\(body)")
        }
    }

    @Test func aUnicodeEscapeDecodes() {
        let call = extractToolCalls(#"<|tool_call>call:a.b{k: '\u0041\u00e9'}<tool_call|>"#)[0]
        #expect(call.arguments["payload"]["k"].asString == "Aé")
    }

    @Test func anUnknownEscapeSurvivesVerbatim() {
        // A Windows path in a program body must not be silently mangled.
        let call = extractToolCalls(#"<|tool_call>call:a.b{k: 'C:\dir'}<tool_call|>"#)[0]
        #expect(call.arguments["payload"]["k"].asString == #"C:\dir"#)
    }

    @Test func aQuotedKeyInEitherStyleLosesItsQuotes() {
        // `"id"` as a literal key name is what killed three posture changes on
        // 2026-08-04; single quotes must not reintroduce it.
        for body in ["{'id': 'assets'}", #"{"id": "assets"}"#] {
            let call = extractToolCalls("<|tool_call>call:a.update\(body)<tool_call|>")[0]
            #expect(call.arguments["payload"]["id"].asString == "assets", "\(body)")
        }
    }
}
