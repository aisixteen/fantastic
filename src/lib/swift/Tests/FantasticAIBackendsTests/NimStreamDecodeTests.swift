// NIM SSE wire-decode tests — the two-channel read.
//
// The defect these pin: nemotron/deepseek-style NIM models stream most tokens as
// `reasoning_content` and only the conclusion as `content` (measured 740 chars of
// reasoning to 129 of content on one turn). Reading `content` alone silently
// dropped whole turns — the chain died with no error anywhere.

import FantasticAICore
import Foundation
import Testing

@testable import FantasticNvidiaNimBackend

@Suite("NIM SSE decode")
struct NimStreamDecodeTests {

    /// Run a whole SSE stream through the decoder the way the provider does: one
    /// wrapper for the turn, stop on `[DONE]`, close a dangling span at the end.
    private func run(_ lines: [String]) -> (tokens: [String], sawDone: Bool) {
        var wrap = ThinkingWrap()
        var out: [String] = []
        var sawDone = false
        for line in lines {
            let decoded = decodeNimSSELine(line, wrap: &wrap)
            out += decoded.tokens
            if decoded.done {
                sawDone = true
                break
            }
        }
        out += wrap.finish()
        return (out, sawDone)
    }

    @Test func threeReasoningChunksAndOneContentChunkWrapExactlyOnce() {
        let (tokens, sawDone) = run([
            #"data: {"choices":[{"delta":{"reasoning_content":"weigh "}}]}"#,
            #"data: {"choices":[{"delta":{"reasoning_content":"the "}}]}"#,
            #"data: {"choices":[{"delta":{"reasoning_content":"options"}}]}"#,
            #"data: {"choices":[{"delta":{"content":"42"}}]}"#,
            "data: [DONE]",
        ])
        let text = tokens.joined()
        #expect(text == "<think>weigh the options</think>42")
        #expect(text.components(separatedBy: THINK_OPEN).count - 1 == 1)
        #expect(text.components(separatedBy: THINK_CLOSE).count - 1 == 1)
        #expect(sawDone)
    }

    @Test func aReasoningOnlyTurnStillCarriesText() {
        let (tokens, _) = run([
            #"data: {"choices":[{"delta":{"reasoning_content":"all of it"}}]}"#,
            "data: [DONE]",
        ])
        #expect(tokens.joined() == "<think>all of it</think>")
    }

    @Test func aContentOnlyTurnIsUntouched() {
        let (tokens, _) = run([
            #"data: {"choices":[{"delta":{"content":"Hel"}}]}"#,
            #"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
            "data: [DONE]",
        ])
        #expect(tokens == ["Hel", "lo"])
    }

    @Test func aDataLineWithoutTheSpaceStillDecodes() {
        // The prefix test is `data:` and the payload is trimmed — matching python.
        // Requiring `"data: "` would silently drop a compliant line, which is the
        // same class of bug as reading one channel.
        var wrap = ThinkingWrap()
        let d = decodeNimSSELine(#"data:{"choices":[{"delta":{"content":"x"}}]}"#, wrap: &wrap)
        #expect(d == NimDecoded(tokens: ["x"], done: false))
        #expect(decodeNimSSELine("data:[DONE]", wrap: &wrap) == NimDecoded(tokens: [], done: true))
    }

    @Test func nonDataAndEmptyAndMalformedLinesAreSkipped() {
        var wrap = ThinkingWrap()
        let nothing = NimDecoded(tokens: [], done: false)
        #expect(decodeNimSSELine("", wrap: &wrap) == nothing)
        #expect(decodeNimSSELine(": keep-alive comment", wrap: &wrap) == nothing)
        #expect(decodeNimSSELine("event: message", wrap: &wrap) == nothing)
        #expect(decodeNimSSELine("data:", wrap: &wrap) == nothing)
        #expect(decodeNimSSELine("data: {not json", wrap: &wrap) == nothing)
        // A chunk with no choices (NIM opens some streams with a usage-only
        // frame) carries no text.
        #expect(decodeNimSSELine(#"data: {"choices":[]}"#, wrap: &wrap) == nothing)
        // A null channel is the wire saying "not this one".
        #expect(
            decodeNimSSELine(
                #"data: {"choices":[{"delta":{"content":null,"reasoning_content":null}}]}"#,
                wrap: &wrap) == nothing)
        // …and none of that opened a span.
        #expect(wrap.finish().isEmpty)
    }

    @Test func interleavedChannelsStayBalanced() {
        // A model that thinks, answers, then thinks again: each span opens and
        // closes exactly once, so the text a reader parses is never unbalanced.
        let (tokens, _) = run([
            #"data: {"choices":[{"delta":{"reasoning_content":"a"}}]}"#,
            #"data: {"choices":[{"delta":{"content":"X"}}]}"#,
            #"data: {"choices":[{"delta":{"reasoning_content":"b"}}]}"#,
            #"data: {"choices":[{"delta":{"content":"Y"}}]}"#,
            "data: [DONE]",
        ])
        #expect(tokens.joined() == "<think>a</think>X<think>b</think>Y")
    }
}
