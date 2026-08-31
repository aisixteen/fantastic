// RAW tool-call parser tests — the JSON envelope's body and the SCANNER that
// finds envelopes in a completion.
//
// Mirrors the canonical Python suite `ai_core/tests/test_tool_parse.py` (the
// `parse_one` + `extract_tool_calls` sections). Every fixture here was run
// against the python implementation and its expectations copied from what
// python actually produced — including object KEY ORDER, which is why a few
// assertions compare serialized bytes.
//
// The malformed-reporting half lives in `MalformedCallsTests.swift`; the other
// two envelopes' body grammars have their own suites,
// `ToolParseChannelTests.swift` and `ToolParseInvokeTests.swift`.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

@Suite("tool-call parser: the JSON envelope")
struct ToolParseTests {

    @Test func parseOneCanonical() {
        let c = parseOne(
            #"{"name":"send","arguments":{"target_id":"core","payload":{"type":"list_agents"}}}"#)!
        #expect(c.name == "send")
        #expect(c.arguments["target_id"].asString == "core")
        #expect(c.arguments["payload"]["type"].asString == "list_agents")
        // The id is minted here — no provider hands us one, the call is text.
        #expect(c.id.hasPrefix("call_"))
        #expect(c.id.count == "call_".count + 8)
    }

    @Test func parseOneFlattenedAndToolAlias() {
        // tiny models drift: `tool` alias for name, flattened args.
        let c = parseOne(#"{"tool":"send","target_id":"foo","payload":{"type":"reflect"}}"#)!
        #expect(c.name == "send")
        #expect(c.arguments.serialize() == #"{"target_id":"foo","payload":{"type":"reflect"}}"#)
    }

    @Test func parseOneStringifiedArguments() {
        let c = parseOne(#"{"name":"send","arguments":"{\"target_id\":\"x\"}"}"#)!
        #expect(c.arguments["target_id"].asString == "x")
    }

    // ── send_binary: the `bytes` field is a RAW program, never JSON-escaped ──

    /// A send_binary tool_call with `program` written RAW as the ABSOLUTE last
    /// field: metadata first, then `"bytes":"<program verbatim>"` with nothing but
    /// closing braces after it. Mirrors python's `_binary_text` fixture.
    private func binaryText(target: String, meta: String, program: String) -> String {
        let prefix = meta.isEmpty ? "" : meta + ","
        return "<tool_call>{\"name\":\"send_binary\",\"arguments\":{\"target_id\":\""
            + target + "\",\"reply_to\":\"self\",\"payload\":{" + prefix
            + "\"bytes\":\"" + program + "\"}}}</tool_call>"
    }

    @Test func sendBinaryProgramParsesVerbatim() {
        // real newlines + quotes + a lone backslash — the stuff that breaks a JSON
        // parser, lifted out raw and NOT unescaped.
        let prog = "import os\nx = \"a \\ b\"\nprint(\"IMPORT_OK\", os.listdir(\".\"))\n"
        let text = binaryText(
            target: "store",
            meta: "\"type\":\"write_stream\",\"path\":\"main.py\",\"truncate\":true",
            program: prog)
        let calls = extractToolCalls(text)
        #expect(calls.count == 1)
        #expect(calls.first?.name == "send_binary")
        let p = calls.first!.arguments["payload"]
        #expect(p["bytes"].asString == prog, "the program must be taken verbatim")
        #expect(p["path"].asString == "main.py")
        #expect(p["truncate"].asBool == true)
    }

    @Test func sendBinaryProgramWithJsonMetacharsSurvives() {
        let prog = "d = {\"k\": \"v\"}\nprint(f\"{d}\")\npath = \"C:\\\\tmp\"\n"
        let text = binaryText(
            target: "store", meta: "\"type\":\"write_stream\",\"path\":\"x.py\"",
            program: prog)
        let calls = extractToolCalls(text)
        #expect(calls.count == 1)
        #expect(calls.first?.arguments["payload"]["bytes"].asString == prog)
    }

    @Test func nonSendBinaryIsUntouched() {
        let plain =
            "<tool_call>{\"name\":\"send\",\"arguments\":{\"target_id\":\"log\",\"payload\":{\"type\":\"note\",\"text\":\"hi\"}}}</tool_call>"
        let calls = extractToolCalls(plain)
        #expect(calls.count == 1 && calls.first?.name == "send")
        #expect(calls.first?.arguments["payload"]["text"].asString == "hi")
    }

    @Test func parseOneMalformedReturnsNil() {
        #expect(parseOne("{not json") == nil)
        #expect(parseOne("[1,2,3]") == nil)
    }

    @Test func strayTopLevelKeysBelongInsideArguments() {
        // Models routinely put `reply_to` one level too high. Discarding it —
        // which both non-python runtimes did — silently turned a chaining call
        // into a fire-and-forget one.
        let c = parseOne(
            #"{"name":"send","reply_to":"self","arguments":{"target_id":"m","payload":{"type":"reflect"}}}"#
        )!
        #expect(
            c.arguments.serialize()
                == #"{"reply_to":"self","target_id":"m","payload":{"type":"reflect"}}"#)
    }

    @Test func argumentsWinOverAStrayOfTheSameName() {
        let c = parseOne(#"{"name":"send","target_id":"stray","arguments":{"target_id":"real"}}"#)!
        #expect(c.arguments.serialize() == #"{"target_id":"real"}"#)
    }

    @Test func trailingJunkAfterACompleteObjectIsRecovered() {
        // Observed live from gpt-oss:120b: it closes `arguments` early and then
        // emits `,"reply_to":"self"}}` — one brace too many. The object that IS
        // there parses, and the stray tail is read for the k/v pairs it held
        // instead of being thrown away, because the model's real intent is in it.
        let c = parseOne(
            #"{"name":"send","arguments":{"target_id":"m","payload":{"type":"reflect"}}},"reply_to":"self"}}"#
        )!
        #expect(
            c.arguments.serialize()
                == #"{"reply_to":"self","target_id":"m","payload":{"type":"reflect"}}"#)
    }

    @Test func theVerbInTheToolNameBecomesThePayload() {
        // VERBATIM from gemma-4-12B, probe 05 run 3, 2026-08-06. `send` is the
        // only tool, so any other `name` is verb+tool compressed. Untranslated it
        // dispatched `send([Item("clock", None)])[0].result` and raised `'NoneType' object has no
        // attribute 'get'`.
        let c = extractToolCalls(
            #"<tool_call>{"name":"reflect","arguments":{"target_id":"clock"}}</tool_call>"#)[0]
        #expect(c.name == "send")
        #expect(c.arguments.serialize() == #"{"target_id":"clock","payload":{"type":"reflect"}}"#)
    }

    @Test func aNamedVerbKeepsItsOtherArgumentsInThePayload() {
        let c = extractToolCalls(
            #"<tool_call>{"name":"write","arguments":{"target_id":"a","path":"f.txt","reply_to":"self"}}</tool_call>"#
        )[0]
        // `reply_to` is ROUTING, so it stays at the top level, never in the payload.
        #expect(
            c.arguments.serialize()
                == #"{"target_id":"a","payload":{"type":"write","path":"f.txt"},"reply_to":"self"}"#
        )
    }

    @Test func anExplicitPayloadBeatsTheToolName() {
        // THE ARGS DECIDE: a model that already wrote a payload gets it through
        // verbatim, name and all.
        let c = extractToolCalls(
            #"<tool_call>{"name":"reflect","arguments":{"target_id":"a","payload":{"type":"list"}}}</tool_call>"#
        )[0]
        #expect(c.name == "reflect")
        #expect(c.arguments["payload"].serialize() == #"{"type":"list"}"#)
    }

    @Test func aNamedVerbWithoutATargetIsLeftAlone() {
        // `{"name":"list_agents","arguments":{}}` names no agent. We do not guess
        // one — it earns the `no target_id` error, which teaches.
        let c = extractToolCalls(
            #"<tool_call>{"name":"list_agents","arguments":{}}</tool_call>"#
        )[0]
        #expect(c.name == "list_agents")
        #expect(c.arguments.asObject?["target_id"] == nil)
    }
}

@Suite("tool-call parser: the scanner")
struct ToolScanTests {

    @Test func extractSingleCallFromProse() {
        let text =
            "ok "
            + #"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"reflect"}}}</tool_call>"#
        let calls = extractToolCalls(text)
        #expect(calls.count == 1)
        #expect(calls[0].name == "send")
        #expect(calls[0].arguments["target_id"].asString == "core")
    }

    @Test func extractMultipleCallsInDocumentOrder() {
        let a =
            #"<tool_call>{"name":"send","arguments":{"target_id":"a","payload":{}}}</tool_call>"#
        let b =
            #"<tool_call>{"name":"send","arguments":{"target_id":"b","payload":{}}}</tool_call>"#
        let calls = extractToolCalls(a + "\n" + b)
        #expect(calls.map { $0.arguments["target_id"].asString } == ["a", "b"])
    }

    @Test func extractSkipsABodyThatDidNotParse() {
        // Malformed JSON inside a closed tag is not a call (it IS reported —
        // see MalformedCallsTests — but it never dispatches).
        #expect(extractToolCalls("<tool_call>{nope}</tool_call>").isEmpty)
        // An incomplete object is not a call either, closer or no closer.
        #expect(extractToolCalls(#"<tool_call>{"name":"send""#).isEmpty)
        // Plain prose has no calls.
        #expect(extractToolCalls("a < b and c").isEmpty)
    }

    @Test func anUnterminatedButCompleteCallIsAccepted() {
        // REWRITTEN, not deleted: this test used to assert that an unterminated
        // tag "yields nothing (no close to bound the JSON)" — which pinned the
        // bug. `nvidia/nemotron-3-ultra-550b` emits a complete, valid call object
        // and simply stops without the closing tag; the envelope is the LAST thing
        // in the completion, so there is nothing after it to misread, and dropping
        // it loses an unambiguous instruction. SHAPE decides, not the closer.
        let calls = extractToolCalls(
            #"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}"#)
        #expect(calls.count == 1)
        #expect(calls[0].arguments["target_id"].asString == "m")
    }

    @Test func aStrayOpenerInProseDoesNotEatTheRealCall() {
        // THE ACTIVE BUG this port fixes. A stray `<tool_call>` in a sentence used
        // to pair with the closing tag of the GENUINE call further down, so the
        // scanner consumed both and the real instruction was never seen — silent,
        // and indistinguishable from the model saying nothing. A mention is
        // stepped over by the OPENER ONLY.
        let text =
            "My previous call was: `<tool_call>` and it failed. Retrying now."
            + #"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}</tool_call>"#
        #expect(extractToolCalls(text).map { $0.arguments["target_id"].asString } == ["m"])
    }

    @Test func threeEnvelopesInterleaveInDocumentOrder() {
        // All three shapes coexist in ONE completion; `<|tool_call>` must not shadow
        // or be shadowed by the taught `<tool_call>` envelope (its pipe means
        // neither tag is a substring of the other, which is exactly why the channel
        // envelope went unseen for so long). Document order is the dispatch order.
        let text =
            #"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}</tool_call>"#
            + "<|tool_call>call:n.two{}<tool_call|>"
            + #"<invoke name="send"><parameter name="arguments">"#
            + #"{"target_id":"o","payload":{"type":"three"}}</parameter></invoke>"#
        let calls = extractToolCalls(text)
        #expect(calls.map { $0.name } == ["send", "send", "send"])
        #expect(calls.map { $0.arguments["payload"]["type"].asString } == ["one", "two", "three"])
        #expect(calls.map { $0.arguments["target_id"].asString } == ["m", "n", "o"])
    }

    @Test func theReactionReaderSeesShapesItUsedToDrop() {
        // `AIBackend.deriveReaction` reads `target_id` + `payload.type` off every
        // call in an assistant turn to decide whether the model reacted to a
        // compaction notice. It is unchanged; the parser under it is not. Neither
        // shape below produced a visible reaction before this port — the
        // unterminated one yielded no call at all, and the verb-named one yielded a
        // call with no `payload` for the reader to look inside — so a model that DID
        // react was recorded as not having reacted. This pins that they now arrive,
        // and arrive carrying exactly the two fields that read-model reads.
        let unterminated =
            #"<tool_call>{"name":"send","arguments":{"target_id":"me","payload":{"type":"recall"}}}"#
        let verbNamed =
            #"<tool_call>{"name":"set","arguments":{"target_id":"mem","key":"k"}}</tool_call>"#
        for text in [unterminated, verbNamed] {
            let call = extractToolCalls(text)[0]
            #expect(call.arguments["target_id"].asString != nil, "\(text)")
            #expect(call.arguments["payload"]["type"].asString != nil, "\(text)")
        }
        #expect(extractToolCalls(unterminated)[0].arguments["payload"]["type"].asString == "recall")
        #expect(extractToolCalls(verbNamed)[0].arguments["payload"]["type"].asString == "set")
    }
}
