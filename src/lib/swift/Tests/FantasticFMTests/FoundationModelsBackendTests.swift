// foundation_models_backend — the record it takes, what it refuses at the write,
// what reflect shows, how ai_core's messages become one FMCore request, and a
// whole turn through a kernel — all against a fake engine (no model, no macOS
// 26 needed). These are the texts the rust bundle must match byte for byte.

import FantasticAICore
import FantasticFMCore
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import OrderedCollections
import Testing

@testable import FantasticFoundationModelsBackend
@testable import FantasticRamStore

private let GUID = "g-fm"

/// A kernel with the FM bundle on `engine`, a ram_store `memory`, and an FM
/// agent `fm` that may call its store and the cli sink.
private func kernel(
    _ engine: any FMEngine, meta: OrderedDictionary<String, JSON> = [:]
) async -> (Kernel, JSON) {
    let registry = BundleRegistry()
    registry.register(HANDLER_MODULE, FoundationModelsBackendBundle(engine: engine))
    registry.register("ram_store.tools", RamStoreBundle())
    let k = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
        meta: ["guid": .string(GUID)])
    k.register(root)
    k.setRoot(root)
    _ = await k.send([
        Item(
            AgentId("kernel_state"),
            [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "ram_store.tools", "id": "memory",
                "callable_by": ["fm": "*"],
            ])
    ])
    var payload: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": .string(HANDLER_MODULE), "id": "fm",
        "memory_store_id": "memory",
        "can_call": ["memory": "*", "cli": "*"],
    ]
    for (key, v) in meta { payload[key] = v }
    let created = await k.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
    return (k, created)
}

private func send(_ k: Kernel, _ text: String, _ extra: OrderedDictionary<String, JSON> = [:])
    async -> JSON
{
    var payload: OrderedDictionary<String, JSON> = ["type": "send", "text": .string(text)]
    for (key, v) in extra { payload[key] = v }
    return await k.send([Item(AgentId("fm"), .object(payload))])[0].result
}

private func content(_ role: String, _ text: String) -> JSON {
    .object(["role": .string(role), "content": .string(text)])
}

@Suite("foundation_models_backend")
struct FoundationModelsBackendTests {

    // MARK: the record schema

