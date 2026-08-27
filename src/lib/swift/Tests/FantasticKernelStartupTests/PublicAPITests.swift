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
        let listed = await kernel.send([Item(AgentId("core"), .object(["type": .string("list_agents")]))])[0].result
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("core"))
        #expect(ids.contains("web"))
    }

    @Test func registersDefaultBundleSet() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        // Spot-check key bundles are registered + dispatchable.
        for hm in ["file_bridge.tools", "proxy_agent.tools", "tools.tools", "ollama_backend.tools"]
        {
            let id = "probe_\(hm.replacingOccurrences(of: ".", with: "_"))"
            let r = await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("create_agent"),
                    "parent_id": .string("core"),
                    "handler_module": .string(hm),
                    "id": .string(id),
                ]))])[0].result
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
            await kernel.send([Item(AgentId("core"), .object(["type": .string("reflect")]))])[0].result[
                "self_extension"
            ].asString)
        let catalog = await kernel.send([Item(AgentId("core"), .object(["type": .string("reflect"), "bundles": .string("all")]))])[0].result
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
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("ollama_backend.tools"),
                "id": .string("ollama_nomodel"),
            ]))])[0].result
        let r = await kernel.send([Item(AgentId("ollama_nomodel"), .object(["type": .string("reflect")]))])[0].result
        #expect(r["model"].isNull, "ollama must not guess a model default: \(r)")
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

// The `Kernel.sendJson` / `sendJsonAs` / `proxyEmit` / `registerTool` suites are
// GONE with the UniFFI-compat surface they covered. Nothing is untested as a
// result: sender attribution is pinned by `sendAsAttributesSender` and emit's
// state event by `emitPublishesStateEvent`, both in KernelDispatchTests, against
// the native API instead of the string shim.

@Suite("save / load")
struct SaveLoadTests {
    @Test func saveReturnsValidJson() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let json = kernel.save()
        let parsed = try JSON.parse(json)
        #expect(parsed["version"].asInt == 1)
        #expect(!parsed["agents"].asArray.isNilOrEmpty)
    }

    @Test func loadRoundTrip() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("file_bridge.tools"),
                "id": .string("snap_target"),
            ]))])[0].result
        let snap = kernel.save()
        // Make a fresh kernel + load.
        let kernel2 = try await startKernelInMemory(portHint: 0)
        try kernel2.load(json: snap)
        let listed = await kernel2.send([Item(AgentId("core"), .object(["type": .string("list_agents")]))])[0].result
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
