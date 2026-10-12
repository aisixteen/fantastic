// Ollama NDJSON wire-decode tests — the two-channel read.
//
// The defect these pin (measured live on gemma4:12b, 2026-08-02): the backend
// asked ollama for thinking and then read `message.content` alone, so a turn
// spent reasoning arrived as an EMPTY completion — no tokens, no tool call, no
// error, and ollama's log showing the request answered normally.

import FantasticAICore
import Foundation
import Testing

@testable import FantasticOllamaBackend

@Suite("ollama NDJSON decode")
struct OllamaStreamDecodeTests {

    /// Run a whole NDJSON stream through the decoder the way the provider does:
    /// one wrapper for the turn, stop on `done`, close a dangling span at the end.
    private func run(_ lines: [String]) -> (tokens: [String], stoppedEarly: Bool) {
        var wrap = ThinkingWrap()
        var out: [String] = []
        var stopped = false
        for line in lines {
            let decoded = decodeOllamaLine(line, wrap: &wrap)
            out += decoded.tokens
            if decoded.done {
                stopped = true
                break
            }
        }
        out += wrap.finish()
        return (out, stopped)
    }

    @Test func threeReasoningChunksAndOneContentChunkWrapExactlyOnce() {
        let (tokens, stopped) = run([
            #"{"message":{"thinking":"weigh ","content":""},"done":false}"#,
            #"{"message":{"thinking":"the ","content":""},"done":false}"#,
            #"{"message":{"thinking":"options","content":""},"done":false}"#,
            #"{"message":{"thinking":"","content":"42"},"done":false}"#,
            #"{"message":{"content":""},"done":true}"#,
        ])
        let text = tokens.joined()
        #expect(text == "<think>weigh the options</think>42")
        #expect(text.components(separatedBy: THINK_OPEN).count - 1 == 1)
        #expect(text.components(separatedBy: THINK_CLOSE).count - 1 == 1)
        #expect(stopped)  // the `done:true` line stops the read
    }

    @Test func aThinkingOnlyTurnStillCarriesText() {
        // The whole turn is reasoning. Reading `content` alone yields "" here —
        // an empty completion that silently ends a chain.
        let (tokens, _) = run([
            #"{"message":{"thinking":"all of it","content":""},"done":false}"#,
            #"{"message":{"content":""},"done":true}"#,
        ])
        #expect(tokens.joined() == "<think>all of it</think>")
    }

    @Test func aContentOnlyTurnIsUntouched() {
        let (tokens, _) = run([
            #"{"message":{"content":"Hel"},"done":false}"#,
            #"{"message":{"content":"lo"},"done":false}"#,
            #"{"message":{"content":""},"done":true}"#,
        ])
        #expect(tokens == ["Hel", "lo"])
    }

    @Test func explicitNullsAndNonJSONLinesAreSkipped() {
        // ollama sometimes sends an explicit `null` for the channel it isn't
        // using; a keep-alive / non-JSON line carries no model text to lose.
        var wrap = ThinkingWrap()
        let nothing = OllamaDecoded(tokens: [], done: false)
        let nulls = #"{"message":{"thinking":null,"content":null},"done":false}"#
        #expect(decodeOllamaLine(nulls, wrap: &wrap) == nothing)
        #expect(decodeOllamaLine("", wrap: &wrap) == nothing)
        #expect(decodeOllamaLine("not json", wrap: &wrap) == nothing)
        // …and none of that opened a span.
        #expect(wrap.finish().isEmpty)
    }

    @Test func aToolCallWrittenMidThoughtSurvivesTheDecode() {
        // The reason the reasoning text is streamed at all rather than dropped:
        // a call the model wrote inside its thought is still in the text a
        // reader parses.
        let call =
            #"<tool_call>{\"name\":\"send\",\"arguments\":{\"target_id\":\"core\"}}</tool_call>"#
        let (tokens, _) = run([
            #"{"message":{"thinking":"I will check: "# + call + #""},"done":false}"#,
            #"{"message":{"content":""},"done":true}"#,
        ])
        #expect(tokens.joined().contains("<tool_call>"))
    }
}
