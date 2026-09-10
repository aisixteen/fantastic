// A LIVE turn through the Anthropic backend. Opt-in, and off by default.
//
// The rest of this suite is pure-function wire decoding, which is the right
// default: unit tests must not need a network, a key, or somebody's GPU. But a
// decoder that parses every synthetic line correctly can still fail against the
// real thing — wrong header, wrong path, wrong body shape — and only a real
// request finds that.
//
// Enable it deliberately:
//
//   ANTHROPIC_LIVE=1 \
//   ANTHROPIC_ENDPOINT=http://127.0.0.1:4000/cc/v1 \
//   ANTHROPIC_API_KEY=keyless \
//   ANTHROPIC_MODEL=<your tool-calling model> \
//   swift test --filter AnthropicLive
//
// Skipped otherwise, so `swift test` stays hermetic and free.

import FantasticAICore
import FantasticJSON
import Foundation
import Testing

@testable import FantasticAnthropicBackend

private var liveEnabled: Bool { ProcessInfo.processInfo.environment["ANTHROPIC_LIVE"] == "1" }
private func env(_ k: String) -> String? { ProcessInfo.processInfo.environment[k] }

@Suite("AnthropicLive", .enabled(if: liveEnabled))
struct AnthropicLiveTests {

    @Test("a real turn streams text back and ends with a terminal stop_reason")
    func liveTurn() async throws {
        let provider = AnthropicProvider(
            endpoint: env("ANTHROPIC_ENDPOINT") ?? DEFAULT_ENDPOINT,
            // Explicit model — DEFAULT_MODEL is now the fail-loud sentinel, so a
            // live turn must name a real model (the connector never guesses one).
            model: env("ANTHROPIC_MODEL") ?? "claude-sonnet-4-6",
            apiKey: env("ANTHROPIC_API_KEY") ?? "",
            maxTokens: 64)

        var text = ""
        // Any error here is a REAL failure: a non-terminal stream, an HTTP
        // status, or a decode that produced nothing. The provider raises rather
        // than returning a half turn, which is the property under test.
        for try await chunk in provider.chat(messages: [
            .object(["role": .string("user"), "content": .string("Reply with the single word: pong")])
        ]) {
            if case .token(let t) = chunk { text += t }
        }
        #expect(!text.isEmpty, "a live turn produced no text at all")
        // `withoutThinking` is what the core applies before persisting a turn;
        // a reasoning model puts its answer outside the thought.
        #expect(text.lowercased().contains("pong"), "unexpected answer: \(text)")
    }
}
