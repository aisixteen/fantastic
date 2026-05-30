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
        let listed = await kernel.send(
            AgentId("core"), .object(["type": .string("list_agents")]))
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("core"))
        #expect(ids.contains("web"))
    }

    @Test func registersDefaultBundleSet() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        // Spot-check key bundles are registered + dispatchable.
        for hm in ["file.tools", "proxy_agent.tools", "tools.tools", "ollama_backend.tools"] {
            let id = "probe_\(hm.replacingOccurrences(of: ".", with: "_"))"
            let r = await kernel.send(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"),
                    "handler_module": .string(hm),
                    "id": .string(id),
                ]))
            #expect(r["id"].asString == id, "failed to create agent for \(hm): \(r)")
        }
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
                lock.lock(); defer { lock.unlock() }
                return events
            }
        }
        let cap = Capture()
        _ = kernel.subscribe(listener: cap)
        _ = await kernel.sendJsonAs(
            senderId: "weather_ui",
            targetId: "core",
            payloadJson: #"{"type":"list_agents"}"#
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
                lock.lock(); defer { lock.unlock() }
                events.append(eventJson)
            }
            func snapshot() -> [String] {
                lock.lock(); defer { lock.unlock() }
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
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))
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
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))
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
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))
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
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("tools.tools"),
                "id": .string("tools"),
            ]))
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
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("file.tools"),
                "id": .string("snap_target"),
            ]))
        let snap = kernel.save()
        // Make a fresh kernel + load.
        let kernel2 = try await startKernelInMemory(portHint: 0)
        try kernel2.load(json: snap)
        let listed = await kernel2.send(
            AgentId("core"),
            .object(["type": .string("list_agents")]))
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
