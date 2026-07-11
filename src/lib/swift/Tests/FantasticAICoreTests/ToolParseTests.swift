// RAW tool-call parser tests — the shared `<tool_call>` envelope helpers a
// CALLER uses to read a completion (no native provider tool API, no streaming
// split: this backend runs one pass, the tool loop is the caller's). Mirrors
// the Rust/Python `parse_one` + `extract_tool_calls` units.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

@Suite("RAW tool-call parser")
struct ToolParseTests {
    @Test func parseOneCanonicalAndFlattenedAndStringified() {
        let (n, a) = parseOneToolCall(
            #"{"name":"send","arguments":{"target_id":"core","payload":{"type":"list_agents"}}}"#)!
        #expect(n == "send")
        #expect(a["target_id"].asString == "core")

        let (n2, a2) = parseOneToolCall(
            #"{"tool":"send","target_id":"foo","payload":{"type":"reflect"}}"#)!
        #expect(n2 == "send")
        #expect(a2["target_id"].asString == "foo")

        let (_, a3) = parseOneToolCall(#"{"name":"send","arguments":"{\"target_id\":\"x\"}"}"#)!
        #expect(a3["target_id"].asString == "x")

        #expect(parseOneToolCall("{not json") == nil)
        #expect(parseOneToolCall("[1,2,3]") == nil)
    }

    @Test func extractSingleCallFromProse() {
        let text =
            "ok "
            + #"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"reflect"}}}</tool_call>"#
        let calls = extractToolCalls(text)
        #expect(calls.count == 1)
        #expect(calls[0].0 == "send")
        #expect(calls[0].1["target_id"].asString == "core")
    }

    @Test func extractMultipleCalls() {
        let a =
            #"<tool_call>{"name":"send","arguments":{"target_id":"a","payload":{}}}</tool_call>"#
        let b =
            #"<tool_call>{"name":"send","arguments":{"target_id":"b","payload":{}}}</tool_call>"#
        let calls = extractToolCalls(a + "\n" + b)
        #expect(calls.count == 2)
        #expect(calls[0].1["target_id"].asString == "a")
        #expect(calls[1].1["target_id"].asString == "b")
    }

    @Test func extractSkipsMalformedAndUnterminated() {
        // Malformed JSON inside a closed tag is skipped (not a valid call).
        #expect(extractToolCalls("<tool_call>{nope}</tool_call>").isEmpty)
        // An unterminated tag yields nothing (no close to bound the JSON).
        #expect(extractToolCalls(#"<tool_call>{"name":"send""#).isEmpty)
        // Plain prose has no calls.
        #expect(extractToolCalls("a < b and c").isEmpty)
    }

    @Test func extractAndRenderRoundTrip() {
        let s = renderToolCall(
            name: "send", arguments: .object(["target_id": .string("core")]))
        let calls = extractToolCalls(s)
        #expect(calls.count == 1)
        #expect(calls[0].1["target_id"].asString == "core")
    }
}
