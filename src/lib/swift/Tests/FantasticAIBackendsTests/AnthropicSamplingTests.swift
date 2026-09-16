// The record's `sampling` object rides the Messages-API request body VERBATIM,
// after the fixed fields and `system`, null values dropped — python's
// `body.update({k: v for k, v in sampling.items() if v is not None})`, rust's
// `for (k, v) in &self.sampling` after `system`. Without it a reasoning model's
// deliberation is unbounded (`thinking_token_budget`) and the CoT-repetition
// loop runs free (`no_repeat_ngram_size`, `repetition_penalty`): measured
// 2026-09-15, swift E20 build failed 3/3 on the harness's 180s send cap.

import FantasticJSON
import OrderedCollections
import Testing

@testable import FantasticAnthropicBackend

@Suite("AnthropicSampling")
struct AnthropicSamplingTests {

    private let turn: [JSON] = [
        .object(["role": .string("system"), "content": .string("be terse")]),
        .object(["role": .string("user"), "content": .string("hi")]),
    ]

    private func provider(sampling: OrderedDictionary<String, JSON>) -> AnthropicProvider {
        AnthropicProvider(
            endpoint: DEFAULT_ENDPOINT,
            model: "claude-x",
            apiKey: "unused",
            maxTokens: 64,
            sampling: sampling)
    }

    // The meta reader: an object keeps its non-null keys in record order; a
    // null value is omitted; unset / non-object is empty.
    @Test("sampling meta drops null values and keeps record order")
    func metaReader() {
        let raw: JSON = .object(["a": .integer(1), "b": .null, "c": .string("x")])
        let got = AnthropicBundle.sampling(raw)
        #expect(Array(got.keys) == ["a", "c"])
        #expect(got["a"] == .integer(1))
        #expect(got["c"] == .string("x"))
        #expect(AnthropicBundle.sampling(nil) == [:])
        #expect(AnthropicBundle.sampling(.string("nope")) == [:])
    }

    // B1.7: PYTHON'S KEY ORDER — `model, max_tokens, messages, stream`, then
    // `system`, then the sampling knobs LAST in the record's order; a null never
    // reaches the wire.
    @Test("request body key order matches python")
    func requestBodyKeyOrderMatchesPython() {
        let p = provider(
            sampling: AnthropicBundle.sampling(
                .object(["a": .integer(1), "b": .null, "c": .string("x")])))
        let body = p.requestBody(messages: turn)
        #expect(
            Array(body.keys) == ["model", "max_tokens", "messages", "stream", "system", "a", "c"])
        #expect(body["a"] == .integer(1))
        #expect(body["c"] == .string("x"))
        #expect(body["b"] == nil)
        #expect(body["system"] == .string("be terse"))
    }

    // A record without `sampling` yields python's four fixed keys plus `system`.
    @Test("no sampling meta leaves the body untouched")
    func bodyWithoutSampling() {
        let body = provider(sampling: AnthropicBundle.sampling(nil)).requestBody(messages: turn)
        #expect(Array(body.keys) == ["model", "max_tokens", "messages", "stream", "system"])
        #expect(body["model"] == .string("claude-x"))
        #expect(body["max_tokens"] == .integer(64))
        #expect(body["stream"] == .bool(true))
        #expect(body["messages"]?.asArray?.count == 1)
    }

    // B1.7/B1.8: `if system:` — an EMPTY system block is no `system` key at all.
    @Test("an empty system block leaves no system key")
    func emptySystemIsOmitted() {
        let body = provider(sampling: [:]).requestBody(messages: [
            .object(["role": .string("system"), "content": .string("")]),
            .object(["role": .string("user"), "content": .string("hi")]),
        ])
        #expect(Array(body.keys) == ["model", "max_tokens", "messages", "stream"])
    }

    // The harness's real knobs, serialized: the JSON text carries every key.
    @Test("the e2e sampling knobs reach the serialized payload")
    func harnessKnobsSerialize() {
        let p = provider(
            sampling: AnthropicBundle.sampling(
                .object([
                    "no_repeat_ngram_size": .integer(3),
                    "repetition_penalty": .double(1.05),
                    "thinking_token_budget": .integer(8000),
                ])))
        let text = JSON.object(p.requestBody(messages: turn)).serialize()
        #expect(text.contains("\"no_repeat_ngram_size\":3"))
        #expect(text.contains("\"repetition_penalty\":1.05"))
        #expect(text.contains("\"thinking_token_budget\":8000"))
    }
}
