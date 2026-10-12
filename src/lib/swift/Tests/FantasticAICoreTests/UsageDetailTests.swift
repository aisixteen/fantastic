// The token usage a finished generation's `status` `done` carries — python
// `test_the_done_status_carries_the_token_usage`, rust `the_done_detail_carries_the_token_usage`.

import FantasticJSON
import Testing

@testable import FantasticAICore

@Suite("usage on the done status, thinking on the row")
struct UsageDetailTests {
    @Test func theDoneDetailCarriesTheTokenUsage() {
        let view: [JSON] = [.object(["role": "user", "content": "go"])]
        let usage = AIUsage()
        usage.lastPromptTokens = 1234
        usage.lastCompletionTokens = 56
        let d = usageDetail(usage, view, "answer", 1.23456, 32768)
        #expect(d.map(\.0) == ["prompt_tokens", "completion_tokens", "gen_seconds", "context_window"])
        #expect(d.map(\.1) == [.integer(1234), .integer(56), .double(1.235), .integer(32768)])
        let e = Dictionary(uniqueKeysWithValues: usageDetail(nil, view, "abcdefgh", 0.5, 8192))
        #expect(e["prompt_tokens_estimated"] == .bool(true))
        #expect(e["completion_tokens"] == .integer(2))
        #expect(e["completion_tokens_estimated"] == .bool(true))
        #expect((e["prompt_tokens"]?.asInt ?? 0) > 0)
    }

    /// python `thinking_fields`: every span, joined by a blank line; none → empty.
    @Test func thinkingIsEverySpanJoined() {
        #expect(thinkingFields("<think>a\nb</think>answer").map(\.1) == [.string("a\nb")])
        #expect(thinkingFields("<think>x</think>mid<think>y").map(\.1) == [.string("x\n\ny")])
        #expect(thinkingFields("plain").isEmpty)
        let long = "<think>" + String(repeating: "z", count: THINKING_CAP + 5) + "</think>"
        #expect(thinkingFields(long).last?.1 == .integer(5))
    }
}
