// JOINING a caller-scoped kernel verb's execution (owner-approved 2026-10-03,
// audit KV-1).
//
// `ledger` and `ack_deadletter` answer from WHO sends them — the executions that
// caller originated. A join never re-runs the handler, so before the fix a joiner
// was handed the original caller's view: `B`, granted `ledger`, joined `A`'s named
// ledger read (or the operator's unscoped one, by its int pid) and received A's
// (or every caller's) rows. Now a join of such an execution by any caller other
// than the one that sent it is refused; the original caller still dedups, and
// cross-caller joins of every other verb are unchanged.
//
// Twin of python `tests/kernel/test_caller_scoped_join.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private let MOD = "scoped_join_probe.tools"

private struct BoomError: Error, CustomStringConvertible {
    var description: String { "boom" }
}

private struct Probe: AgentBundle {
    let name = "scoped_join_probe"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown", "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": "scoped join probe",
                "verbs": .object([:]),
            ])
        case "work":
            return .object(["body": "result-of-a"])
        case "boom":
            throw BoomError()
        default:
            return .object(["error": .string("unknown verb \(pyRepr(payload["type"]))")])
        }
    }
}

/// `A` and `B`, children of the root granted its caller-scoped verbs (and `ps`)
/// by name — a wildcard never covers a kernel verb — and a `desk` both may call.
private func seeded() async -> Kernel {
    let r = BundleRegistry()
    r.register(MOD, Probe())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: ["guid": "g1"])
    k.register(root)
    k.setRoot(root)
    func create(_ meta: OrderedDictionary<String, JSON>) async {
        var p: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
        ]
        for (key, v) in meta { p[key] = v }
        let out = await k.send([Item("kernel", .object(p))])[0].result
        #expect(out["id"] == p["id"], "\(out.serialize())")
    }
    await create([
        "id": "desk", "handler_module": .string(MOD), "callable_by": ["A": "*", "B": "*"],
    ])
    for aid in ["A", "B"] {
        await create([
            "id": .string(aid), "handler_module": .string(MOD),
            "can_call": ["desk": "*", "kernel_state": ["ledger", "ack_deadletter", "ps"]],
        ])
    }
    return k
}

private func send(_ k: Kernel, _ item: Item, _ sender: String? = nil) async -> Outcome {
    guard let sender else { return await k.send([item])[0] }
    return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
        await k.send([item])[0]
    }
}

/// A NAMED call: the ledger keeps a named execution's result body.
private func work() -> Item { Item("desk", ["type": "work"], id: "work-a") }

private func kernelItem(_ payload: JSON, id: String? = nil) -> Item {
    Item("kernel", payload, id: id)
}

private func byPid(_ payload: JSON, _ pid: Int64) -> Item {
    var it = Item("kernel", payload)
    it.rawId = .integer(pid)
    return it
}

private func refused(_ execId: String, _ verb: String) -> JSON {
    .object([
        "error": .string(
            "send: id \(execId) names another caller's \(verb) execution — \(verb) answers "
                + "only the caller that sent it, so it cannot be joined; make your own "
                + "\(verb) call under a new id"),
        "reason": "bad_arg",
        "arg": "id",
    ])
}

@Suite("joining a caller-scoped kernel verb")
struct CallerScopedJoinTests {

    @Test func aForeignJoinOfANamedLedgerReadIsRefused() async {
        let k = await seeded()
        _ = await send(k, work(), "A")
        let own = await send(k, kernelItem(["type": "ledger"], id: "led-A"), "A")
        #expect(own.status == .completed, "\(own.asJSON.serialize())")
        #expect(own.result.serialize().contains("result-of-a"), "\(own.result.serialize())")

        let byName = await send(k, kernelItem(["type": "ledger"], id: "led-A"), "B")
        #expect(byName.status == .failed)
        #expect(byName.result == refused("'led-A'", "ledger"), "\(byName.result.serialize())")
        let pid = own.pid!.value
        let viaPid = await send(k, byPid(["type": "ledger"], pid), "B")
        #expect(viaPid.result == refused(String(pid), "ledger"), "\(viaPid.result.serialize())")

        // the original caller still dedups: the recorded view
        let again = await send(k, kernelItem(["type": "ledger"], id: "led-A"), "A")
        #expect(again.result == own.result)
        #expect(again.pid == own.pid)
    }

    @Test func anAgentJoiningTheOperatorsLedgerReadIsRefused() async {
        let k = await seeded()
        _ = await send(k, work(), "A")
        let op = await send(k, kernelItem(["type": "ledger"]))
        #expect(op.result.serialize().contains("result-of-a"), "the operator's read is unscoped")
        let pid = op.pid!.value
        let out = await send(k, byPid(["type": "ledger"], pid), "B")
        #expect(out.result == refused(String(pid), "ledger"), "\(out.result.serialize())")
    }

    @Test func theOperatorJoiningAnAgentsLedgerReadIsRefused() async {
        let k = await seeded()
        _ = await send(k, kernelItem(["type": "ledger"], id: "led-A"), "A")
        let out = await send(k, kernelItem(["type": "ledger"], id: "led-A"))
        #expect(out.result == refused("'led-A'", "ledger"), "\(out.result.serialize())")
    }

    @Test func aForeignJoinOfAnAckDeadletterIsRefused() async {
        let k = await seeded()
        let dead = await send(k, Item("desk", ["type": "boom"]), "A")
        #expect(dead.status == .failed, "\(dead.asJSON.serialize())")
        let ack = kernelItem(
            ["type": "ack_deadletter", "id": .integer(dead.pid!.value)], id: "ack-A")
        let own = await send(k, ack, "A")
        #expect(own.result["acked"] == .bool(true), "\(own.result.serialize())")
        let out = await send(k, ack, "B")
        #expect(out.result == refused("'ack-A'", "ack_deadletter"), "\(out.result.serialize())")
    }

    /// `ps` answers the same table to everyone: B joining A's named `ps` is the
    /// once-only contract, unchanged.
    @Test func crossCallerJoinsOfOtherVerbsStayOpen() async {
        let k = await seeded()
        let own = await send(k, kernelItem(["type": "ps"], id: "ps-A"), "A")
        let joined = await send(k, kernelItem(["type": "ps"], id: "ps-A"), "B")
        #expect(own.status == .completed, "\(own.asJSON.serialize())")
        #expect(joined.result == own.result)
        #expect(joined.pid == own.pid)
    }
}
