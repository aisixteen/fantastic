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
        let reply = await kernel.send([Item("core", ["type": "boot"])])[0].result
        // Bare agent's boot returns null per substrate convention.
        #expect(reply.isNull)
    }

    @Test func sendToBundleAgentEchoes() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "echo1",
            ])])[0].result
        let reply = await kernel.send([Item("echo1", ["type": "say", "text": "hi"])])[0].result
        #expect(reply["type"].asString == "echoed")
        #expect(reply["original"]["text"].asString == "hi")
    }

    @Test func sendToMissingAgentNamesTheLegalIds() async {
        // A bare `no agent 'x'` teaches the caller nothing it can act on.
        // Measured 2026-08-04: a model copied the literal placeholder `<id>` out
        // of a prompt example, got the same refusal 34 times in one run, and
        // never corrected. The call still fails exactly once, by the one path —
        // it just says what the legal ids are on the way out.
        let kernel = makeKernel()
        let out = await kernel.send([Item("ghost", ["type": "anything"])])[0]
        #expect(out.status == .failed)
        #expect(out.pid == nil, "a refusal never reached the task table")
        #expect(out.result["error"].asString?.contains("no agent") == true)
        #expect(out.result["reason"].asString == "unknown_agent")
        #expect(out.result["agents"].asArray?.contains(.string("core")) == true)
    }

    @Test func sendUnknownHandlerModuleReturnsError() async {
        let kernel = makeKernel(withEcho: false)
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "missing.tools",
                "id": "x",
            ])])[0].result
        let reply = await kernel.send([Item("x", ["type": "say"])])[0].result
        #expect(reply["error"].asString?.contains("missing.tools") == true)
    }

    @Test func kernelAliasResolvesToRoot() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("kernel", ["type": "reflect"])])[0].result
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
        _ = await kernel.send([Item("core", ["type": "list_agents"])])[0].result
        let evs = cap.snapshot()
        let sendEvents = evs.filter { $0["type"].asString == "send" }
        #expect(!sendEvents.isEmpty)
        #expect(sendEvents.last?["verb"].asString == "list_agents")
    }

    @Test func unsubscribeStopsDelivery() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        let token = kernel.subscribe(cap.handler())
        _ = await kernel.send([Item("core", ["type": "list_agents"])])[0].result
        let before = cap.snapshot().count
        kernel.unsubscribe(token)
        _ = await kernel.send([Item("core", ["type": "list_agents"])])[0].result
        let after = cap.snapshot().count
        #expect(after == before)
    }

    @Test func sendAsAttributesSender() async {
        let kernel = makeKernel()
        // Seed a real `ui` agent as a CHILD of the root: every verb is gated now
        // (reflect included), so only a PERMITTED send fires the state event whose
        // sender attribution is under test. A child reflecting its parent is covered
        // by the structural baseline on both ends — no explicit grant needed.
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "echo.tools", "id": "ui"])])[0].result
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        _ = await kernel.sendAs(
            sender: "ui", target: "core", payload: ["type": "reflect"])
        let evs = cap.snapshot()
        let sends = evs.filter { $0["type"].asString == "send" }
        #expect(sends.last?["sender"].asString == "ui")
    }
}

@Suite("System verbs")
struct SystemVerbTests {
    @Test func listAgents() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("core", ["type": "list_agents"])])[0].result
        let agents = reply["agents"].asArray ?? []
        #expect(agents.count == 1)
        #expect(agents[0]["id"].asString == "core")
    }

    @Test func createAgent() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "alpha",
            ])])[0].result
        #expect(reply["id"].asString == "alpha")
        #expect(reply["handler_module"].asString == "echo.tools")

        let listed = await kernel.send([Item("core", ["type": "list_agents"])])[0].result
        let names = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(names.contains("alpha"))
    }

    @Test func createAgentRequiresHandlerModule() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "id": "x"])])[0].result
        #expect(reply["error"].asString?.contains("handler_module") == true)
    }

    @Test func createAgentRefusesDuplicate() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "dup",
            ])])[0].result
        let second = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "dup",
            ])])[0].result
        #expect(second["error"].asString?.contains("already exists") == true)
    }

    @Test func deleteAgent() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "doomed",
            ])])[0].result
        let reply = await kernel.send([Item("core", ["type": "delete_agent", "id": "doomed"])])[0].result
        #expect(reply["deleted"].asBool == true)
        let listed = await kernel.send([Item("core", ["type": "list_agents"])])[0].result
        let names = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(!names.contains("doomed"))
    }

    @Test func deleteAgentRefusedWhenLocked() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "locked",
                "delete_lock": true,
            ])])[0].result
        let reply = await kernel.send([Item("core", ["type": "delete_agent", "id": "locked"])])[0].result
        // #27: the machine field, not the ad-hoc flag it replaced.
        #expect(reply["reason"].asString == "locked")
        #expect(reply["blocked_by"].asString == "locked")
    }

    @Test func deleteCascadesToChildren() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "parent",
            ])])[0].result
        _ = await kernel.send([Item("kernel", [
                "type": "create_agent",
                "parent_id": "parent",
                "handler_module": "echo.tools",
                "id": "child",
            ])])[0].result
        _ = await kernel.send([Item("core", ["type": "delete_agent", "id": "parent"])])[0].result
        #expect(kernel.agent("parent") == nil)
        #expect(kernel.agent("child") == nil)
    }

    @Test func updateAgent() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "u1",
                "display_name": "Original",
            ])])[0].result
        let updated = await kernel.send([Item("core", [
                "type": "update_agent",
                "id": "u1",
                "display_name": "Renamed",
                "color": "#ff00ff",
            ])])[0].result
        // {updated, id, agent} — python's shape; the record rides under `agent`.
        #expect(updated["updated"].asBool == true)
        #expect(updated["id"].asString == "u1")
        #expect(updated["agent"]["display_name"].asString == "Renamed")
        #expect(updated["agent"]["color"].asString == "#ff00ff")
    }

    @Test func getIsNotAKernelVerb() async {
        // It was, in rust and swift only — python has no `get` and answers the
        // root bundle's UNHANDLED refusal. A record is read with `list_agents`
        // or a `reflect`, both of which every runtime has.
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "echo.tools",
                "id": "g1",
            ])])[0].result
        let reply = await kernel.send([Item("core", ["type": "get", "id": "g1"])])[0].result
        #expect(reply["reason"].asString == "unhandled", "\(reply.serialize())")
        #expect(reply["verb"].asString == "get", "the refusal names the verb")
        #expect(reply["id"].asString == nil, "it must not answer with a record")
    }
}
