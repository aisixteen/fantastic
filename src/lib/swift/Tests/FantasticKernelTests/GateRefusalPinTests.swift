// The call gate's refusal bodies, byte for byte — key order included.
//
// The gate (`Kernel.gate`, `gatePlacement`, `checkConferral`,
// `heritableConferral`) spells its refusals as literal dicts, and their key
// orders differ: an egress/ingress/placement denial is `error, reason, hint,
// side`; a conferral denial is `error, reason, side, [missing|parent|arg,]
// hint`. Only the first matches `Refusal.make`'s order, so a sweep into the one
// helper must keep every body below. Texts are python `kernel/_agent.py`'s.

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

private let FROM_ABOVE =
    "authority is granted from ABOVE — set at create_agent by your creator, or by the operator; "
    + "a parent may delegate a subset of its own reach. You cannot grant it to yourself. Agents "
    + "are deny-all by default (both to call and to be called)."

private let BASE: OrderedDictionary<String, JSON> = [
    "$self": "*", "$parent": "*", "$children": "*",
]

/// root `kernel_state` (guid g1); `a` (may call `b`, may create/update at the
/// root) with a child `kid`; `b` (accepts nobody); `lonely` (structural tokens
/// only); `placer` (may `create_agent` on `b`).
private func gatedKernel() async -> Kernel {
    let r = BundleRegistry()
    r.register("noop.tools", Noop())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: ["guid": "g1"])
    k.register(root)
    k.setRoot(root)
    func seed(_ id: String, _ parent: String, _ extra: OrderedDictionary<String, JSON> = [:]) async
    {
        var p: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": .string(parent), "id": .string(id),
            "handler_module": "noop.tools",
        ]
        for (key, v) in extra { p[key] = v }
        _ = await k.send([Item("kernel", .object(p))])[0].result
    }
    var aCan = BASE
    aCan["b"] = "*"
    aCan["kernel_state"] = .array(["update_agent", "create_agent"])
    await seed("a", "kernel_state", ["can_call": .object(aCan)])
    await seed("b", "kernel_state")
    await seed("lonely", "kernel_state", ["can_call": .object(BASE)])
    var placer = BASE
    placer["b"] = .array(["create_agent"])
    await seed("placer", "kernel_state", ["can_call": .object(placer)])
    await seed("kid", "a")
    return k
}

private func call(_ k: Kernel, as sender: String, _ target: String, _ p: JSON) async -> String {
    await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
        await k.send([Item(AgentId(target), p)])[0].result.serialize()
    }
}

private func grantToKid(_ k: Kernel, _ canCall: JSON) async -> String {
    await call(
        k, as: "a", "kernel", .object(["type": "update_agent", "id": "kid", "can_call": canCall]))
}

@Suite("call gate — refusal bodies")
struct GateRefusalPinTests {

