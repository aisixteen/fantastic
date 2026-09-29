// The connectors never guess a network-reaching endpoint or model (by default,
// no defaults). Both are REQUIRED on the record: building the provider fails
// loud naming the missing field (python `ai_core.required_meta`), and the
// provider's stream path still refuses an empty model before any network I/O.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import OrderedCollections
import Testing

@testable import FantasticAnthropicBackend
@testable import FantasticLlamaCppBackend
@testable import FantasticNvidiaNimBackend
@testable import FantasticOllamaBackend
@testable import FantasticResponsesBackend

@Suite("NoDefaultModel")
struct NoDefaultModelTests {

    private static func build(_ cfg: AIBackendConfig, _ meta: OrderedDictionary<String, JSON>)
        async -> String?
    {
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let a = Agent(id: AgentId("a"), handlerModule: nil, parentId: nil, meta: meta)
        do {
            _ = try await cfg.makeProvider(a, "c", kernel)
            return nil
        } catch {
            return "\(error)"
        }
    }

    // Every backend names the FIRST missing connector field, in python's words.
    @Test("an absent endpoint or model fails loud, naming the field")
    func requiredFields() async {
        let cases: [(String, AIBackendConfig, Bool)] = [
            ("ollama_backend", OllamaBackendBundle().core.config, true),
            ("anthropic_backend", AnthropicBundle().core.config, true),
            ("responses_backend", ResponsesBundle().core.config, true),
            ("llamacpp_backend", LlamaCppBundle().core.config, false),
        ]
        for (kind, cfg, needsModel) in cases {
            #expect(
                await Self.build(cfg, [:])
                    == "ValueError: \(kind): `endpoint` is required on the agent record — "
                    + "nothing is guessed (update_agent <id> endpoint=…)", "\(kind)")
            let noModel = await Self.build(cfg, ["endpoint": .string("http://h.invalid")])
            if needsModel {
                #expect(
                    noModel
                        == "ValueError: \(kind): `model` is required on the agent record — "
                        + "nothing is guessed (update_agent <id> model=…)", "\(kind)")
            }
        }
    }

    // The stream path raises loud — naming `model` — before any network I/O.
    @Test("anthropic chat raises loud when the model is empty")
    func anthropicGuard() async {
        let provider = AnthropicProvider(
            endpoint: "http://h.invalid/v1/messages",
            model: "",
            apiKey: "unused",
            maxTokens: 64,
            sampling: [:])
        await #expect(throws: Error.self) {
            for try await _ in provider.chat(messages: [
                .object(["role": .string("user"), "content": .string("hi")])
            ]) {}
        }
    }

    @Test("nvidia chat raises loud when the model is empty")
    func nvidiaGuard() async {
        let provider = NvidiaNimProvider(
            endpoint: "http://h.invalid/v1",
            model: "",
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

    // B1.25: the extras are temperature / num_predict / think / has_api_key, and
    // `num_ctx` is deliberately ABSENT (llama-server fixes the window at start).
    @Test("llamacpp reflect extras match python")
    func llamacppReflectExtrasMatchPython() {
        let cfg = LlamaCppBundle().core.config
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
