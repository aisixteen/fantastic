// The call gate's refusal bodies, byte for byte — key order included.
//
// The gate (`Kernel.gate`) spells its refusals as literal dicts: an
// egress/ingress denial is `error, reason, hint, side`; a mutator denial is
// `error, reason, side`. Texts are python `kernel/_agent.py`'s.

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
    "grants come from above — an ancestor (update_agent on an agent in its subtree) or the "
    + "operator writes can_call and callable_by; an agent never raises its own"

private let BASE: OrderedDictionary<String, JSON> = ["$self": "*"]

/// root `kernel_state` (guid g1, accepts `a`); `a` (may call `b`, may
/// create/update at the root) with a child `kid`; `b` (accepts nobody);
/// `lonely` (the baseline only).
private func gatedKernel() async -> Kernel {
    let r = BundleRegistry()
    r.register("noop.tools", Noop())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: "kernel_state", handlerModule: nil, parentId: nil,
        meta: ["guid": "g1", "callable_by": ["a": "*"]])
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
    await seed("kid", "a")
    return k
}

private func call(_ k: Kernel, as sender: String, _ target: String, _ p: JSON) async -> String {
    await KernelTaskLocals.$sender.withValue(AgentId(sender)) {
        await k.send([Item(AgentId(target), p)])[0].result.serialize()
    }
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

    @Test func mutatorDenials() async {
        let k = await gatedKernel()
        #expect(
            await call(
                k, as: "a", "kernel",
                [
                    "type": "create_agent", "parent_id": "b", "id": "x1",
                    "handler_module": "noop.tools",
                ])
                == #"{"error":"a may only create_agent within its own subtree — b is not in it","reason":"unauthorized","side":"mutator"}"#
        )
        #expect(
            await call(
                k, as: "a", "kernel", ["type": "update_agent", "id": "a", "can_call": ["b": "*"]])
                == #"{"error":"a may only update_agent within its own subtree — a is not in it","reason":"unauthorized","side":"mutator"}"#
        )
        // an unknown id is not a direct child either
        #expect(
            await call(k, as: "a", "kernel", ["type": "update_agent", "id": "nobody"])
                == #"{"error":"a may only update_agent within its own subtree — nobody is not in it","reason":"unauthorized","side":"mutator"}"#
        )
    }

    /// A mutator with a MISSING or non-string operand gets that verb's own
    /// missing/shape refusal — the mutator rule judges only a named operand.
    @Test func aMissingOperandGetsTheVerbsOwnRefusal() async {
        let k = await gatedKernel()
        #expect(
            await call(k, as: "a", "kernel", ["type": "create_agent", "handler_module": "noop.tools"])
                == #"{"error":"create_agent: parent_id required","reason":"missing_arg","hint":"name the agent that will OWN the new one — your own id for a tool you keep, or the root ('kernel_state') for a top-level agent","arg":"parent_id"}"#
        )
        #expect(
            await call(k, as: "a", "kernel", ["type": "update_agent", "description": "x"])
                == #"{"error":"update_agent: id required","reason":"missing_arg","arg":"id"}"#
        )
        #expect(
            await call(k, as: "a", "kernel", ["type": "update_agent", "id": 7, "description": "x"])
                == #"{"error":"update_agent: id required","reason":"missing_arg","arg":"id"}"#
        )
    }

    /// A target on a CONNECTED peer kernel is an egress question only, and the
    /// hint names the two can_call spellings that would reach it.
    @Test func aRemoteTargetIsAnEgressDenialNamingThePeer() async {
        let k = await gatedKernel()
        let route = PeerRoute(via: "leg") { _, _, _ in (.object(["ok": true]), nil) } emit: {
            _, _, _ in
        }
        k.registerPeer("g9", route)
        #expect(
            await call(k, as: "lonely", "g9|canvas", ["type": "ping"])
                == #"{"error":"lonely may not call 'ping' on g9|canvas","reason":"unauthorized","#
                + #""hint":"g9|canvas is an agent on the connected kernel g9: name it in can_call — {'g9|canvas': [...]} or {'g9|*': [...]}","side":"egress"}"#
        )
        k.unregisterPeer("g9", route)
    }
}
