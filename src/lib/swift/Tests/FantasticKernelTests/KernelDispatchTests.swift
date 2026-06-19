// Kernel.send / emit / subscribe + system verb tests.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

/// Echo bundle — verb dispatch returns the payload echoed back
/// with `{type: "echoed", original: payload}`.
struct EchoBundle: AgentBundle {
    let name = "echo"

    func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        return .object([
            "type": .string("echoed"),
            "original": payload,
        ])
    }
}

/// Captures every state event for assertions.
final class EventCapture: @unchecked Sendable {
    private let lock = NSLock()
    private var events: [JSON] = []

    func handler() -> @Sendable (JSON) -> Void {
        return { [weak self] ev in
            self?.lock.lock()
            self?.events.append(ev)
            self?.lock.unlock()
        }
    }

    func snapshot() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }
}

func makeKernel(withEcho: Bool = true) -> Kernel {
    let registry = BundleRegistry()
    if withEcho {
        registry.register("echo.tools", EchoBundle())
    }
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

@Suite("Kernel.send dispatch")
struct KernelSendTests {
    @Test func sendToBareAgentBoot() async {
        let kernel = makeKernel()
        let reply = await kernel.send("core", ["type": "boot"])
        // Bare agent's boot returns null per substrate convention.
        #expect(reply.isNull)
    }

    @Test func sendToBundleAgentEchoes() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "echo1",
            ])
        let reply = await kernel.send("echo1", ["type": "say", "text": "hi"])
        #expect(reply["type"].asString == "echoed")
        #expect(reply["original"]["text"].asString == "hi")
    }

    @Test func sendToMissingAgentReturnsError() async {
        let kernel = makeKernel()
        let reply = await kernel.send("ghost", ["type": "anything"])
        #expect(reply["error"].asString?.contains("no agent ghost") == true)
    }

    @Test func sendUnknownHandlerModuleReturnsError() async {
        let kernel = makeKernel(withEcho: false)
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "missing.tools",
                "id": "x",
            ])
        let reply = await kernel.send("x", ["type": "say"])
        #expect(reply["error"].asString?.contains("missing.tools") == true)
    }

    @Test func kernelAliasResolvesToRoot() async {
        let kernel = makeKernel()
        let reply = await kernel.send("kernel", ["type": "reflect"])
        #expect(reply["id"].asString == "core")
    }
}

@Suite("Kernel.emit")
struct KernelEmitTests {
    @Test func emitDeliversToInbox() async {
        let kernel = makeKernel()
        let stream = kernel.register(
            Agent(id: "listener", handlerModule: nil, parentId: nil))
        await kernel.emit("listener", ["type": "ping", "x": 1])

        var iterator = stream.makeAsyncIterator()
        let first = await iterator.next()
        #expect(first?["type"].asString == "ping")
        #expect(first?["x"].asInt == 1)
    }

    @Test func emitPublishesStateEvent() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        await kernel.emit("core", ["type": "thing"])
        let evs = cap.snapshot()
        #expect(evs.contains { $0["type"].asString == "emit" && $0["target"].asString == "core" })
    }
}

@Suite("Kernel.subscribe state events")
struct KernelSubscribeTests {
    @Test func sendPublishesStateEvent() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        _ = await kernel.send("core", ["type": "list_agents"])
        let evs = cap.snapshot()
        let sendEvents = evs.filter { $0["type"].asString == "send" }
        #expect(!sendEvents.isEmpty)
        #expect(sendEvents.last?["verb"].asString == "list_agents")
    }

    @Test func unsubscribeStopsDelivery() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        let token = kernel.subscribe(cap.handler())
        _ = await kernel.send("core", ["type": "list_agents"])
        let before = cap.snapshot().count
        kernel.unsubscribe(token)
        _ = await kernel.send("core", ["type": "list_agents"])
        let after = cap.snapshot().count
        #expect(after == before)
    }

    @Test func sendAsAttributesSender() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        _ = await kernel.sendAs(
            sender: "ui", target: "core", payload: ["type": "list_agents"])
        let evs = cap.snapshot()
        let sends = evs.filter { $0["type"].asString == "send" }
        #expect(sends.last?["sender"].asString == "ui")
    }
}

@Suite("System verbs")
struct SystemVerbTests {
    @Test func listAgents() async {
        let kernel = makeKernel()
        let reply = await kernel.send("core", ["type": "list_agents"])
        let agents = reply["agents"].asArray ?? []
        #expect(agents.count == 1)
        #expect(agents[0]["id"].asString == "core")
    }

    @Test func createAgent() async {
        let kernel = makeKernel()
        let reply = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "alpha",
            ])
        #expect(reply["id"].asString == "alpha")
        #expect(reply["handler_module"].asString == "echo.tools")

        let listed = await kernel.send("core", ["type": "list_agents"])
        let names = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(names.contains("alpha"))
    }

    @Test func createAgentRequiresHandlerModule() async {
        let kernel = makeKernel()
        let reply = await kernel.send(
            "core", ["type": "create_agent", "id": "x"])
        #expect(reply["error"].asString?.contains("handler_module") == true)
    }

    @Test func createAgentRefusesDuplicate() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "dup",
            ])
        let second = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "dup",
            ])
        #expect(second["error"].asString?.contains("already exists") == true)
    }

    @Test func deleteAgent() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "doomed",
            ])
        let reply = await kernel.send(
            "core", ["type": "delete_agent", "id": "doomed"])
        #expect(reply["deleted"].asBool == true)
        let listed = await kernel.send("core", ["type": "list_agents"])
        let names = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(!names.contains("doomed"))
    }

    @Test func deleteAgentRefusedWhenLocked() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "locked",
                "delete_lock": true,
            ])
        let reply = await kernel.send(
            "core", ["type": "delete_agent", "id": "locked"])
        #expect(reply["locked"].asBool == true)
        #expect(reply["blocked_by"].asString == "locked")
    }

    @Test func deleteCascadesToChildren() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "parent",
            ])
        _ = await kernel.send(
            "parent",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "child",
            ])
        _ = await kernel.send(
            "core", ["type": "delete_agent", "id": "parent"])
        #expect(kernel.agent("parent") == nil)
        #expect(kernel.agent("child") == nil)
    }

    @Test func updateAgent() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "u1",
                "display_name": "Original",
            ])
        let updated = await kernel.send(
            "core",
            [
                "type": "update_agent",
                "id": "u1",
                "display_name": "Renamed",
                "color": "#ff00ff",
            ])
        #expect(updated["display_name"].asString == "Renamed")
        #expect(updated["color"].asString == "#ff00ff")
    }

    @Test func getAgentReturnsRecord() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "g1",
            ])
        let reply = await kernel.send(
            "core", ["type": "get", "id": "g1"])
        #expect(reply["id"].asString == "g1")
        #expect(reply["handler_module"].asString == "echo.tools")
    }
}
