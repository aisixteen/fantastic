// The SECRET-HOLDER MARK's two kernel rules, pinned WHOLE (owner 2026-10-02).
//
// `holds_secrets: true` makes an agent unreachable from a model turn (ai_core).
// The kernel owns the field itself:
//   - SHAPE, at the write, on every agent: exactly `true`, or `null` (the clear) —
//     anything else is `invalid_meta`, at create_agent and update_agent alike;
//   - WHO LOWERS IT, in the gate: on a holder, only the operator writes anything
//     but `true` — an agent granted `update_agent` cannot unmark a holder and then
//     call it.
// Order is fixed: the gate (who) → the kernel's shape checks → the bundle's own
// `validateMeta`.
//
// Twin of python `tests/kernel/test_holder_mark.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct Probe: AgentBundle {
    let name = "holder_mark_probe"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": "holder probe", "verbs": .object([:]),
            ])
        default:
            return .object(["ok": .string(agentId.value), "verb": payload["type"]])
        }
    }
}

private let MOD = "holder_mark_probe.tools"
// AN AGENT EDITS A RECORD IT NAMES (AV-1): `update_agent` is gated on its operand —
// egress naming it with the verb, and the operand's ingress accepting the editor.
private let UPDATER: JSON = ["vault": ["update_agent"], "plain": ["update_agent"]]
private let ACCEPTS_A: JSON = ["a": ["update_agent"]]

private func unmark(_ sender: String, _ target: String) -> JSON {
    .object([
        "error": .string(
            "\(sender) may not clear holds_secrets on \(target): it holds secrets, "
                + "and only the operator lowers that mark"),
        "reason": "unauthorized",
        "side": "secret-holder",
        "hint": .string(
            "only the operator clears it (update_agent holds_secrets=null); an agent "
                + "may mark a holder, never unmark one"),
    ])
}

private func shape(_ got: String) -> String {
    "holds_secrets must be exactly true (this agent holds secrets) or null "
        + "(the operator clears the mark) — got \(got)"
}

/// Every non-true, non-null value, with the type name the refusal reports.
private let BAD: [(JSON, String)] = [
    (false, "bool"),
    ("yes", "str"),
    ("true", "str"),
    (1, "int"),
    (0, "int"),
    (1.5, "float"),
    ([true], "list"),
    (["v": true], "dict"),
]

/// python `seeded_kernel`: a root `kernel_state` and the probe bundle.
private func seeded() -> Kernel {
    let r = BundleRegistry()
    r.register(MOD, Probe())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: ["guid": "g1"])
    k.register(root)
    k.setRoot(root)
    return k
}

/// python `k.create(MOD, id=…, **meta)` — an operator create that must succeed.
private func make(_ k: Kernel, _ id: String, _ meta: OrderedDictionary<String, JSON> = [:]) async {
    var p: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state", "handler_module": .string(MOD),
        "id": .string(id),
    ]
    for (key, v) in meta { p[key] = v }
    let made = await k.send([Item("kernel", .object(p))])[0].result
    #expect(made["id"].asString == id, "\(made.serialize())")
}

private func op(_ k: Kernel, _ payload: JSON) async -> JSON {
    await k.send([Item("kernel", payload)])[0].result
}

private func asAgent(_ k: Kernel, _ sender: String, _ payload: JSON) async -> Outcome {
    await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
        await k.send([Item("kernel", payload)])[0]
    }
}

/// The record's `holds_secrets` — `nil` when the record has no such field.
private func mark(_ k: Kernel, _ id: String) -> JSON? {
    k.agent(AgentId(id))?.metaValue(forKey: "holds_secrets")
}

private func keys(_ j: JSON) -> [String] {
    j.asObject.map { Array($0.keys) } ?? []
}

@Suite("the secret-holder mark — kernel rules")
struct HolderMarkTests {

    // ─── K1: only the operator lowers the mark ───────────────────────────────

