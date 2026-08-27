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

@Suite("Kernel.sendJson / sendJsonAs")
struct SendJsonTests {
    @Test func sendJsonRoundTrip() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let reply = await kernel.sendJson(
            targetId: "core",
            payloadJson: #"{"type":"list_agents"}"#
        )
        let parsed = try JSON.parse(reply)
        let ids = (parsed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("core"))
    }

    @Test func sendJsonReturnsErrorOnBadPayload() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let reply = await kernel.sendJson(
            targetId: "core",
            payloadJson: "{not valid json"
        )
        #expect(reply.contains("error"))
    }

    @Test func sendJsonAsAttributesSender() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        // Hook a listener to inspect state events.
        final class Capture: StateListener, @unchecked Sendable {
            let lock = NSLock()
            var events: [String] = []
            func onEvent(eventJson: String) {
                lock.lock()
                events.append(eventJson)
                lock.unlock()
            }
            func snapshot() -> [String] {
                lock.lock()
                defer { lock.unlock() }
                return events
            }
        }
        // Seed a real `weather_ui` agent as a CHILD of the root: every verb is gated
        // now (reflect included), so only a PERMITTED send fires the state event whose
        // sender attribution is under test. A child reflecting its parent is covered by
        // the structural baseline on both ends — no explicit grant needed.
        _ = await kernel.sendJson(
            targetId: "core",
            payloadJson: #"{"type":"create_agent","parent_id":"core","handler_module":"echo.tools","id":"weather_ui"}"#
        )
        let cap = Capture()
        _ = kernel.subscribe(listener: cap)
        _ = await kernel.sendJsonAs(
            senderId: "weather_ui",
            targetId: "core",
            payloadJson: #"{"type":"reflect"}"#
        )
        let sendEvents = cap.snapshot().compactMap { try? JSON.parse($0) }.filter {
            $0["type"].asString == "send"
        }
        #expect(sendEvents.last?["sender"].asString == "weather_ui")
    }
}

@Suite("Kernel.proxyEmit")
struct ProxyEmitTests {
    @Test func proxyEmitPublishesStateEvent() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        final class Capture: StateListener, @unchecked Sendable {
            let lock = NSLock()
            var events: [String] = []
            func onEvent(eventJson: String) {
                lock.lock()
                defer { lock.unlock() }
                events.append(eventJson)
            }
            func snapshot() -> [String] {
                lock.lock()
                defer { lock.unlock() }
                return events
            }
        }
        let cap = Capture()
        _ = kernel.subscribe(listener: cap)
        await kernel.proxyEmit(
            agentId: "core",
            eventJson: #"{"type":"focus_changed","focused":true}"#
        )
        let emits = cap.snapshot().compactMap { try? JSON.parse($0) }.filter {
            $0["type"].asString == "emit"
        }
        #expect(!emits.isEmpty)
    }
}

@Suite("Kernel.registerTool")
struct ToolShimsTests {
    @Test func registerToolRoundTrip() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        // Create the tools agent first.
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))])[0].result
        let reply = await kernel.registerTool(
            senderId: "weather",
            name: "get_weather",
            agentId: "weather_agent",
            verb: "lookup",
            description: "Return weather for a city.",
            parametersSchemaJson: #"{"type":"object","properties":{"city":{"type":"string"}}}"#
        )
        let parsed = try JSON.parse(reply)
        #expect(parsed["ok"].asBool == true)
        #expect(parsed["name"].asString == "get_weather")
    }

    @Test func listToolsForLlmReturnsRegistered() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))])[0].result
        _ = await kernel.registerTool(
            senderId: "x",
            name: "ping",
            agentId: "health",
            verb: nil,
            description: "Ping the health agent.",
            parametersSchemaJson: #"{"type":"object"}"#
        )
        let listReply = await kernel.listToolsForLlm()
        let parsed = try JSON.parse(listReply)
        let names = (parsed["tools"].asArray ?? []).compactMap { $0["name"].asString }
        #expect(names.contains("ping"))
    }

    @Test func unregisterToolDropsIt() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))])[0].result
        _ = await kernel.registerTool(
            senderId: "x", name: "t1", agentId: "a", verb: nil,
            description: "d", parametersSchemaJson: #"{}"#
        )
        let r = await kernel.unregisterTool(senderId: "x", name: "t1")
        let parsed = try JSON.parse(r)
        #expect(parsed["ok"].asBool == true)
    }

    @Test func unregisterToolsBySenderDropsAll() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))])[0].result
        _ = await kernel.registerTool(
            senderId: "auth", name: "a1", agentId: "x", verb: nil,
            description: "d", parametersSchemaJson: #"{}"#
        )
        _ = await kernel.registerTool(
            senderId: "auth", name: "a2", agentId: "x", verb: nil,
            description: "d", parametersSchemaJson: #"{}"#
        )
        let r = await kernel.unregisterToolsBySender(senderId: "auth")
        let parsed = try JSON.parse(r)
        #expect(parsed["ok"].asBool == true)
        #expect(parsed["removed"].asInt == 2)
    }
}

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
