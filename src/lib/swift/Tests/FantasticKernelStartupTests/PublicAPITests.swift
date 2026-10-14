// 8A public API shim tests.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("startKernelInMemory")
struct StartKernelInMemoryTests {
    @Test func bootsWithCoreAndWebAgents() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let listed = await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("list_agents")]))])[0].result
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("kernel_state"))
        #expect(ids.contains("web"))
    }

    @Test func registersDefaultBundleSet() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        // Spot-check key bundles are registered + dispatchable.
        for hm in ["file_bridge.tools", "cli.tools", "ollama_backend.tools"]
        {
            let id = "probe_\(hm.replacingOccurrences(of: ".", with: "_"))"
            // An AI backend's required record fields are refused at the write when
            // absent (its schema); stub values — nothing here dials the provider.
            let payload: JSON =
                hm == "ollama_backend.tools"
                ? .object([
                    "type": .string("create_agent"),
                    "parent_id": .string("kernel_state"),
                    "handler_module": .string(hm),
                    "id": .string(id),
                    "endpoint": .string("http://127.0.0.1:9/stub"),
                    "model": .string("stub"),
                    "memory_store_id": .string("memory"),
                ])
                : .object([
                    "type": .string("create_agent"),
                    "parent_id": .string("kernel_state"),
                    "handler_module": .string(hm),
                    "id": .string(id),
                ])
            let r = await kernel.send([Item(AgentId("kernel_state"), payload)])[0].result
            #expect(r["id"].asString == id, "failed to create agent for \(hm): \(r)")
        }
    }

    /// The SHIPPED registry's self_extension, not a synthetic one. A hard-coded
    /// executor name was wrong in both directions — a conditionally-compiled
    /// executor went unnamed, and on a build that did not register it the string
    /// named a module a caller cannot use. Assert against what THIS build
    /// registered: every executor in the table is named iff it is registered.
    @Test func selfExtensionNamesTheExecutorsThisBuildShips() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let s = try #require(
            await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect")]))])[0].result[
                "self_extension"
            ].asString)
        let catalog = await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect"), "bundles": .string("all")]))])[0].result
        let registered = Set(
            (catalog["bundles"].asArray ?? []).compactMap { $0["handler_module"].asString })
        // Every `<something>.tools` module the self_extension NAMES must actually be
        // registered in this build — advertising a module a caller cannot use is the
        // whole defect this derivation exists to prevent.
        let named = s.split(whereSeparator: { " \t\n,;:'\"`()[]".contains($0) })
            .map(String.init)
            .filter { $0.hasSuffix(".tools") }
        #expect(!named.isEmpty, "the self_extension must name this build's executor: \(s)")
        for module in named {
            #expect(
                registered.contains(module),
                "self_extension names \(module), which this build does not register: \(s)")
        }
        #expect(!s.contains("NO script executor"), "\(s)")
    }

    @Test func ollamaModelHasNoGuessedDefault() async throws {
        // "By default, no defaults": an ollama agent with no `model` set must NOT
        // fall back to a guessed model (a wrong guess reaches the network and 404s).
        // Reflect surfaces `null`, not a baked-in default like "llama3.2".
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("kernel_state"),
                "handler_module": .string("ollama_backend.tools"),
                "id": .string("ollama_nomodel"),
            ]))])[0].result
        let r = await kernel.send([Item(AgentId("ollama_nomodel"), .object(["type": .string("reflect")]))])[0].result
        // Python answers `rec.get("model")` — no model on the record is
        // null, never a guessed one.
        #expect(r["model"].isNull, "ollama must not guess a model default: \(r)")
        #expect(r["endpoint"].isNull, "ollama must not guess an endpoint: \(r)")
    }

    @Test func httpPortIsZeroPreListener() async throws {
        // `httpPort()` defaults to 0 until a WebServer binds. Use a
        // bare kernel here: `startKernelInMemory` creates a `web`
        // agent, and `create_agent` auto-fires `boot` (canonical —
        // Python's create_agent wraps create with a boot send), so
        // web would already be listening on a real port.
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        #expect(kernel.httpPort() == 0)
    }
}

@Suite("save / load")
struct SaveLoadTests {
    @Test func saveReturnsValidJson() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let json = try kernel.saveJSON()
        let parsed = try JSON.parse(json)
        #expect(parsed["version"].asInt == 1)
        #expect(!parsed["records"].asArray.isNilOrEmpty)
    }

    @Test func loadRoundTrip() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("kernel_state"),
                "handler_module": .string("file_bridge.tools"), "allow": ["."],
                "id": .string("snap_target"),
            ]))])[0].result
        let snap = try kernel.saveJSON()
        // Make a fresh kernel + load.
        let kernel2 = try await startKernelInMemory(portHint: 0)
        try kernel2.loadJSON(snap)
        let listed = await kernel2.send([Item(AgentId("kernel_state"), .object(["type": .string("list_agents")]))])[0].result
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("snap_target"))
    }
}

extension Optional where Wrapped == [JSON] {
    fileprivate var isNilOrEmpty: Bool {
        switch self {
        case .none: return true
        case .some(let a): return a.isEmpty
        }
    }
}
