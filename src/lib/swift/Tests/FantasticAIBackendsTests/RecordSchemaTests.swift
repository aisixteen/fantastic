// Lib request #3 (2026-09-30) — a backend KIND's record schema, readable before any
// agent of it exists, and the SAME schema enforced at the write. Twins of python's
// `tests/kernel/test_kernel_reflect_extras.py`
// (`test_the_catalog_carries_each_backend_kinds_record_schema`,
// `test_the_schema_is_what_the_write_enforces`); the field-for-field comparison
// against python's own output is in `FantasticParityTests`.

import FantasticAICore
import FantasticAnthropicBackend
import FantasticJSON
import FantasticKernel
import FantasticLlamaCppBackend
import FantasticNvidiaNimBackend
import FantasticOllamaBackend
import FantasticResponsesBackend
import FantasticVllmBackend
import Foundation
import OrderedCollections
import Testing

/// A bundle that publishes no schema — the catalog must not invent one.
private struct Plain: AgentBundle {
    let name = "plain"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? { nil }
}

@Suite struct RecordSchemaTests {
    private func kernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register(FantasticAnthropicBackend.HANDLER_MODULE, AnthropicBundle())
        registry.register(FantasticResponsesBackend.HANDLER_MODULE, ResponsesBundle())
        registry.register(FantasticNvidiaNimBackend.HANDLER_MODULE, NvidiaNimBundle())
        registry.register(FantasticOllamaBackend.HANDLER_MODULE, OllamaBackendBundle())
        registry.register(FantasticLlamaCppBackend.HANDLER_MODULE, LlamaCppBundle())
        registry.register(FantasticVllmBackend.HANDLER_MODULE, VllmBundle())
        registry.register("plain.tools", Plain())
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        return k
    }

    private func catalog(_ k: Kernel, _ tier: String) async -> [JSON] {
        await k.send([Item(AgentId("kernel_state"), ["type": "reflect", "bundles": .string(tier)])])[0]
            .result["bundles"].asArray ?? []
    }

    @Test func theCatalogCarriesEachBackendKindsRecordSchema() async {
        let k = kernel()
        let by = Dictionary(
            uniqueKeysWithValues: await catalog(k, "all").map { ($0["name"].asString ?? "", $0) })
        let vllm = by["vllm_backend"]!["schema"]
        let fields = Dictionary(
            uniqueKeysWithValues: (vllm["fields"].asArray ?? []).map { ($0["name"].asString ?? "", $0) })
        let required = (vllm["fields"].asArray ?? []).filter { $0["required"].asBool == true }
            .compactMap { $0["name"].asString }
        #expect(
            required == [
                "endpoint", "model", "max_tokens", "kv_slots", "kv_overflow", "memory_store_id",
            ], "\(required)")
        // a required field has no default — nothing is guessed about a server
        #expect(required.allSatisfy { fields[$0]?.asObject?["default"] == nil })
        // the defaulted fields carry THE BACKEND'S OWN values
        #expect(fields["temperature"]?["default"] == .double(0.2))
        #expect(fields["thinking_token_budget"]?["default"] == .integer(16000))
        #expect(fields["kv_overflow"]?["enum"] == .array(["evict", "reject"]))
        #expect(fields["reasoning_effort"]?["enum"] == .array(["low", "medium", "xhigh"]))
        #expect(vllm["key_var"] == "VLLM_API_KEY")  // sent only when set
        #expect(by["responses_backend"]?["schema"]["key_var"] == "OPENAI_API_KEY")
        #expect(by["anthropic_backend"]?["schema"]["key_var"] == "ANTHROPIC_API_KEY")
        #expect(by["nvidia_nim_backend"]?["schema"]["key_var"] == "NVIDIA_API_KEY")
        // an entry is `name, handler_module, schema` in that order
        #expect(by["vllm_backend"]?.asObject.map { Array($0.keys) } == ["name", "handler_module", "schema"])
        // a bundle that is not a backend publishes none
        #expect(by["plain"]?.asObject?["schema"] == nil)
        // the cheap tier stays names only
        #expect((await catalog(k, "ids")).allSatisfy { $0.asString != nil })
    }

    @Test func theSchemaIsWhatTheWriteEnforces() async {
        let k = kernel()
        func create(_ meta: OrderedDictionary<String, JSON>) async -> JSON {
            var payload: OrderedDictionary<String, JSON> = [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "anthropic_backend.tools",
            ]
            for (key, v) in meta { payload[key] = v }
            return await k.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
        }
        var r = await create(["id": "a1"])
        #expect(r["reason"] == "invalid_meta", "\(r)")
        #expect(
            r["error"].asString?.contains("missing: endpoint, model, memory_store_id") == true,
            "\(r)")
        r = await create([
            "id": "a2", "endpoint": "http://127.0.0.1:9/stub", "model": "m",
            "memory_store_id": "memory", "context_strategy": "bogus",
        ])
        #expect(r["reason"] == "invalid_meta", "\(r)")
        #expect(r["error"].asString?.contains("context_strategy") == true, "\(r)")
        r = await create([
            "id": "a3", "endpoint": "http://127.0.0.1:9/stub", "model": "m",
            "memory_store_id": "memory",
        ])
        #expect(r["id"] == "a3", "\(r)")
    }

    /// The refusal texts are python's, byte for byte.
    @Test func theRefusalTextsArePythons() {
        #expect(
            NvidiaNimBundle().validateMeta([:], record: [:])
                == "nvidia_nim_backend: required on the record and missing: endpoint, model, "
                + "memory_store_id — nothing is guessed. endpoint = the NIM OpenAI-compatible "
                + "URL; model = the model name; memory_store_id = the memory store agent that "
                + "keeps this agent's conversation (e.g. a ram_store)")
        #expect(
            OllamaBackendBundle().validateMeta(
                ["context_strategy": "bogus"],
                record: ["endpoint": "http://h", "model": "m", "memory_store_id": "memory"])
                == "context_strategy='bogus' is not one of the legal values: compact, truncate, "
                + "strip, fold")
        // a blank string is not a value; a present non-string field is
        #expect(
            LlamaCppBundle().validateMeta(["endpoint": "  "], record: ["memory_store_id": "m"])?
                .contains("missing: endpoint —") == true)
        // an update is judged on the MERGED record, and a null clears a field
        let full: OrderedDictionary<String, JSON> = [
            "endpoint": "http://h/v1", "model": "m", "memory_store_id": "memory",
        ]
        #expect(ResponsesBundle().validateMeta(["max_tokens": 10], record: full) == nil)
        #expect(
            ResponsesBundle().validateMeta(["model": .null], record: full)?
                .contains("missing: model —") == true)
    }
}
