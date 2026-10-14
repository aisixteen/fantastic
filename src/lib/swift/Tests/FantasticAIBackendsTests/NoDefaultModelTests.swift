// The connectors never guess a network-reaching endpoint or model (by default,
// no defaults). Both are REQUIRED on the record: building the provider fails
// loud naming the missing field (python `ai_core.required_meta`), and the
// provider's stream path still refuses an empty model before any network I/O.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import OrderedCollections
import Testing

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
        let cases: [(String, AIBackendConfig)] = [
            ("ollama_backend", OllamaBackendBundle().core.config),
            ("responses_backend", ResponsesBundle().core.config),
        ]
        for (kind, cfg) in cases {
            #expect(
                await Self.build(cfg, [:])
                    == "ValueError: \(kind): `endpoint` is required on the agent record — "
                    + "nothing is guessed (update_agent <id> endpoint=…)", "\(kind)")
            #expect(
                await Self.build(cfg, ["endpoint": .string("http://h.invalid")])
                    == "ValueError: \(kind): `model` is required on the agent record — "
                    + "nothing is guessed (update_agent <id> model=…)", "\(kind)")
        }
    }

    // ── the reflect extras ───────────────────────────────

    private func agent(_ meta: OrderedDictionary<String, JSON>) -> Agent {
        Agent(id: AgentId("a"), handlerModule: nil, parentId: nil, meta: meta)
    }

    // `model` and `endpoint` are BASE fields; the extras are the
    // sampling knobs, in python's order, and `temperature` defaults to 0.3.
    @Test("ollama reflect extras match python")
    func reflectExtrasMatchPython() {
        let cfg = OllamaBackendBundle().core.config
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let bare = cfg.reflectExtra(agent([:]), kernel)
        #expect(Array(bare.keys) == ["temperature", "num_ctx", "num_predict", "think"])
        #expect(bare["temperature"] == .double(0.3))
        #expect(bare["num_ctx"] == .null && bare["num_predict"] == .null && bare["think"] == .null)
        let set = cfg.reflectExtra(
            agent([
                "temperature": .double(0.7), "num_ctx": .integer(4096),
                "num_predict": .integer(64), "think": .bool(false),
            ]), kernel)
        #expect(set["temperature"] == .double(0.7))
        #expect(set["num_ctx"] == .integer(4096))
        #expect(set["num_predict"] == .integer(64))
        #expect(set["think"] == .bool(false))
    }
}
