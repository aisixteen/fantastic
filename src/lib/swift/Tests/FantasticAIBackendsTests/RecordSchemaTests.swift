// Lib request #3 (2026-09-30) — a backend KIND's record schema, readable before any
// agent of it exists, and the SAME schema enforced at the write. Twins of python's
// `tests/kernel/test_kernel_reflect_extras.py`
// (`test_the_catalog_carries_each_backend_kinds_record_schema`,
// `test_the_schema_is_what_the_write_enforces`); the field-for-field comparison
// against python's own output is in `FantasticParityTests`.

import FantasticAICore
import FantasticJSON
import FantasticKernel
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
        registry.register(FantasticResponsesBackend.HANDLER_MODULE, ResponsesBundle())
        registry.register(FantasticOllamaBackend.HANDLER_MODULE, OllamaBackendBundle())
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
        #expect(required == ["endpoint", "kv_overflow", "memory_store_id"], "\(required)")
        // a required field has no default; the counts a record leaves out are read
        // from the server before a turn — no default either
        #expect(required.allSatisfy { fields[$0]?.asObject?["default"] == nil })
        for n in ["model", "max_tokens", "context_window", "kv_slots"] {
            #expect(fields[n]?.asObject?["default"] == nil, "\(n)")
            #expect(fields[n]?["hint"].asString?.contains("read from the server") == true, "\(n)")
        }
        // the defaulted fields carry THE BACKEND'S OWN values
        #expect(fields["temperature"]?["default"] == .double(0.2))
        #expect(fields["thinking_token_budget"]?["default"] == .integer(16000))
        #expect(fields["kv_overflow"]?["enum"] == .array(["evict", "reject"]))
        #expect(fields["reasoning_effort"]?["enum"] == .array(["low", "medium", "xhigh"]))
        // the key: optional where a server may be keyless, required where it never
        // is — a `credentials` field (a store ref), never an environment variable
        #expect(fields["credentials"]?["type"] == "credentials")
        #expect(fields["credentials"]?["required"] == .bool(false))
        #expect(vllm.asObject.map { Array($0.keys) } == ["fields"], "no `key_var`")
        let sch = by["responses_backend"]?["schema"] ?? .null
        #expect(sch.asObject.map { Array($0.keys) } == ["fields"], "responses_backend: no `key_var`")
        let cred = (sch["fields"].asArray ?? []).first { $0["name"] == "credentials" }
        #expect(cred?["required"] == .bool(true))
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
                "handler_module": "responses_backend.tools",
            ]
            for (key, v) in meta { payload[key] = v }
            return await k.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
        }
        var r = await create(["id": "a1"])
        #expect(r["reason"] == "invalid_meta", "\(r)")
        #expect(
            r["error"].asString?.contains("missing: endpoint, model, credentials, memory_store_id")
                == true,
            "\(r)")
        r = await create([
            "id": "a2", "endpoint": "http://127.0.0.1:9/stub", "model": "m",
            "memory_store_id": "memory", "credentials": ["store": "creds", "ref": "main"],
            "context_strategy": "bogus",
        ])
        #expect(r["reason"] == "invalid_meta", "\(r)")
        #expect(r["error"].asString?.contains("context_strategy") == true, "\(r)")
        r = await create([
            "id": "a3", "endpoint": "http://127.0.0.1:9/stub", "model": "m",
            "memory_store_id": "memory", "credentials": ["store": "creds"],
        ])
        #expect(r["reason"] == "invalid_meta", "\(r)")
        #expect(r["error"].asString?.contains("credentials must be {store") == true, "\(r)")
        r = await create([
            "id": "a4", "endpoint": "http://127.0.0.1:9/stub", "model": "m",
            "memory_store_id": "memory", "credentials": ["store": "creds", "ref": "main"],
        ])
        #expect(r["id"] == "a4", "\(r)")
    }

    /// `system_prompt` (every AI agent's, owner 2026-10-03): an optional `str` in
    /// the shared fields, refused AT THE WRITE unless a non-blank string; null
    /// clears it. Python's texts.
    @Test func aRecordSystemPromptIsAnOptionalStrRefusedAtTheWriteUnlessNonBlank() async {
        let refusal =
            "system_prompt must be a non-empty string (it replaces the default system "
            + "block on every turn) — null clears it"
        let hint =
            "a system block that REPLACES the default one on every turn of this agent (a "
            + "per-call `system_prompt` beats it for that delivery); placeholders {tree} "
            + "{self} {store} {howto} are filled each turn; null = the default"
        let field = (OllamaBackendBundle().recordSchema?["fields"].asArray ?? [])
            .first { $0["name"] == "system_prompt" }
        #expect(
            field
                == .object([
                    "name": "system_prompt", "type": "str", "required": false,
                    "hint": .string(hint),
                ]), "\(field ?? .null)")
        let full: OrderedDictionary<String, JSON> = [
            "endpoint": "http://h", "model": "m", "memory_store_id": "memory",
        ]
        let b = OllamaBackendBundle()
        for bad: JSON in ["", "  \n\t", 42, .bool(true), ["x"], ["a": "b"]] {
            #expect(b.validateMeta(["system_prompt": bad], record: full) == refusal, "\(bad)")
        }
        #expect(b.validateMeta(["system_prompt": "You are a terse reviewer."], record: full) == nil)
        #expect(b.validateMeta(["system_prompt": .null], record: full) == nil, "null clears it")
        // at the kernel's write: create refused, a good update lands, a bad one refused
        let k = kernel()
        let create: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state", "id": "o1",
            "handler_module": "ollama_backend.tools", "endpoint": "http://h", "model": "m",
            "memory_store_id": "memory", "system_prompt": " ",
        ]
        var r = await k.send([Item(AgentId("kernel_state"), .object(create))])[0].result
        #expect(r["reason"] == "invalid_meta" && r["error"].asString == refusal, "\(r)")
        var ok = create
        ok["system_prompt"] = "ROLE"
        r = await k.send([Item(AgentId("kernel_state"), .object(ok))])[0].result
        #expect(r["id"] == "o1", "\(r)")
        r = await k.send([
            Item(AgentId("kernel_state"), ["type": "update_agent", "id": "o1", "system_prompt": 7])
        ])[0].result
        #expect(r["reason"] == "invalid_meta" && r["error"].asString == refusal, "\(r)")
        #expect(k.agent(AgentId("o1"))?.metaValue(forKey: "system_prompt") == "ROLE")
        r = await k.send([
            Item(AgentId("kernel_state"), ["type": "update_agent", "id": "o1", "system_prompt": .null])
        ])[0].result
        #expect(r["error"].isNull, "\(r)")
    }

    /// The refusal texts are python's, byte for byte.
    @Test func theRefusalTextsArePythons() {
        #expect(
            ResponsesBundle().validateMeta([:], record: [:])
                == "responses_backend: required on the record and missing: endpoint, model, "
                + "credentials, memory_store_id — nothing is guessed. endpoint = the full "
                + "Responses API URL (…/v1/responses); model = the model name; credentials = "
                + "{store, ref}: "
                + "the credential store and ref holding `api_key` — read at each request, sent "
                + "only to the ref's host; memory_store_id = the memory store agent that "
                + "keeps this agent's conversation (e.g. a ram_store)")
        #expect(
            OllamaBackendBundle().validateMeta(
                ["context_strategy": "bogus"],
                record: ["endpoint": "http://h", "model": "m", "memory_store_id": "memory"])
                == "context_strategy='bogus' is not one of the legal values: compact, truncate, "
                + "strip, fold")
        // a blank string is not a value; a present non-string field is
        #expect(
            OllamaBackendBundle().validateMeta(
                ["endpoint": "  "], record: ["model": "m", "memory_store_id": "m"])
                == "ollama_backend: required on the record and missing: endpoint — nothing is "
                + "guessed. endpoint = the ollama server URL")
        // an update is judged on the MERGED record, and a null clears a field
        let full: OrderedDictionary<String, JSON> = [
            "endpoint": "http://h/v1", "model": "m", "memory_store_id": "memory",
            "credentials": ["store": "creds", "ref": "r"],
        ]
        #expect(ResponsesBundle().validateMeta(["max_tokens": 10], record: full) == nil)
        #expect(
            ResponsesBundle().validateMeta(["model": .null], record: full)?
                .contains("missing: model —") == true)
        // a malformed `credentials` is refused at the write, python's texts
        #expect(
            ResponsesBundle().validateMeta(["credentials": "creds/r"], record: full)
                == "credentials must be {store: <credential_store id>, ref: <ref>}")
        #expect(
            ResponsesBundle().validateMeta(["credentials": ["store": "", "ref": "r"]], record: full)
                == "credentials.store must be a non-empty string")
        // optional on a keyless server: absent is fine, a null clears it
        #expect(
            VllmBundle().validateMeta(
                ["credentials": .null],
                record: [
                    "endpoint": "http://h", "model": "m", "max_tokens": 10, "kv_slots": 2,
                    "kv_overflow": "evict", "memory_store_id": "m", "context_window": 262144,
                ])
                == nil)
    }
}
