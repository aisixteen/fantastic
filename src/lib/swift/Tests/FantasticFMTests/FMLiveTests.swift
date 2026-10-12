// THE ONE LIVE TEST — the real on-device model, opt-in: `FM_LIVE=1 swift test
// --filter liveOnDeviceModel`. Skipped otherwise, so the default suite never
// needs Apple Intelligence. On-device and free; one run takes seconds.
//
// It walks the whole stack once with tiny prompts: FMCore's availability and
// window, a stream over a replayed transcript, the C ABI the rust bundle links,
// a cancel mid-stream, and one turn through a kernel's foundation_models_backend
// agent. Every step prints a `[FM_LIVE]` line with what it measured.
//
// An extension of the SERIALIZED bridge suite: the C ABI's engine is
// process-wide, and this test must hold the real one while it runs.

import FantasticFMCore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticFMBridge
@testable import FantasticFoundationModelsBackend
@testable import FantasticRamStore

private func secs(_ t: Date) -> String { String(format: "%.2fs", Date().timeIntervalSince(t)) }

extension FMBridgeTests {

    @Test(.enabled(if: ProcessInfo.processInfo.environment["FM_LIVE"] == "1"))
    func liveOnDeviceModel() async throws {
        let engine = SystemFMEngine()

        // 1. availability + the window, from the SDK
        let availability = engine.availability()
        print(
            "[FM_LIVE] availability=\(availability.json) context_size=\(engine.contextSize) "
                + "source=\(engine.contextSizeSource.rawValue)")
        try #require(availability == .available, "the live test needs the model: \(availability)")
        #expect(engine.contextSize > 0)
        #expect(engine.contextSizeSource == .sdk)

        // 2. a stream over a REPLAYED transcript — the prior turns reach the model
        var t0 = Date()
        var text = ""
        var promptTokens: Int? = nil
        var deltas = 0
        for try await e in engine.stream(
            FMRequest(
                instructions: "Answer with the name only.",
                turns: [
                    .init(role: "user", content: "My name is Zed. Remember it."),
                    .init(role: "assistant", content: "Noted."),
                ],
                prompt: "What is my name?", temperature: 0, maxTokens: 16))
        {
            switch e {
            case .text(let d):
                text += d
                deltas += 1
            case .promptTokens(let n): promptTokens = n
            }
        }
        print(
            "[FM_LIVE] transcript: text=\(text.debugDescription) deltas=\(deltas) "
                + "prompt_tokens=\(promptTokens.map(String.init) ?? "nil") \(secs(t0))")
        #expect(text.contains("Zed"))
        if #available(macOS 27.0, *) { #expect(promptTokens != nil) }

        // 3. the C ABI the rust bundle links
        await with(engine) {
            let c = Collector()
            t0 = Date()
            _ = stream(
                #"{"instructions":"Answer with one word.","turns":[],"prompt":"Reply with the word pong.","temperature":0,"max_tokens":16}"#,
                c)
            #expect(await eventually(60) { c.doneCount == 1 })
            print(
                "[FM_LIVE] C ABI: text=\(c.text.debugDescription) error=\(c.error ?? "nil") "
                    + "prompt_tokens=\(c.promptTokens) chunks=\(c.chunks) \(secs(t0))")
            #expect(c.error == nil)
            #expect(c.text.lowercased().contains("pong"))
        }

        // 4. cancel mid-stream: the generation stops, nothing hangs
        t0 = Date()
        let firstDelta = Collector()
        let run = Task { () -> (String, String) in
            var n = 0
            var got = ""
            do {
                for try await e in engine.stream(
                    FMRequest(
                        instructions: "", turns: [],
                        prompt: "Count from 1 to 300, one number per line."))
                {
                    if case .text(let d) = e {
                        n += 1
                        got += d
                        if n == 1 { firstDelta.done(nil, 0) }
                    }
                }
                return ("ended after \(n) deltas", got)
            } catch is CancellationError {
                return ("cancelled after \(n) deltas", got)
            } catch {
                return ("error \(error)", got)
            }
        }
        #expect(await eventually(60) { firstDelta.doneCount == 1 })
        run.cancel()
        let (outcome, counted) = await run.value
        print("[FM_LIVE] cancel: \(outcome), \(counted.utf8.count) bytes \(secs(t0))")
        #expect(!outcome.hasPrefix("error"), "\(outcome)")
        #expect(!counted.contains("300"), "the count was stopped, not finished")

        // 5. one turn through a kernel's agent — a lean system_prompt, because
        //    the default system block does not fit the on-device window
        let registry = BundleRegistry()
        registry.register(HANDLER_MODULE, FoundationModelsBackendBundle(engine: engine))
        registry.register("ram_store.tools", RamStoreBundle())
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(
            id: AgentId("kernel_state"), handlerModule: nil, parentId: nil, meta: ["guid": "g-live"])
        k.register(root)
        k.setRoot(root)
        // a call-less turn gives up at once instead of reminding the model
        AISilentStreaks.setCap(k, 1)
        _ = await k.send([
            Item(
                AgentId("kernel_state"),
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "ram_store.tools", "id": "memory", "callable_by": ["fm": "*"],
                ])
        ])
        _ = await k.send([
            Item(
                AgentId("kernel_state"),
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": .string(HANDLER_MODULE), "id": "fm",
                    "memory_store_id": "memory", "can_call": ["memory": "*", "cli": "*"],
                    "temperature": 0, "max_tokens": 32,
                ])
        ])
        let reflect = await k.send([Item(AgentId("fm"), ["type": "reflect"])])[0].result
        print(
            "[FM_LIVE] reflect: available=\(reflect["available"].serialize()) "
                + "context_window=\(reflect["context_window"].serialize()) "
                + "context_size=\(reflect["context_size"].serialize())")
        #expect(reflect["available"] == .bool(true))
        #expect(reflect["context_window"] == .integer(Int64(engine.contextSize)))
        t0 = Date()
        let r = await k.send([
            Item(
                AgentId("fm"),
                [
                    "type": "send", "text": "Say hello in one word.",
                    "system_prompt": "You are terse. Answer in one word, then write <call name=\"done\"></call>",
                ])
        ])[0].result
        let status = await k.send([Item(AgentId("fm"), ["type": "context_status"])])[0].result
        print(
            "[FM_LIVE] kernel send: \(r.serialize()) reported_prompt_tokens="
                + "\(status["reported_prompt_tokens"].serialize()) \(secs(t0))")
        #expect(r["error"].isNull, "\(r)")
        #expect(!(r["response"].asString ?? "").isEmpty, "\(r)")
        await k.shutdown()
    }
}