    @Test func theSchemaIsTwoKnobsThenTheAIFieldsWithTheModelsWindow() {
        let schema = FoundationModelsBackendBundle(engine: FakeEngine(contextSize: 4096)).recordSchema!
        let fields = schema["fields"].asArray ?? []
        #expect(
            fields.compactMap { $0["name"].asString }
                == [
                    "temperature", "max_tokens", "memory_store_id", "context_window",
                    "output_reserve", "context_strategy", "recent_n", "system_prompt",
                    "file_bridge_id", "system_prompt_file", "pre_system_prompt",
                    "post_system_prompt",
                ])
        let by = Dictionary(uniqueKeysWithValues: fields.map { ($0["name"].asString!, $0) })
        // the window is the MODEL'S, read from the engine — not the kernel's 128k
        #expect(by["context_window"]?["default"] == .integer(4096))
        // the knobs have no default of ours: unset is the model's own
        #expect(by["temperature"]?.asObject?["default"] == nil)
        #expect(by["max_tokens"]?.asObject?["default"] == nil)
        #expect(
            fields.filter { $0["required"].asBool == true }.compactMap { $0["name"].asString }
                == ["memory_store_id"])
        // the exact field bytes rust must match
        #expect(
            fields[0]
                == .object([
                    "name": "temperature", "type": "float", "required": false,
                    "hint": "sampling temperature (>= 0); unset, the on-device model's own",
                ]))
        #expect(
            fields[1]
                == .object([
                    "name": "max_tokens", "type": "int", "required": false,
                    "hint": "a turn's output budget (tokens); unset, the on-device model's own",
                ]))
        // another engine, another window
        let big = FoundationModelsBackendBundle(engine: FakeEngine(contextSize: 8192)).recordSchema!
        #expect(big["fields"][3]["default"] == .integer(8192))
    }

    @Test func theCatalogCarriesTheSchemaWithNoAgentExisting() async {
        let registry = BundleRegistry()
        registry.register(HANDLER_MODULE, FoundationModelsBackendBundle(engine: FakeEngine(contextSize: 4096)))
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        let bundles =
            await k.send([Item(AgentId("kernel_state"), ["type": "reflect", "bundles": "all"])])[0]
            .result["bundles"].asArray ?? []
        let fm = bundles.first { $0["name"].asString == "foundation_models_backend" }
        #expect(fm != nil, "\(bundles.map { $0["name"] })")
        #expect(fm?["schema"]["fields"][3]["default"] == .integer(4096))
    }

    @Test func theDefaultRegistryCarriesTheBundle() {
        #expect(defaultBundleRegistry().get(HANDLER_MODULE) is FoundationModelsBackendBundle)
    }

    // MARK: validate_meta

    @Test func anOffDeviceFieldIsRefusedAtTheWrite() {
        let b = FoundationModelsBackendBundle(engine: FakeEngine())
        for key in ["endpoint", "model", "credentials"] {
            #expect(
                b.validateMeta([key: "x", "memory_store_id": "memory"], record: [:])
                    == "foundation_models_backend: `\(key)` has no meaning here — the model is "
                    + "Apple's on-device system model (no endpoint, no model choice, no credentials)")
        }
        // a null clears nothing that was never there — not a refusal
        #expect(b.validateMeta(["model": .null, "memory_store_id": "memory"], record: [:]) == nil)
    }

    @Test func theKnobsAreRefusedOutsideTheirShape() {
        let b = FoundationModelsBackendBundle(engine: FakeEngine(contextSize: 4096))
        let ok: OrderedDictionary<String, JSON> = ["memory_store_id": "memory"]
        func check(_ meta: OrderedDictionary<String, JSON>) -> String? {
            var m = ok
            for (k, v) in meta { m[k] = v }
            return b.validateMeta(m, record: [:])
        }
        #expect(check(["temperature": .double(-0.1)]) == "temperature must be a number >= 0")
        #expect(check(["temperature": "hot"]) == "temperature must be a number >= 0")
        #expect(check(["temperature": .bool(true)]) == "temperature must be a number >= 0")
        #expect(check(["temperature": 0]) == nil)
        #expect(check(["temperature": .double(0.7)]) == nil)
        #expect(check(["max_tokens": 0]) == "max_tokens must be a positive int")
        #expect(check(["max_tokens": .double(5.5)]) == "max_tokens must be a positive int")
        #expect(check(["max_tokens": 256]) == nil)
        let tooBig =
            "context_window must be a positive int no larger than the on-device model's window "
            + "(4096 tokens)"
        #expect(check(["context_window": 8192]) == tooBig)
        #expect(check(["context_window": 0]) == tooBig)
        #expect(check(["context_window": 2048]) == nil)
        #expect(check(["context_window": 4096]) == nil)
    }

    @Test func aRecordWithoutAStoreIsRefusedNamingIt() {
        let b = FoundationModelsBackendBundle(engine: FakeEngine())
        #expect(
            b.validateMeta([:], record: [:])
                == "foundation_models_backend: required on the record and missing: "
                + "memory_store_id — nothing is guessed. memory_store_id = the memory store "
                + "agent that keeps this agent's conversation (e.g. a ram_store)")
    }

    // MARK: reflect

    @Test func reflectShowsAvailabilityTheWindowAndTheKnobs() async {
        let engine = FakeEngine(contextSize: 4096)
        let (k, created) = await kernel(engine, meta: ["temperature": .double(0.3)])
        #expect(created["id"] == "fm", "\(created)")
        var r = await k.send([Item(AgentId("fm"), ["type": "reflect"])])[0].result
        #expect(
            r["sentence"].asString
                == "Apple Foundation Models LLM agent — the on-device system model (raw "
                + "prompt-and-parse tool-calling; no endpoint, no model choice, no key).")
        #expect(r["model"].isNull && r["endpoint"].isNull, "nothing off-device on the record")
        #expect(r["context_window"] == .integer(4096), "the model's window, not 128k")
        #expect(r["available"] == .bool(true))
        #expect(r["unavailable_reason"].isNull)
        #expect(r["context_size"] == .integer(4096))
        #expect(r["temperature"] == .double(0.3))
        #expect(r["max_tokens"].isNull)
        // the extras ride AFTER the shared base, in this order
        let keys = r.asObject?.keys.map { $0 } ?? []
        let tail = ["available", "unavailable_reason", "context_size", "temperature", "max_tokens"]
        if let i = keys.firstIndex(of: "available") {
            #expect(Array(keys[i..<min(i + tail.count, keys.count)]) == tail, "\(keys)")
            #expect((keys.firstIndex(of: "generating") ?? keys.count) < i, "\(keys)")
            // how to CALL it is `docs`; what it IS carries no verb docs or notes
            #expect(!keys.contains("concurrency") && !keys.contains("verbs"), "\(keys)")
        } else {
            Issue.record("no `available` in \(keys)")
        }

        engine.setAvailability(.unavailable(reason: FMReason.modelNotReady))
        r = await k.send([Item(AgentId("fm"), ["type": "reflect"])])[0].result
        #expect(r["available"] == .bool(false))
        #expect(r["unavailable_reason"] == .string(FMReason.modelNotReady))
    }

    @Test func aSmallerRecordWindowWins() async {
        let (k, _) = await kernel(FakeEngine(contextSize: 4096), meta: ["context_window": 3000])
        let r = await k.send([Item(AgentId("fm"), ["type": "reflect"])])[0].result
        #expect(r["context_window"] == .integer(3000))
    }

    // MARK: messages -> one request

    @Test func systemTextBecomesInstructionsAndTheLastUserTurnThePrompt() throws {
        let r = try FoundationModelsProvider.request(
            [
                content("system", "you are fm"), content("user", "hi"), content("assistant", "hello"),
                content("system", "late note"), content("user", "again"),
            ], temperature: 0.2, maxTokens: 64)
        #expect(
            r
                == FMRequest(
                    instructions: "you are fm\n\nlate note",
                    turns: [.init(role: "user", content: "hi"), .init(role: "assistant", content: "hello")],
                    prompt: "again", temperature: 0.2, maxTokens: 64))
    }

    @Test func aListNotEndingInAUserTurnIsRefused() {
        func message(_ m: [JSON]) -> String? {
            do { _ = try FoundationModelsProvider.request(m, temperature: nil, maxTokens: nil) } catch {
                return "\(error)"
            }
            return nil
        }
        #expect(
            message([content("user", "a"), content("assistant", "b")])
                == "RuntimeError: foundation_models_backend: the last message is not a user turn "
                + "(role assistant) — the on-device session answers a prompt")
        #expect(
            message([content("system", "s")])
                == "RuntimeError: foundation_models_backend: the last message is not a user turn "
                + "(role none) — the on-device session answers a prompt")
    }

    @Test func aSideCallsBoundOverridesTheRecordsAndThePromptCountIsReported() async throws {
        let engine = FakeEngine(script: [[.text("sum"), .text("mary"), .promptTokens(321)]])
        let p = FoundationModelsProvider(engine: engine, temperature: nil, maxTokens: 900)
        var text = ""
        for try await chunk in p.chat(messages: [content("user", "x")], maxTokens: 50) {
            if case .token(let t) = chunk { text += t }
        }
        #expect(text == "summary")
        #expect(engine.requests.first?.maxTokens == 50)
        #expect(p.usage?.lastPromptTokens == 321)
        #expect(p.usage?.lastStopReason == nil, "FM reports no stop reason; none is claimed")
    }

    @Test func aGenerationFailureIsRaisedUnderTheBundlesName() async {
        let engine = FakeEngine(
            script: [[.text("par")]],
            failure: FMError("the on-device model failed: The session's transcript exceeded the model's context size."))
        let p = FoundationModelsProvider(engine: engine, temperature: nil, maxTokens: nil)
        var text = ""
        do {
            for try await chunk in p.chat(messages: [content("user", "x")]) {
                if case .token(let t) = chunk { text += t }
            }
            Issue.record("must throw")
        } catch {
            #expect(
                "\(error)"
                    == "RuntimeError: foundation_models_backend: the on-device model failed: The "
                    + "session's transcript exceeded the model's context size.")
        }
        #expect(text == "par")
    }

    // MARK: a whole turn through the kernel

    @Test func aSendStreamsTheEnginesTextAndReplaysTheConversationNextTurn() async throws {
        let engine = FakeEngine(
            contextSize: 32768,
            script: [
                [.text("first "), .text("<call name=\"done\"></call>"), .promptTokens(1200)],
                [.text("second <call name=\"done\"></call>")],
            ])
        let (k, created) = await kernel(engine, meta: ["temperature": .double(0.1), "max_tokens": 300])
        #expect(created["id"] == "fm", "\(created)")
        let one = await send(k, "hi")
        #expect(one["response"].asString?.hasPrefix("first ") == true, "\(one)")
        let req0 = try #require(engine.requests.first)
        #expect(!req0.instructions.isEmpty, "the system block is the instructions")
        #expect(req0.turns.isEmpty)
        #expect(req0.prompt == "hi")
        #expect(req0.temperature == 0.1 && req0.maxTokens == 300)

        let two = await send(k, "again")
        #expect(two["response"].asString?.hasPrefix("second") == true, "\(two)")
        #expect(engine.requests.count == 2, "\(engine.requests.count)")
        let req1 = engine.requests[1]
        #expect(req1.turns.map(\.role) == ["user", "assistant"], "\(req1.turns)")
        #expect(req1.turns.first?.content == "hi")
        #expect(req1.turns.last?.content.hasPrefix("first ") == true)
        #expect(req1.prompt == "again")
    }

    @Test func aLeanSystemPromptTurnRunsInTheRealOnDeviceWindow() async {
        // 4096 tokens, the window the SDK reports on macOS 27.0: a caller's own
        // short system block is the instructions, verbatim, and the turn runs.
        let engine = FakeEngine(contextSize: 4096)
        let (k, _) = await kernel(engine)
        let r = await send(k, "hi", ["system_prompt": "Be terse."])
        #expect(r["response"].asString?.hasPrefix("ok ") == true, "\(r)")
        #expect(engine.requests.map(\.instructions) == ["Be terse."])
    }

    @Test func theDefaultSystemBlockFitsTheOnDeviceWindow() async {
        // With the verb docs out of the reflects, the kernel's default system block fits the 4096-token on-device
        // window — the turn runs with no `system_prompt`. Rust
        // `the_default_system_block_fits_the_on_device_window`.
        let engine = FakeEngine(contextSize: 4096)
        let (k, _) = await kernel(engine)
        let r = await send(k, "hi")
        #expect(r["response"].asString?.hasPrefix("ok ") == true, "\(r)")
        #expect(!engine.requests.isEmpty, "the model is called")
    }

    @Test func aSendOnAnUnavailableModelFailsLoudNamingTheReason() async {
        let engine = FakeEngine(availability: .unavailable(reason: FMReason.appleIntelligenceNotEnabled))
        let (k, created) = await kernel(engine)
        #expect(created["id"] == "fm", "creating the agent needs no model")
        let r = await send(k, "hi")
        #expect(
            r["error"]
                == .string(
                    "RuntimeError: foundation_models_backend: the on-device model is unavailable: "
                        + FMReason.appleIntelligenceNotEnabled), "\(r)")
        #expect(engine.requests.isEmpty, "nothing reached the engine — and nothing else either")
    }

    // MARK: the readme

    @Test func theReadmeIsTheOneResourceFile() throws {
        let file = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent(
                "Sources/FantasticFoundationModelsBackend/Resources/readme.md")
        let text = try String(contentsOf: file, encoding: .utf8)
        #expect(FoundationModelsBackendBundle(engine: FakeEngine()).readme == text)
        #expect(text.hasPrefix("# foundation_models_backend — "))
    }
}
