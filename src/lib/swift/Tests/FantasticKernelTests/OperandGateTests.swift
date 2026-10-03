// `update_agent` / `delete_agent` are gated on their OPERAND (`payload.id`), the
// way `create_agent` is gated on its placement (audit 2026-10-03, AV-1).
//
// Measured before the fix: an agent holding only `<root>:[update_agent,
// delete_agent]` — the ordinary builder wiring — rewrote an unrelated agent's
// record, cleared another agent's `delete_lock` and deleted it. The gate asked
// about the ROOT (which merely ANSWERS the verb) and never about the agent the
// verb ACTS UPON. And the same gate refused an agent editing its OWN child
// without that root grant.
//
// The rule: editing or deleting inside your OWN subtree (yourself included) is
// baseline. Anywhere else needs egress NAMING the operand with the verb
// (`<id>: ["update_agent"]` — never a wildcard, not even `<id>: "*"`) AND the
// operand's `callable_by` accepting the sender. The operator is ungated.
//
// Twin of python `tests/kernel/test_operand_gate.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct Noop: AgentBundle {
    let name = "operand_noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default: return .object(["ok": .bool(true)])
        }
    }
}

private let MOD = "operand_noop.tools"
private let BASE: OrderedDictionary<String, JSON> = [
    "$self": "*", "$parent": "*", "$children": "*",
]

private func grant(_ extra: OrderedDictionary<String, JSON>) -> JSON {
    var g = BASE
    for (k, v) in extra { g[k] = v }
    return .object(g)
}

private func seeded() -> Kernel {
    let r = BundleRegistry()
    r.register(MOD, Noop())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    return k
}

/// Create as the OPERATOR (ungated) — the fixture builder.
@discardableResult
private func plain(
    _ k: Kernel, _ id: String, parent: String = "kernel_state",
    _ meta: OrderedDictionary<String, JSON> = [:]
) async -> String {
    var p: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": .string(parent), "id": .string(id),
        "handler_module": .string(MOD),
    ]
    for (key, v) in meta { p[key] = v }
    let r = await k.send([Item(AgentId("kernel"), .object(p))])[0].result
    #expect(r["id"].asString == id, "\(r.serialize())")
    return id
}

private func call(_ k: Kernel, _ sender: String?, _ payload: JSON) async -> JSON {
    let item = Item(AgentId("kernel"), payload)
    guard let s = sender else { return await k.send([item])[0].result }
    return await KernelTaskLocals.$currentSender.withValue(AgentId(s)) {
        await k.send([item])[0].result
    }
}

private func upd(_ id: String, _ meta: OrderedDictionary<String, JSON>) -> JSON {
    var p: OrderedDictionary<String, JSON> = ["type": "update_agent", "id": .string(id)]
    for (key, v) in meta { p[key] = v }
    return .object(p)
}

private func del(_ id: String) -> JSON {
    .object(["type": "delete_agent", "id": .string(id)])
}

@Suite("operand gate (update_agent / delete_agent)")
struct OperandGateTests {

    // ─── the escalation ──────────────────────────────────────────────────

    /// THE ESCALATION. `<root>:[update_agent, delete_agent]` is the builder
    /// wiring; it must not be a key to every other agent's record.
    @Test func aRootGrantDoesNotReachAnUnrelatedAgent() async {
        let k = seeded()
        let victim = await plain(k, "og1_victim", ["description": "before"])
        let builder = await plain(
            k, "og1_builder",
            ["can_call": grant(["kernel_state": ["update_agent", "delete_agent"]])])
        var r = await call(k, builder, upd(victim, ["description": "pwned"]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "egress", "\(r.serialize())")
        #expect(r["error"].asString == "\(builder) may not update_agent \(victim)")
        #expect(
            r["hint"].asString
                == "\(victim) is outside your subtree: your can_call must name it with the "
                + "verb — {'\(victim)': ['update_agent']}, never a wildcard — and its "
                + "callable_by must accept you")
        #expect(k.agent(AgentId(victim))?.metaValue(forKey: "description") == "before")
        r = await call(k, builder, del(victim))
        #expect(r["reason"] == "unauthorized" && r["side"] == "egress", "\(r.serialize())")
        #expect(r["error"].asString == "\(builder) may not delete_agent \(victim)")
        #expect(k.agent(AgentId(victim)) != nil)
    }

