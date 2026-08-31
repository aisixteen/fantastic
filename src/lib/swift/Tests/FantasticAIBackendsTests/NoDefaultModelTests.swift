// The connectors never guess a network-reaching model (by default, no defaults).
// DEFAULT_MODEL is a fail-loud SENTINEL, not a real id — it exists only so
// reflect can say what is true, and the provider's stream path raises loud when
// the resolved model is empty or still the sentinel. Mirrors python's
// `{anthropic,nvidia_nim}_backend.provider` guard.

import FantasticJSON
import Testing

@testable import FantasticAnthropicBackend
@testable import FantasticNvidiaNimBackend

@Suite("NoDefaultModel")
struct NoDefaultModelTests {

    // A bare construction yields the sentinel, never a real model id.
    @Test("anthropic DEFAULT_MODEL is the fail-loud sentinel")
    func anthropicSentinel() {
        #expect(FantasticAnthropicBackend.DEFAULT_MODEL == "(no default — set the `model` meta)")
    }

    @Test("nvidia DEFAULT_MODEL is the fail-loud sentinel")
    func nvidiaSentinel() {
        #expect(FantasticNvidiaNimBackend.DEFAULT_MODEL == "(no default — set the `model` meta)")
    }

    // The stream path raises loud — naming `model` — before any network I/O.
    @Test("anthropic chat raises loud when the model is the sentinel")
    func anthropicGuard() async {
        let provider = AnthropicProvider(
            endpoint: DEFAULT_ENDPOINT,
            model: FantasticAnthropicBackend.DEFAULT_MODEL,
            apiKey: "unused",
            maxTokens: 64,
            toolUse: false)
        await #expect(throws: Error.self) {
            for try await _ in provider.chat(messages: [
                .object(["role": .string("user"), "content": .string("hi")])
            ]) {}
        }
    }

    @Test("nvidia chat raises loud when the model is the sentinel")
    func nvidiaGuard() async {
        let provider = NvidiaNimProvider(
            host: "https://integrate.api.nvidia.com",
            model: FantasticNvidiaNimBackend.DEFAULT_MODEL,
            apiKey: "unused")
        await #expect(throws: Error.self) {
            for try await _ in provider.chat(messages: [
                .object(["role": .string("user"), "content": .string("hi")])
            ]) {}
        }
    }
}
