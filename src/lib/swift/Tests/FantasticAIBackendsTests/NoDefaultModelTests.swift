// The connectors never guess a network-reaching model (by default, no defaults).
// DEFAULT_MODEL is a fail-loud SENTINEL, not a real id — it exists only so
// reflect can say what is true, and the provider's stream path raises loud when
// the resolved model is empty or still the sentinel. Mirrors python's
// `{anthropic,nvidia_nim}_backend.provider` guard.

import FantasticJSON
import FantasticKernel
import OrderedCollections
import Testing

@testable import FantasticAnthropicBackend
@testable import FantasticLlamaCppBackend
@testable import FantasticNvidiaNimBackend
@testable import FantasticOllamaBackend

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
            endpoint: FantasticAnthropicBackend.DEFAULT_ENDPOINT,
            model: FantasticAnthropicBackend.DEFAULT_MODEL,
            apiKey: "unused",
            maxTokens: 64,
            sampling: [:])
        await #expect(throws: Error.self) {
            for try await _ in provider.chat(messages: [
                .object(["role": .string("user"), "content": .string("hi")])
            ]) {}
        }
    }

    @Test("nvidia chat raises loud when the model is the sentinel")
    func nvidiaGuard() async {
        let provider = NvidiaNimProvider(
            endpoint: FantasticNvidiaNimBackend.DEFAULT_ENDPOINT,
            model: FantasticNvidiaNimBackend.DEFAULT_MODEL,
            apiKey: "unused")
        await #expect(throws: Error.self) {
            for try await _ in provider.chat(messages: [
                .object(["role": .string("user"), "content": .string("hi")])
            ]) {}
        }
    }

    // ── the reflect extras (B1.24 / B1.25) ───────────────────────────────

    private func agent(_ meta: OrderedDictionary<String, JSON>) -> Agent {
        Agent(id: AgentId("a"), handlerModule: nil, parentId: nil, meta: meta)
    }

    // B1.24: `model` and `endpoint` are BASE fields now; the extras are the
    // sampling knobs, in python's order, and `temperature` defaults to 0.3.
    @Test("ollama reflect extras match python")
    func reflectExtrasMatchPython() {
        let cfg = OllamaBackendBundle().core.config
        #expect(cfg.defaultModel == "(no default — set the `model` meta)")
        #expect(cfg.defaultEndpoint == "http://localhost:11434")
        let bare = cfg.reflectExtra(agent([:]))
        #expect(Array(bare.keys) == ["temperature", "num_ctx", "num_predict", "think"])
        #expect(bare["temperature"] == .double(0.3))
        #expect(bare["num_ctx"] == .null && bare["num_predict"] == .null && bare["think"] == .null)
        let set = cfg.reflectExtra(
            agent([
                "temperature": .double(0.7), "num_ctx": .integer(4096),
                "num_predict": .integer(64), "think": .bool(false),
            ]))
        #expect(set["temperature"] == .double(0.7))
        #expect(set["num_ctx"] == .integer(4096))
        #expect(set["num_predict"] == .integer(64))
        #expect(set["think"] == .bool(false))
    }

    // B1.25: llama-server owns the model, so `reflect.model` says so; the extras
    // are temperature / num_predict / think / has_api_key, and `num_ctx` is
    // deliberately ABSENT.
    @Test("llamacpp reflect extras match python")
    func llamacppReflectExtrasMatchPython() {
        let cfg = LlamaCppBundle().core.config
        #expect(cfg.defaultModel == "(server-loaded)")
        #expect(cfg.defaultEndpoint == "http://127.0.0.1:8080/v1")
        let bare = cfg.reflectExtra(agent([:]))
        #expect(Array(bare.keys) == ["temperature", "num_predict", "think", "has_api_key"])
        #expect(bare["temperature"] == .double(0.3))
        #expect(bare["num_predict"] == .null && bare["think"] == .null)
        #expect(bare["has_api_key"] == .bool(false))
        // `int(rec[k]) if rec.get(k) else None` — a FALSY `num_predict` is no knob.
        let set = cfg.reflectExtra(
            agent([
                "temperature": .double(0.7), "num_predict": .integer(0),
                "think": .bool(false), "api_key": .string("k"),
            ]))
        #expect(set["temperature"] == .double(0.7))
        #expect(set["num_predict"] == .null)
        #expect(set["think"] == .bool(false))
        #expect(set["has_api_key"] == .bool(true))
    }
}