    @Test func aDeleteLockOnAnUnrelatedAgentCannotBeCleared() async {
        let k = seeded()
        let victim = await plain(k, "og2_victim", ["delete_lock": true])
        let builder = await plain(
            k, "og2_builder",
            ["can_call": grant(["kernel_state": ["update_agent", "delete_agent"]])])
        let r = await call(k, builder, upd(victim, ["delete_lock": .null]))
        #expect(r["reason"] == "unauthorized", "\(r.serialize())")
        #expect(k.agent(AgentId(victim))?.metaValue(forKey: "delete_lock") == .bool(true))
    }

    /// `<id>: "*"` is reach, not authority over the record: the verb is named.
    @Test func aWildcardNeverCoversTheOperand() async {
        let k = seeded()
        let victim = await plain(
            k, "og3_victim", ["callable_by": grant(["og3_holder": "*"])])
        let holder = await plain(k, "og3_holder", ["can_call": grant([victim: "*"])])
        for payload in [upd(victim, ["description": "x"]), del(victim)] {
            let r = await call(k, holder, payload)
            #expect(r["reason"] == "unauthorized" && r["side"] == "egress", "\(r.serialize())")
        }
        let holder2 = await plain(k, "og3_holder2", ["can_call": grant([victim: ["*"]])])
        let r = await call(k, holder2, upd(victim, ["description": "x"]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "egress", "\(r.serialize())")
    }

    /// `$parent: ["update_agent"]` names a verb, not the operand.
    @Test func aStructuralTokenNeverNamesTheOperand() async {
        let k = seeded()
        let parent = await plain(k, "og4_parent")
        let kid = await plain(
            k, "og4_kid", parent: parent, ["can_call": grant(["$parent": ["update_agent"]])])
        let r = await call(k, kid, upd(parent, ["description": "x"]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "egress", "\(r.serialize())")
    }

    // ─── baseline: your own subtree ──────────────────────────────────────

    /// Before the fix this was refused (`unauthorized egress`) without a ROOT
    /// grant — the only grant that existed was tree-wide.
    @Test func anAgentEditsAndDeletesItsOwnSubtreeWithNoGrant() async {
        let k = seeded()
        let owner = await plain(k, "og5_owner")
        let kid = await plain(k, "og5_kid", parent: owner)
        let grandkid = await plain(k, "og5_grandkid", parent: kid)
        var r = await call(k, owner, upd(grandkid, ["description": "mine"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
        #expect(k.agent(AgentId(grandkid))?.metaValue(forKey: "description") == "mine")
        r = await call(k, owner, upd(owner, ["description": "me"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
        r = await call(k, owner, del(kid))
        #expect(r["deleted"] == .bool(true), "\(r.serialize())")
        #expect(k.agent(AgentId(grandkid)) == nil)
    }

    @Test func ownSubtreeKeepsTheSelfGrantRule() async {
        let k = seeded()
        let owner = await plain(k, "og6_owner")
        let r = await call(k, owner, upd(owner, ["can_call": grant(["kernel_state": "*"])]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "self-grant", "\(r.serialize())")
    }

    @Test func ownSubtreeKeepsTheHolderMarkRule() async {
        let k = seeded()
        let owner = await plain(k, "og7_owner")
        let vault = await plain(k, "og7_vault", parent: owner, ["holds_secrets": true])
        let r = await call(k, owner, upd(vault, ["holds_secrets": .null]))
        #expect(
            r["reason"] == "unauthorized" && r["side"] == "secret-holder", "\(r.serialize())")
        #expect(k.agent(AgentId(vault))?.metaValue(forKey: "holds_secrets") == .bool(true))
    }

    @Test func ownSubtreeKeepsTheDelegationRule() async {
        let k = seeded()
        let other = await plain(k, "og8_other")
        let owner = await plain(k, "og8_owner")
        let kid = await plain(k, "og8_kid", parent: owner)
        let r = await call(k, owner, upd(kid, ["can_call": grant([other: ["reflect"]])]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "delegation", "\(r.serialize())")
    }

    // ─── anywhere else: both halves, naming the operand ──────────────────

    @Test func anUnrelatedOperandNeedsEgressNamingItAndItsIngress() async {
        let k = seeded()
        let victim = await plain(k, "og9_victim", ["description": "before"])
        let editor = await plain(
            k, "og9_editor", ["can_call": grant([victim: ["update_agent", "delete_agent"]])])
        // egress names it; the operand has not accepted the editor yet
        var r = await call(k, editor, upd(victim, ["description": "x"]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "ingress", "\(r.serialize())")
        #expect(r["error"].asString == "\(victim) does not accept 'update_agent' from \(editor)")
        r = await call(k, editor, del(victim))
        #expect(r["reason"] == "unauthorized" && r["side"] == "ingress", "\(r.serialize())")
        // the operator opens the operand's ingress → both halves granted
        _ = await call(
            k, nil,
            upd(victim, ["callable_by": grant([editor: ["update_agent", "delete_agent"]])]))
        r = await call(k, editor, upd(victim, ["description": "after"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
        r = await call(k, editor, del(victim))
        #expect(r["deleted"] == .bool(true), "\(r.serialize())")
    }

    /// `<root>: ["update_agent"]` now means what it says: edit the ROOT.
    @Test func theRootRecordIsAnOperandLikeAnyOther() async {
        let k = seeded()
        let editor = await plain(
            k, "og10_editor", ["can_call": grant(["kernel_state": ["update_agent"]])])
        // the root's baseline callable_by accepts its direct children
        let r = await call(k, editor, upd("kernel_state", ["description": "root edited"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
    }

    @Test func aHeritableEntryNamingTheOperandReachesIt() async {
        let k = seeded()
        let victim = await plain(
            k, "og11_victim", ["callable_by": grant(["og11_kid": ["*"]])])
        let hub = await plain(
            k, "og11_hub",
            [
                "can_call": grant([
                    victim: ["verbs": ["update_agent"], "for": "descendants"]
                ])
            ])
        let kid = await plain(k, "og11_kid", parent: hub)
        var r = await call(k, kid, upd(victim, ["description": "via hub"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
        // the holder itself is never granted by its heritable entry
        r = await call(k, hub, upd(victim, ["description": "x"]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "egress", "\(r.serialize())")
    }

    @Test func theOperatorIsUngated() async {
        let k = seeded()
        let victim = await plain(k, "og12_victim")
        var r = await call(k, nil, upd(victim, ["description": "op"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
        r = await call(k, nil, del(victim))
        #expect(r["deleted"] == .bool(true), "\(r.serialize())")
    }

    /// No operand to gate: the root grant decides, and the verb says what is
    /// wrong (as an unknown `parent_id` does for create_agent).
    @Test func anUnknownOperandIsAnsweredByTheVerb() async {
        let k = seeded()
        let builder = await plain(
            k, "og13_builder", ["can_call": grant(["kernel_state": ["update_agent"]])])
        let r = await call(k, builder, upd("og13_nobody", ["description": "x"]))
        #expect(r["reason"] == "unknown_agent", "\(r.serialize())")
    }

    // ─── a relayed foreign caller owns no subtree here (audit G-1) ───────

    /// Measured before the fix: a foreign caller a leg relays — refused `reflect`
    /// on the leg's child — edited and deleted that child and re-pinned the leg's
    /// own record: the gate took the LEG's own-subtree baseline on its behalf.
    @Test func aRelayedCallerDoesNotInheritTheLegsSubtree() async {
        let k = seeded()
        let leg = await plain(k, "og14_leg", ["peer_guid": "g9"])
        let kid = await plain(k, "og14_kid", parent: leg, ["description": "before"])
        for (verb, id, payload) in [
            ("update_agent", kid, upd(kid, ["description": "pwned"])),
            ("delete_agent", kid, del(kid)),
            ("update_agent", leg, upd(leg, ["peer_guid": "g0"])),
        ] {
            let r = await relayed(k, leg, "g9|mallory", payload)
            #expect(r["reason"] == "unauthorized" && r["side"] == "egress", "\(r.serialize())")
            #expect(r["error"].asString == "\(leg) may not \(verb) \(id)")
        }
        #expect(k.agent(AgentId(kid))?.metaValue(forKey: "description") == "before")
        #expect(k.agent(AgentId(leg))?.metaValue(forKey: "peer_guid") == "g9")
        // the leg on its OWN behalf keeps its baseline
        let r = await call(k, leg, upd(kid, ["description": "mine"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
    }

    /// Egress is the leg's `can_call` naming the operand; ingress is the
    /// operand's `callable_by` against `<guid>|<agent>` — accepting the leg (the
    /// child's `$parent`) is not accepting its peer.
    @Test func aRelayedCallerIsJudgedOnTheOperandAsItsQualifiedId() async {
        let k = seeded()
        let leg = await plain(
            k, "og15_leg", ["can_call": grant(["og15_kid": ["update_agent"]])])
        let kid = await plain(k, "og15_kid", parent: leg, ["description": "before"])
        var r = await relayed(k, leg, "g9|scout", upd(kid, ["description": "x"]))
        #expect(r["reason"] == "unauthorized" && r["side"] == "ingress", "\(r.serialize())")
        #expect(r["error"].asString == "\(kid) does not accept 'update_agent' from g9|scout")
        _ = await call(k, nil, upd(kid, ["callable_by": grant(["g9|scout": ["update_agent"]])]))
        r = await relayed(k, leg, "g9|scout", upd(kid, ["description": "after"]))
        #expect(r["updated"] == .bool(true), "\(r.serialize())")
        #expect(k.agent(AgentId(kid))?.metaValue(forKey: "description") == "after")
    }

    /// Measured before the fix (audit 2026-10-03, AV-3): kernel verbs at the root
    /// skip ingress, so a foreign caller relayed by a direct child of the root got
    /// `list_agents` on the leg's `$parent:*` alone while `reflect` on the same
    /// root was refused. The root's `callable_by` must accept `<guid>|<agent>` or
    /// `<guid>|*`; a local caller is unchanged.
    @Test func aRelayedCallerNeedsTheRootsCallableByOnKernelVerbs() async {
        let k = seeded()
        let leg = await plain(k, "og16_leg", ["can_call": grant(["kernel_state": ["ps"]])])
        for verb in ["list_agents", "ps"] {
            var r = await relayed(k, leg, "g9|mallory", .object(["type": .string(verb)]))
            #expect(r["reason"] == "unauthorized" && r["side"] == "ingress", "\(r.serialize())")
            #expect(r["error"].asString == "kernel_state does not accept '\(verb)' from g9|mallory")
            #expect(
                r["hint"].asString
                    == "a foreign caller is accepted only by name: callable_by "
                    + "{'g9|mallory': [...]} or {'g9|*': [...]} on kernel_state")
            // the leg on its OWN behalf: egress is the whole check, as before
            r = await call(k, leg, .object(["type": .string(verb)]))
            #expect(r.asObject?["error"] == nil, "\(r.serialize())")
        }
        let root = k.agent(AgentId("kernel_state"))
        var rootCb = root?.metaValue(forKey: "callable_by")?.asObject ?? [:]
        rootCb["g9|*"] = ["list_agents"]
        rootCb["g9|mallory"] = ["ps"]
        _ = await call(k, nil, upd("kernel_state", ["callable_by": .object(rootCb)]))
        var r = await relayed(k, leg, "g9|mallory", .object(["type": "list_agents"]))
        #expect(r.asObject?["error"] == nil && r.asObject?["agents"] != nil, "\(r.serialize())")
        r = await relayed(k, leg, "g9|mallory", .object(["type": "ps"]))
        #expect(r.asObject?["error"] == nil, "\(r.serialize())")
        // a guid the root does not name is still refused
        r = await relayed(k, leg, "g7|eve", .object(["type": "list_agents"]))
        #expect(r["side"] == "ingress", "\(r.serialize())")
    }
}

/// `payload` to the root as bridge leg `leg` relaying the foreign `caller`.
private func relayed(_ k: Kernel, _ leg: String, _ caller: String, _ payload: JSON) async -> JSON {
    await KernelTaskLocals.withRemoteCaller(leg: AgentId(leg), address: caller) {
        await call(k, leg, payload)
    }
}
