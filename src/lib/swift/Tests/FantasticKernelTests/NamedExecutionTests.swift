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
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    private func seed(_ kernel: Kernel, _ id: String) async {
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
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
        let ids = (ps["tasks"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("job-1"), "the caller's own id must be the ps handle: \(ids)")

        let c = await kernel.send([
            Item(
                AgentId("kernel"),
                .object(["type": .string("cancel"), "task_id": .string("job-1")]))
        ])[0].result
        #expect(c["cancelled"].asBool == true, "\(c.serialize())")
        #expect(c["killed"][0].asString == "job-1", "\(c.serialize())")

        let out = await running.value
        #expect(out.status == .cancelled, "\(out.result.serialize())")
        #expect(out.pid?.value == "job-1", "the outcome's pid is the caller's id")
    }

    @Test func anUnnamedExecutionStillMintsAPid() async {
        let kernel = freshKernel()
        await seed(kernel, "w")
        let out = await kernel.send([Item(AgentId("w"), .object(["type": .string("ping")]))])[0]
        let pid = out.pid?.value ?? ""
        #expect(pid.hasPrefix("w:"), "a minted pid is <agent>:<hex>, got \(pid)")
    }
}
