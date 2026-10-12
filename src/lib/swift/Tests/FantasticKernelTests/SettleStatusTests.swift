// How a dispatch ENDED decides its status — never the reply's own keys.
//
// Python `_settle_once`: FAILED only from the `except` branch (the handler
// raised), CANCELLED only when the task itself was cancelled, TIMED_OUT only
// when the deadline left the task pending and the kill ended it. A returned
// dict is ERRORED or COMPLETED on its `error` alone. Swift used to read
// `cancelled`/`raised` off the body, so the `cancel` verb's own answer settled
// `cancelled` (and was dead-lettered), a relayed `{error, raised: true}`
// settled `failed`, and under a deadline any `{cancelled: true}` reply came
// back as the timeout refusal.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

/// `work` parks for 30 s; `say_cancelled` answers `{cancelled: true}`;
/// `say_raised` answers `{error, raised: true}` WITHOUT throwing; `boom` throws.
private struct Replier: AgentBundle {
    let name = "replier"
    struct Boom: Error, CustomStringConvertible { var description: String { "Boom: no" } }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString {
        case "work":
            try await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["done": .bool(true)])
        case "say_cancelled":
            return .object(["cancelled": .bool(true)])
        case "say_raised":
            return .object(["error": .string("relayed"), "raised": .bool(true)])
        case "boom":
            throw Boom()
        default:
            return .object(["ok": .bool(true)])
        }
    }
}

@Suite("settle status")
struct SettleStatusTests {

    private func freshKernel() async -> Kernel {
        let registry = BundleRegistry()
        registry.register("replier.tools", Replier())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string("w"), "handler_module": .string("replier.tools"),
                ]))
        ])[0]
        return kernel
    }

    @Test func theCancelVerbsOwnAnswerSettlesCompleted() async throws {
        let kernel = await freshKernel()
        let running = Task {
            await kernel.send([Item(AgentId("w"), .object(["type": .string("work")]))])[0]
        }
        try await Task.sleep(nanoseconds: 200_000_000)
        let ps = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0]
        let jid = (ps.result["tasks"].asArray ?? []).first { $0["agent"].asString == "w" }?["id"]
        let c = await kernel.send([
            Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": jid ?? .null]))
        ])[0]
        #expect(c.result["cancelled"].asBool == true, "\(c.result.serialize())")
        #expect(c.status == .completed, "the cancel verb ANSWERED; its call completed")
        // The KILLED dispatch is the one that settles cancelled.
        #expect(await running.value.status == .cancelled)
    }

    @Test func aReturnedCancelledTrueIsAReplyNotAKill() async {
        let kernel = await freshKernel()
        let out = await kernel.send([
            Item(AgentId("w"), .object(["type": .string("say_cancelled")]))
        ])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result["cancelled"].asBool == true)
    }

    @Test func aReturnedRaisedTrueIsErroredOnlyARaiseFails() async {
        let kernel = await freshKernel()
        let said = await kernel.send([Item(AgentId("w"), .object(["type": .string("say_raised")]))]
        )[0]
        #expect(said.status == .errored, "a returned dict settles on its `error`: \(said.status)")
        let threw = await kernel.send([Item(AgentId("w"), .object(["type": .string("boom")]))])[0]
        #expect(threw.status == .failed, "\(threw.result.serialize())")
        #expect(threw.result["raised"].asBool == true)
    }

    @Test func underADeadlineACancelledTrueReplyIsNotATimeout() async {
        let kernel = await freshKernel()
        var item = Item(AgentId("w"), .object(["type": .string("say_cancelled")]))
        item.timeout = 5
        let out = await kernel.send([item])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == .object(["cancelled": .bool(true)]))
    }

    @Test func underADeadlineAnExternalCancelSettlesCancelled() async throws {
        let kernel = await freshKernel()
        let running = Task {
            var item = Item(AgentId("w"), .object(["type": .string("work")]), id: "deadlined")
            item.timeout = 20
            return await kernel.send([item])[0]
        }
        try await Task.sleep(nanoseconds: 200_000_000)
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object(["type": .string("cancel"), "task_id": .string("deadlined")]))
        ])[0]
        let out = await running.value
        #expect(out.status == .cancelled, "\(out.result.serialize())")
        #expect(out.result == .object(["cancelled": .bool(true)]))
    }

    /// Python formats the RAW `timeout`: an int `1` reads `1s`, never `1.0s`.
    @Test func anIntTimeoutReadsAsAnIntInBothRefusals() async throws {
        let kernel = await freshKernel()
        var owned = Item(AgentId("w"), .object(["type": .string("work")]))
        owned.rawTimeout = .integer(1)
        let out = await kernel.send([owned])[0]
        #expect(out.status == .timedOut)
        #expect(
            out.result["error"].asString
                == "send: w 'work' exceeded its 1s timeout; task cancelled",
            "\(out.result.serialize())")

        let opener = Task {
            await kernel.send([
                Item(AgentId("w"), .object(["type": .string("work")]), id: "slow-int")
            ])[0]
        }
        try await Task.sleep(nanoseconds: 100_000_000)
        var joiner = Item(AgentId("w"), .object(["type": .string("work")]), id: "slow-int")
        joiner.rawTimeout = .integer(1)
        let joined = await kernel.send([joiner])[0]
        let openedId = kernel.ledger.resolveName("slow-int") ?? -1
        #expect(
            joined.result["error"].asString
                == "send: joined execution \(openedId) did not settle within this caller's 1s "
                + "wait; the execution itself keeps running",
            "\(joined.result.serialize())")
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object(["type": .string("cancel"), "task_id": .string("slow-int")]))
        ])[0]
        _ = await opener.value
    }
}