    @Test func egressAndIngressDenials() async {
        let k = await gatedKernel()
        #expect(
            await call(k, as: "lonely", "b", ["type": "ping"])
                == #"{"error":"lonely may not call 'ping' on b","reason":"unauthorized","hint":"\#(FROM_ABOVE)","side":"egress"}"#
        )
        #expect(
            await call(k, as: "a", "b", ["type": "ping"])
                == #"{"error":"b does not accept 'ping' from a","reason":"unauthorized","hint":"\#(FROM_ABOVE)","side":"ingress"}"#
        )
    }

    @Test func placementDenialsJudgeTheParent() async {
        let k = await gatedKernel()
        #expect(
            await call(
                k, as: "lonely", "kernel",
                [
                    "type": "create_agent", "parent_id": "b", "id": "x1",
                    "handler_module": "noop.tools",
                ])
                == #"{"error":"lonely may not create under b","reason":"unauthorized","hint":"\#(FROM_ABOVE)","side":"egress"}"#
        )
        #expect(
            await call(
                k, as: "placer", "kernel",
                [
                    "type": "create_agent", "parent_id": "b", "id": "x2",
                    "handler_module": "noop.tools",
                ])
                == #"{"error":"b does not accept a child from placer","reason":"unauthorized","hint":"\#(FROM_ABOVE)","side":"ingress"}"#
        )
    }

    @Test func conferralDenials() async {
        let k = await gatedKernel()
        #expect(
            await call(
                k, as: "a", "kernel", ["type": "update_agent", "id": "a", "can_call": ["b": "*"]])
                == #"{"error":"a may not edit its own can_call/callable_by","reason":"unauthorized","side":"self-grant","hint":"\#(FROM_ABOVE)"}"#
        )
        #expect(
            await grantToKid(k, ["$foo": "*"])
                == #"{"error":"a cannot confer '$foo': not a structural token — $self, $parent, $children and $loader are the only ones","#
                + #""reason":"bad_arg","arg":"can_call","side":"delegation","hint":"\#(FROM_ABOVE)"}"#
        )
        #expect(
            await grantToKid(k, ["$loader": ["ps"]])
                == #"{"error":"a cannot delegate 'ps' on $loader: it resolves to kernel_state, which you do not hold — $loader is real reach on the kernel's control surface, not a structural token","#
                + #""reason":"unauthorized","side":"delegation","hint":"\#(FROM_ABOVE)"}"#)
        #expect(
            await grantToKid(k, ["ghost": "*"])
                == #"{"error":"a cannot delegate '*' on ghost: no such agent — calls of one batch run concurrently, so an agent created in this batch does not exist yet; create in one batch, grant in the next","#
                + #""reason":"unauthorized","side":"delegation","missing":"ghost","hint":"\#(FROM_ABOVE)"}"#
        )
        #expect(
            await grantToKid(k, ["lonely": "*"])
                == #"{"error":"a cannot delegate '*' on lonely: lonely is not your child (its parent is kernel_state) and not in your can_call — you may only confer can_call you already hold; create the agents you will wire as YOUR children","#
                + #""reason":"unauthorized","side":"delegation","parent":"kernel_state","hint":"\#(FROM_ABOVE)"}"#
        )
        #expect(
            await grantToKid(k, ["b": ["verbs": ["ping"], "for": "descendants"]])
                == #"{"error":"a cannot confer a heritable 'ping' on b: you do not hold that heritable entry — only the operator, or a holder of the same entry, confers one (a subset of it)","#
                + #""reason":"unauthorized","side":"delegation","hint":"\#(FROM_ABOVE)"}"#)
        // a subset of what the granter holds passes
        let ok = await grantToKid(k, ["b": ["ping"]])
        #expect(ok.hasPrefix(#"{"updated":true,"id":"kid","#), "\(ok)")
    }

    /// A relayed FOREIGN caller is judged by its qualified address, and the
    /// hint names the two callable_by spellings that would accept it.
    @Test func aForeignCallerIsNamedInTheIngressDenial() async {
        let k = await gatedKernel()
        let out = await KernelTaskLocals.withRemoteCaller(leg: AgentId("a"), address: "g2|scout") {
            await call(k, as: "a", "b", ["type": "ping"])
        }
        #expect(
            out
                == #"{"error":"b does not accept 'ping' from g2|scout","reason":"unauthorized","#
                + #""hint":"a foreign caller is accepted only by name: callable_by {'g2|scout': [...]} or {'g2|*': [...]} on b","side":"ingress"}"#
        )
    }

    /// A target on a CONNECTED peer kernel is an egress question only, and the
    /// hint names the two can_call spellings that would reach it.
    @Test func aRemoteTargetIsAnEgressDenialNamingThePeer() async {
        let k = await gatedKernel()
        let route = PeerRoute(via: "leg") { _, _, _, _ in (.object(["ok": true]), nil) }
        #expect(k.registerPeer("g9", route) == nil)
        #expect(
            await call(k, as: "lonely", "g9|canvas", ["type": "ping"])
                == #"{"error":"lonely may not call 'ping' on g9|canvas","reason":"unauthorized","#
                + #""hint":"g9|canvas is an agent on the connected kernel g9: name it in can_call — {'g9|canvas': [...]} or {'g9|*': [...]}","side":"egress"}"#
        )
        k.unregisterPeer("g9", route)
    }
}