    @Test func anAgentCannotClearTheMarkOnAHolder() async {
        let k = seeded()
        await make(k, "vault", ["holds_secrets": true, "callable_by": ACCEPTS_A])
        await make(k, "a", ["can_call": UPDATER])
        for v in [JSON.null] + BAD.map({ $0.0 }) {
            let out = await asAgent(
                k, "a", ["type": "update_agent", "id": "vault", "holds_secrets": v])
            #expect(out.pid == nil && out.status == .failed, "\(v.serialize())")
            #expect(
                out.result == unmark("a", "vault"), "\(v.serialize()): \(out.result.serialize())")
            #expect(keys(out.result) == ["error", "reason", "side", "hint"])
        }
        #expect(mark(k, "vault") == .bool(true))
    }

    /// The whole update is refused — a field beside the clear is not written.
    @Test func theRefusalWinsOverEveryOtherFieldInThePatch() async {
        let k = seeded()
        await make(k, "vault", ["holds_secrets": true, "callable_by": ACCEPTS_A])
        await make(k, "a", ["can_call": UPDATER])
        let r = await asAgent(
            k, "a",
            [
                "type": "update_agent", "id": "vault", "holds_secrets": .null,
                "description": "unmarked",
            ]
        ).result
        #expect(r == unmark("a", "vault"), "\(r.serialize())")
        #expect(k.agent(AgentId("vault"))?.metaValue(forKey: "description") == nil)
    }

    @Test func aHolderCannotClearItsOwnMark() async {
        let k = seeded()
        await make(k, "vault", ["holds_secrets": true, "can_call": UPDATER])
        let r = await asAgent(
            k, "vault", ["type": "update_agent", "id": "vault", "holds_secrets": .null]
        ).result
        #expect(r == unmark("vault", "vault"), "\(r.serialize())")
        #expect(mark(k, "vault") == .bool(true))
    }

    /// `true` passes from anywhere — on a plain agent it marks it, on a holder it
    /// is the value already there. Other fields of a holder stay writable.
    @Test func anAgentMayMarkButNeverUnmark() async {
        let k = seeded()
        await make(k, "plain", ["callable_by": ACCEPTS_A])
        await make(k, "vault", ["holds_secrets": true, "callable_by": ACCEPTS_A])
        await make(k, "a", ["can_call": UPDATER])
        for target in ["plain", "vault"] {
            let out = await asAgent(
                k, "a", ["type": "update_agent", "id": .string(target), "holds_secrets": true])
            #expect(out.result["updated"] == .bool(true), "\(out.result.serialize())")
            #expect(mark(k, target) == .bool(true))
        }
        let out = await asAgent(
            k, "a", ["type": "update_agent", "id": "vault", "description": "d"])
        #expect(out.result["updated"] == .bool(true), "\(out.result.serialize())")
        let r = await asAgent(
            k, "a", ["type": "update_agent", "id": "plain", "holds_secrets": .null]
        ).result
        #expect(r == unmark("a", "plain"), "once marked, the agent cannot take it back")
    }

    /// The rule guards a HOLDER. `null` on an agent with no mark changes nothing it
    /// protects, so it is not refused.
    @Test func nullOnAnUnmarkedAgentIsNotAClear() async {
        let k = seeded()
        await make(k, "plain", ["callable_by": ACCEPTS_A])
        await make(k, "a", ["can_call": UPDATER])
        let out = await asAgent(
            k, "a", ["type": "update_agent", "id": "plain", "holds_secrets": .null])
        #expect(out.result["updated"] == .bool(true), "\(out.result.serialize())")
    }

    /// A non-operator's malformed value on a HOLDER gets the gate's refusal; on a
    /// plain agent, the shape's.
    @Test func theGateJudgesBeforeTheShape() async {
        let k = seeded()
        await make(k, "plain", ["callable_by": ACCEPTS_A])
        await make(k, "vault", ["holds_secrets": true, "callable_by": ACCEPTS_A])
        await make(k, "a", ["can_call": UPDATER])
        let onHolder = await asAgent(
            k, "a", ["type": "update_agent", "id": "vault", "holds_secrets": "yes"]
        ).result
        #expect(onHolder == unmark("a", "vault"), "\(onHolder.serialize())")
        let onPlain = await asAgent(
            k, "a", ["type": "update_agent", "id": "plain", "holds_secrets": "yes"]
        ).result
        #expect(
            onPlain
                == .object([
                    "error": .string(shape("str")), "reason": "invalid_meta", "id": "plain",
                ]), "\(onPlain.serialize())")
    }

    /// `holds_secrets: null` is the clear: update_agent REPLACES the named field,
    /// so the record carries `null` — not a holder (only exactly `true` marks one).
    @Test func theOperatorClearsTheMarkWithNull() async {
        let k = seeded()
        await make(k, "vault", ["holds_secrets": true, "callable_by": ACCEPTS_A])
        let out = await op(k, ["type": "update_agent", "id": "vault", "holds_secrets": .null])
        #expect(out["updated"] == .bool(true), "\(out.serialize())")
        #expect(out["agent"].asObject?["holds_secrets"] == .null, "\(out.serialize())")
        #expect(mark(k, "vault") == .null)
        // cleared means cleared: an agent's `null` is no longer refused
        await make(k, "a", ["can_call": UPDATER])
        let again = await asAgent(
            k, "a", ["type": "update_agent", "id": "vault", "holds_secrets": .null])
        #expect(again.result["updated"] == .bool(true), "\(again.result.serialize())")
    }

    // ─── K2: the shape, at the write, on every agent ─────────────────────────

    @Test func everyNonTrueValueIsRefusedAtUpdate() async {
        let k = seeded()
        await make(k, "plain", ["callable_by": ACCEPTS_A])
        for (v, got) in BAD {
            let r = await op(k, ["type": "update_agent", "id": "plain", "holds_secrets": v])
            #expect(
                r
                    == .object([
                        "error": .string(shape(got)), "reason": "invalid_meta", "id": "plain",
                    ]),
                "\(v.serialize()): \(r.serialize())")
            #expect(keys(r) == ["error", "reason", "id"])
        }
        #expect(mark(k, "plain") == nil)
    }

    @Test func everyNonTrueValueIsRefusedAtCreate() async {
        let k = seeded()
        for (i, (v, got)) in BAD.enumerated() {
            let aid = "made_\(i)"
            let r = await op(
                k,
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": .string(MOD),
                    "id": .string(aid), "holds_secrets": v,
                ])
            #expect(
                r == .object(["error": .string(shape(got)), "reason": "invalid_meta"]),
                "\(v.serialize()): \(r.serialize())")
            #expect(k.agent(AgentId(aid)) == nil)
        }
    }

    /// The shape check is the write's, not the operator's: an agent forking its
    /// own subtree is refused the same way.
    @Test func anAgentCreateIsShapeCheckedToo() async {
        let k = seeded()
        await make(k, "a")
        let r = await asAgent(
            k, "a",
            [
                "type": "create_agent", "parent_id": "a", "handler_module": .string(MOD),
                "id": "kid", "holds_secrets": false,
            ]
        ).result
        #expect(
            r == .object(["error": .string(shape("bool")), "reason": "invalid_meta"]),
            "\(r.serialize())")
        #expect(k.agent(AgentId("kid")) == nil)
    }

    @Test func trueAndNullAreLegalAtCreate() async {
        let k = seeded()
        for (aid, v) in [("marked", JSON.bool(true)), ("unmarked", JSON.null)] {
            let r = await op(
                k,
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": .string(MOD),
                    "id": .string(aid), "holds_secrets": v,
                ])
            #expect(r["id"].asString == aid, "\(r.serialize())")
            #expect(mark(k, aid) == v)
        }
    }
}
