// A NAMED execution's task-table handle IS the caller's id.
//
// Measured 2026-09-02: `send([Item(target, payload, id: "job-1")])` registered
// the dispatch under a freshly minted `<agent>:<hex>` pid, so `ps` showed an id
// the caller had never seen and `cancel task_id: "job-1"` answered
// `{cancelled:false, killed:[]}` — the caller could neither find nor stop the
// execution it had just named. Python registers named work under `exec_id`.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

private struct Slow: AgentBundle {
    let name = "slow"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        if payload["type"].asString == "work" {
            try await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["done": .bool(true)])
        }
        return .object(["ok": .bool(true)])
    }
}

@Suite("named execution")
struct NamedExecutionTests {

    private func freshKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("slow.tools", Slow())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    private func seed(_ kernel: Kernel, _ id: String) async {
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string(id), "handler_module": .string("slow.tools"),
                ]))
        ])[0].result
    }

    @Test func aNamedExecutionIsRegisteredUnderItsOwnId() async throws {
        let kernel = freshKernel()
        await seed(kernel, "w")
        let running = Task {
            await kernel.send([
                Item(AgentId("w"), .object(["type": .string("work")]), id: "job-1")
            ])[0]
        }
        try await Task.sleep(nanoseconds: 200_000_000)
        let ps = await kernel.send([
            Item(AgentId("kernel"), .object(["type": .string("ps")]))
        ])[0].result
        let ids = (ps["tasks"].asArray ?? []).compactMap { $0["id"].asInt }
        // The caller's NAME resolves to the int its execution runs under — that
        // int is the ps handle (python: one id space, names beside it).
        let jid = kernel.ledger.resolveName("job-1")
        #expect(jid != nil && ids.contains(jid!), "the name's id must be the ps handle: \(ids)")

        let c = await kernel.send([
            Item(
                AgentId("kernel"),
                .object(["type": .string("cancel"), "task_id": .string("job-1")]))
        ])[0].result
        #expect(c["cancelled"].asBool == true, "\(c.serialize())")
        #expect(c["killed"][0].asInt == kernel.ledger.resolveName("job-1"), "\(c.serialize())")

        let out = await running.value
        #expect(out.status == .cancelled, "\(out.result.serialize())")
        #expect(out.pid?.value == kernel.ledger.resolveName("job-1"), "the outcome's pid is the id the caller's name opened")
    }

    @Test func anUnnamedExecutionStillMintsAPid() async {
        let kernel = freshKernel()
        await seed(kernel, "w")
        let out = await kernel.send([Item(AgentId("w"), .object(["type": .string("ping")]))])[0]
        let pid = out.pid?.value ?? 0
        #expect(pid > 0, "a minted pid is an int from the one counter, got \(pid)")
    }

    /// C31: python `f"send: {d.target.id} {d.payload['type']!r} exceeded its
    /// {d.timeout}s timeout; task cancelled"`, carrying `task_id`.
    @Test func ownTimeoutTextIsPythonExact() async {
        let kernel = freshKernel()
        await seed(kernel, "w")
        var item = Item(AgentId("w"), .object(["type": .string("work")]))
        item.timeout = 0.05
        let out = await kernel.send([item])[0]
        #expect(out.status == .timedOut, "\(out.result.serialize())")
        #expect(
            out.result["error"].asString
                == "send: w 'work' exceeded its 0.05s timeout; task cancelled",
            "\(out.result.serialize())")
        #expect(out.result["reason"].asString == "timeout")
        #expect(out.result["task_id"].asInt == out.pid?.value, "\(out.result.serialize())")
    }

    /// C31: python `f"send: joined execution {d.pid!r} did not settle within
    /// this caller's {d.timeout}s wait; the execution itself keeps running"`,
    /// carrying `task_id` — the JOINED execution's pid, always (not nil): the
    /// execution itself keeps running, unowned by the joiner.
    @Test func joinerTimeoutTextIsPythonExactAndExecutionKeepsRunning() async {
        let kernel = freshKernel()
        await seed(kernel, "w")
        let opener = Task {
            await kernel.send([
                Item(AgentId("w"), .object(["type": .string("work")]), id: "job-slow")
            ])[0]
        }
        try? await Task.sleep(nanoseconds: 100_000_000)
        var joiner = Item(AgentId("w"), .object(["type": .string("work")]), id: "job-slow")
        joiner.timeout = 0.05
        let joined = await kernel.send([joiner])[0]
        #expect(joined.status == .timedOut, "\(joined.result.serialize())")
        let openedId = kernel.ledger.resolveName("job-slow")
        #expect(
            joined.result["error"].asString
                == "send: joined execution \(openedId ?? -1) did not settle within this "
                    + "caller's 0.05s wait; the execution itself keeps running",
            "\(joined.result.serialize())")
        #expect(joined.result["task_id"].asInt == openedId, "\(joined.result.serialize())")
        #expect(joined.pid?.value == openedId, "the outcome pid is the JOINED execution's id")
        // The execution itself keeps running — cancel it so the test doesn't
        // leak a 30s sleeper.
        _ = await kernel.send([
            Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .string("job-slow")]))
        ])[0]
        _ = await opener.value
    }
}
