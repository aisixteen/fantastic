// Twins of python `test_secret_marks.py`,
// `test_minimal_gate.py::test_a_deleted_id_leaves_every_grant` and
// `test_send_launch.py::test_draining_refuses_a_fork_but_admits_a_keepalive_one`.
//
// - SECRET MARKS: an item's `secret` paths are shown as `[secret]` on the call
//   row and the state stream; the handler gets the real payload and reads the
//   marks as `KernelTaskLocals.currentSecret`.
// - A deleted id leaves every grant that named it.
// - While draining, a fork's send is new work (refused); a keepalive fork's is not.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private final class Seen: @unchecked Sendable {
    private let lock = NSLock()
    private var _items: [JSON] = []
    var items: [JSON] { lock.withLock { _items } }
    func add(_ v: JSON) { lock.withLock { _items.append(v) } }
}

private struct Probe: AgentBundle {
    let name = "audit_probe"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            return .object([
                "got": payload["fields"]["api_key"],
                "marks": .array(KernelTaskLocals.currentSecret.map { .string($0) }),
            ])
        }
    }
}

private func makeKernel() -> Kernel {
    let r = BundleRegistry()
    r.register("audit_probe.tools", Probe())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    return k
}

private func create(
    _ k: Kernel, _ id: String, parent: String = "kernel_state",
    _ extra: OrderedDictionary<String, JSON> = [:]
) async {
    var p: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"), "parent_id": .string(parent), "id": .string(id),
        "handler_module": .string("audit_probe.tools"),
    ]
    for (key, v) in extra { p[key] = v }
    let r = await k.send([Item(AgentId("kernel"), .object(p))])[0].result
    #expect(r["id"].asString == id, "\(r.serialize())")
}

@Suite("secret marks, grant pruning, drain")
struct SecretMarksGrantsDrainTests {
    @Test func aMarkedValueIsShownNeverKept() async {
        let k = makeKernel()
        await create(k, "sp")
        let seen = Seen()
        let token = k.subscribe { ev, _ in seen.add(ev) }
        defer { k.unsubscribe(token) }
        var item = Item(
            AgentId("sp"),
            .object([
                "type": .string("set"), "ref": .string("r"),
                "fields": .object(["api_key": .string("sk-REAL")]),
            ]))
        item.secret = .array([.string("fields.api_key")])
        let out = await k.send([item])[0]
        #expect(out.result["got"] == .string("sk-REAL"), "\(out.result.serialize())")
        #expect(out.result["marks"] == .array([.string("fields.api_key")]))
        let call = k.rows.all().first { $0["id"] == .integer(out.pid?.value ?? -1) }
        #expect(call?["payload"]?["fields"]["api_key"] == .string("[secret]"))
        #expect(call?["secret"] == .array([.string("fields.api_key")]))
        let events = seen.items.map { $0.serialize() }.joined()
        #expect(!events.contains("sk-REAL"), "the state stream carried the value")
        #expect(events.contains("[secret]"))
        for bad: JSON in [
            .string("fields.api_key"), .array([.string("")]), .array([.string("a..b")]),
            .array([.integer(1)]), .array([.string("type")]),
        ] {
            var it = Item(AgentId("sp"), .object(["type": .string("set")]))
            it.secret = bad
            let r = await k.send([it])[0].result
            #expect(r["arg"] == .string("secret"), "\(bad.serialize()): \(r.serialize())")
        }
    }

    @Test func aDeletedIdLeavesEveryGrant() async {
        let k = makeKernel()
        await create(
            k, "vault",
            ["callable_by": .object(["$self": .string("*"), "reader": .array([.string("get")])])])
        await create(k, "reader")
        await create(k, "reader_kid", parent: "reader")
        await create(
            k, "other",
            [
                "callable_by": .object([
                    "$self": .string("*"), "reader_kid": .string("*"), "vault": .string("*"),
                ])
            ])
        let r = await k.send([
            Item(AgentId("kernel"), .object(["type": .string("delete_agent"), "id": .string("reader")]))
        ])[0].result
        #expect(r["deleted"] == .bool(true), "\(r.serialize())")
        #expect(
            k.agent(AgentId("vault"))?.metaValue(forKey: "callable_by")
                == .object(["$self": .string("*")]))
        #expect(
            k.agent(AgentId("other"))?.metaValue(forKey: "callable_by")
                == .object(["$self": .string("*"), "vault": .string("*")]))
    }

    @Test func whileDrainingAForkIsRefusedAndAKeepaliveForkIsNot() async {
        let k = makeKernel()
        await create(k, "d2")
        _ = await k.shutdownAgents()
        for keepalive in [false, true] {
            let box = Seen()
            let done = AsyncStream<Void>.makeStream()
            k.spawnTask(agent: AgentId("d2"), verb: "loop", keepalive: keepalive) {
                let out = await k.send([Item(AgentId("d2"), .object(["type": .string("work")]))])[0]
                box.add(.object(["status": .string(out.status.rawValue), "result": out.result]))
                done.continuation.finish()
            }
            for await _ in done.stream {}
            let got = box.items.first
            if keepalive {
                #expect(got?["status"] == .string("completed"), "\(got?.serialize() ?? "nil")")
            } else {
                #expect(got?["result"]["reason"] == .string("draining"), "\(got?.serialize() ?? "nil")")
            }
        }
    }
}
