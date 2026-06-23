// RAW tool-call parser tests — the shared `<tool_call>` envelope parsing
// (no native provider tool API). Mirrors the Rust/Python parser units.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

private func rawStream(_ chunks: [String]) -> AsyncThrowingStream<AIChunk, Error> {
    AsyncThrowingStream { continuation in
        for c in chunks { continuation.yield(.token(c)) }
        continuation.finish()
    }
}

private func drain(_ s: AsyncThrowingStream<AIChunk, Error>) async throws -> (String, [JSON]) {
    var content = ""
    var calls: [JSON] = []
    for try await ev in s {
        switch ev {
        case .token(let t): content += t
        case .toolCall(let c): calls.append(c)
        }
    }
    return (content, calls)
}

private func argsOf(_ call: JSON) -> JSON {
    let raw = call["function"]["arguments"]
    if let s = raw.asString { return (try? JSON.parse(s)) ?? .object([:]) }
    return raw
}

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

    @Test func plainTextNoCalls() async throws {
        let (c, calls) = try await drain(parseToolCalls(rawStream(["Hello ", "world"])))
        #expect(c == "Hello world")
        #expect(calls.isEmpty)
    }

    @Test func singleCallAndProse() async throws {
        let env =
            #"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"reflect"}}}</tool_call>"#
        let (c, calls) = try await drain(parseToolCalls(rawStream(["ok ", env])))
        #expect(c == "ok ")
        #expect(calls.count == 1)
        #expect(argsOf(calls[0])["target_id"].asString == "core")
    }

    @Test func tagSplitAcrossSingleCharChunks() async throws {
        let full =
            #"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"list_agents"}}}</tool_call>"#
        let chunks = full.map { String($0) }
        let (c, calls) = try await drain(parseToolCalls(rawStream(chunks)))
        #expect(c == "")
        #expect(calls.count == 1)
        #expect(argsOf(calls[0])["target_id"].asString == "core")
    }

    @Test func multipleCalls() async throws {
        let a = #"<tool_call>{"name":"send","arguments":{"target_id":"a","payload":{}}}</tool_call>"#
        let b = #"<tool_call>{"name":"send","arguments":{"target_id":"b","payload":{}}}</tool_call>"#
        let (_, calls) = try await drain(parseToolCalls(rawStream([a, "\n", b])))
        #expect(calls.count == 2)
        #expect(argsOf(calls[0])["target_id"].asString == "a")
        #expect(argsOf(calls[1])["target_id"].asString == "b")
    }

    @Test func malformedAndUnterminatedSurfaceAsContent() async throws {
        let (c1, calls1) = try await drain(parseToolCalls(rawStream(["<tool_call>{nope}</tool_call>"])))
        #expect(calls1.isEmpty)
        #expect(c1 == "<tool_call>{nope}</tool_call>")

        let (c2, calls2) = try await drain(parseToolCalls(rawStream([#"<tool_call>{"name":"send""#])))
        #expect(calls2.isEmpty)
        #expect(c2.hasPrefix("<tool_call>"))
    }

    @Test func loneAngleBracketNotHeld() async throws {
        let (c, calls) = try await drain(parseToolCalls(rawStream(["a < b ", "and c"])))
        #expect(calls.isEmpty)
        #expect(c == "a < b and c")
    }

    @Test func extractAndRenderRoundTrip() {
        let s = renderToolCall(
            name: "send", arguments: .object(["target_id": .string("core")]))
        let calls = extractToolCalls(s)
        #expect(calls.count == 1)
        #expect(calls[0].1["target_id"].asString == "core")
    }
}
