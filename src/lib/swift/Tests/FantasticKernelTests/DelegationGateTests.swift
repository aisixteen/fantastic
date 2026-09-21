// The conferral gate names the TRUE reason.
//
// THE BATCH TRAP: `[create X, grant peer→X]` in ONE send. Items launch
// concurrently, so X is not an agent yet when the grant's gate runs. The refusal
// must say "no such agent" and point at the next batch — "you don't hold X" is
// false and sent a builder into a 382-second deliberation (measured 2026-09-02,
// E30 subai:python). The same grant in the NEXT send passes.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct Noop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}

@Suite("delegation gate")
struct DelegationGateTests {
    @Test func delegatingOnAnAgentThatDoesNotExistNamesThat() async {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)

        let peer = await kernel.send([Item(AgentId("kernel_state"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "id": .string("peer"), "handler_module": .string("noop.tools"),
        ]))])[0].result
        #expect(peer["id"].asString == "peer", "\(peer.serialize())")
        let granter = await kernel.send([Item(AgentId("kernel_state"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "id": .string("granter"), "handler_module": .string("noop.tools"),
            "can_call": .object([
                "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
                "kernel_state": .array([.string("*"), .string("create_agent"), .string("update_agent")]),
            ]),
        ]))])[0].result
        #expect(granter["id"].asString == "granter", "\(granter.serialize())")

        let grant: JSON = .object([
            "type": .string("update_agent"), "id": .string("peer"),
            "can_call": .object(["kid": .string("*")]),
        ])
        let (no, ok, held): (JSON, JSON, JSON) = await KernelTaskLocals.$currentSender.withValue(
            AgentId("granter")
        ) {
            let batch = await kernel.send([
                Item(AgentId("kernel"), .object([
                    "type": .string("create_agent"), "parent_id": .string("granter"),
                    "id": .string("kid"), "handler_module": .string("noop.tools"),
                ])),
                Item(AgentId("kernel"), grant),
            ])
            // Next batch: kid exists and is the granter's child → the grant passes.
            let ok = await kernel.send([Item(AgentId("kernel"), grant)])[0].result
            // An EXISTING agent the granter does not hold: the refusal says whose
            // child it is — the diagnosis behind "create your organs UNDER YOURSELF".
            let held = await kernel.send([Item(AgentId("kernel"), .object([
                "type": .string("update_agent"), "id": .string("kid"),
                "can_call": .object(["peer": .string("*")]),
            ]))])[0].result
            return (batch[1].result, ok, held)
        }
        #expect(held["side"].asString == "delegation", "\(held.serialize())")
        #expect(held["parent"].asString == "kernel_state", "\(held.serialize())")
        #expect((held["error"].asString ?? "").contains("not your child"), "\(held.serialize())")
        #expect(no["reason"].asString == "unauthorized", "\(no.serialize())")
        #expect(no["side"].asString == "delegation", "\(no.serialize())")
        #expect(no["missing"].asString == "kid", "\(no.serialize())")
        let msg = no["error"].asString ?? ""
        #expect(msg.contains("no such agent") && msg.contains("next"), "\(no.serialize())")
        // `update_agent` answers python's shape — {updated, id, agent} — so the
        // granted edge is read off `agent`, not off the reply's own top level.
        #expect(ok["updated"].asBool == true, "\(ok.serialize())")
        #expect(ok["id"].asString == "peer", "\(ok.serialize())")
        #expect(ok["agent"]["can_call"]["kid"].asString == "*", "\(ok.serialize())")
    }
}
