// Audit W-8 — the one write-time check every leg makes, and a malformed rule
// that reaches a record anyway.
//
// Python `io_bridge.unknown_ingress_refusal`: an `ingress_rule` no rule
// implements — an unknown name, or a malformed spec — is refused at the WRITE by
// every leg's `validateMeta`, naming the leg's legal values. The wire legs
// (ws_bridge, relay_connector, web_ws, web_rest) had no such check, and a
// malformed rule a record still carries used to deny in words of its own.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticWebRest
import FantasticWebWS
import Foundation
import Testing

@Suite("io legs: the ingress_rule write check")
struct LegIngressWriteCheckTests {
    private static let wireLegal =
        "legal values: allow_all_ingress, deny_ingress, password_ingress"

    @Test func everyWireLegRefusesAnUnknownOrMalformedRule() {
        let legs: [(String, any AgentBundle)] = [
            ("ws_bridge", KernelBridgeBundle(family: .ws)),
            ("relay_connector", KernelBridgeBundle(family: .relay)),
            ("web_ws", WebWSBundle()),
            ("web_rest", WebRestBundle()),
        ]
        let cases: [(JSON, String)] = [
            (.string("open_sesame"), "'open_sesame'"),
            (.integer(7), "7"),
            (.array([.string("allow_all_ingress")]), "['allow_all_ingress']"),
            (.object(["type": .array([.string("x")])]), "{'type': ['x']}"),
            (
                .object(["type": .string("password_ingress"), "env": .integer(7)]),
                "{'type': 'password_ingress', 'env': 7}"
            ),
        ]
        for (name, leg) in legs {
            for (spec, shown) in cases {
                #expect(
                    leg.validateMeta(["ingress_rule": spec], record: [:])
                        == "ingress_rule=\(shown) is not a rule this bundle implements — "
                        + Self.wireLegal,
                    "\(name) \(shown)")
            }
            for spec: JSON in [
                .string("password_ingress"),
                .object(["type": .string("password_ingress"), "env": .string("GROUP_TOKEN")]),
            ] {
                #expect(leg.validateMeta(["ingress_rule": spec], record: [:]) == nil, "\(name)")
            }
        }
    }

    /// A record can still carry one (written below the verbs, or by hand on
    /// disk): the inbound call is ANSWERED with the seal's denial in python's
    /// words. It read as a PLAIN seal here (`edge sealed by policy`).
    @Test(.timeLimit(.minutes(1)))
    func aMalformedRuleOnTheRecordDeniesInPythonsWords() async throws {
        let ws = KernelBridgeBundle(family: .ws)
        let r = BundleRegistry()
        r.register("ws_bridge.tools", ws)
        let k = Kernel(storage: .inMemory, bundles: r)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        k.update(root.id, ["guid": .string("guid-b")])
        let made = await k.send([
            Item(
                AgentId("kernel"),
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "ws_bridge.tools", "id": "b_leg", "transport": "memory",
                ])
        ])[0].result
        #expect(made["id"] == "b_leg", "\(made.serialize())")
        k.update(AgentId("b_leg"), ["ingress_rule": .integer(7)])
        let (peer, mine) = MemoryTransport.pair()
        ws.injectTransport(agentId: AgentId("b_leg"), transport: mine)
        let booted = await k.send([Item(AgentId("b_leg"), ["type": "boot"])])[0].result
        #expect(booted["booted"] == true, "\(booted.serialize())")
        try await peer.send([
            "type": "call", "id": "c7", "target": "kernel_state",
            "payload": ["type": "reflect"],
        ])
        let reply = try await peer.recv().frame
        #expect(reply["id"] == "c7")
        #expect(reply["data"]["error"] == "edge sealed — unknown ingress rule '7'")
        #expect(reply["data"]["reason"] == "unauthorized")
        await peer.close()
    }
}
