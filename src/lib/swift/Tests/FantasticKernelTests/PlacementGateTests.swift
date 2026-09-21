// `create_agent` is gated on the PLACEMENT TARGET, and `$loader` is real reach.
//
// Both measured 2026-09-02 against the live python kernel and ported here:
//
// - An agent holding the ordinary "may author tools" wiring
//   (`<root>:[create_agent]`) could plant a child under ANY agent, and the
//   child's stamped `$parent:*` baseline handed it unrestricted reach into that
//   agent. The gate asked about the ROOT (which merely ANSWERS the verb) instead
//   of the parent (which is ACTED UPON).
// - `checkConferral` skipped every `$`-prefixed key as "structural", but
//   `$loader` resolves to the ROOT for whoever holds it, so conferring it handed
//   over the kernel's control surface.

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

@Suite("placement gate")
struct PlacementGateTests {

    private func freshKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    /// Create as the OPERATOR (ungated) — the fixture builder.
    @discardableResult
    private func seed(
        _ kernel: Kernel, _ id: String, parent: String,
        _ extra: OrderedDictionary<String, JSON> = [:]
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"), "parent_id": .string(parent),
            "id": .string(id), "handler_module": .string("noop.tools"),
        ]
        for (k, v) in extra { p[k] = v }
        return await kernel.send([Item(AgentId("kernel"), .object(p))])[0].result
    }

    /// Create AS `sender` — the gated path under test.
    private func createAs(
        _ kernel: Kernel, _ sender: String, _ payload: OrderedDictionary<String, JSON>
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"), "handler_module": .string("noop.tools"),
        ]
        for (k, v) in payload { p[k] = v }
        return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
            await kernel.send([Item(AgentId("kernel"), .object(p))])[0].result
        }
    }

    private let baseline: OrderedDictionary<String, JSON> = [
        "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
    ]

    @Test func anAgentCreatesUnderItselfWithNoGrant() async {
        let kernel = freshKernel()
        await seed(kernel, "mid", parent: "kernel_state")
        await seed(kernel, "deep", parent: "mid")  // a GRANDCHILD
        let r = await createAs(
            kernel, "deep", ["parent_id": .string("deep"), "id": .string("kid")])
        #expect(r["id"].asString == "kid", "\(r.serialize())")
    }

    @Test func creatingUnderOwnDescendantIsBaseline() async {
        let kernel = freshKernel()
        await seed(kernel, "mid", parent: "kernel_state")
        await seed(kernel, "kid", parent: "mid")
        let r = await createAs(
            kernel, "mid", ["parent_id": .string("kid"), "id": .string("grandkid")])
        #expect(r["id"].asString == "grandkid", "\(r.serialize())")
    }

    @Test func aRootCreateGrantDoesNotReachASibling() async {
        // THE ESCALATION. `<root>:[create_agent]` is the ordinary authoring
        // grant; it must not be a key to every other agent's subtree.
        let kernel = freshKernel()
        await seed(kernel, "victim", parent: "kernel_state")
        var can = baseline
        can["kernel_state"] = .array([.string("*"), .string("create_agent"), .string("update_agent")])
        await seed(kernel, "author", parent: "kernel_state", ["can_call": .object(can)])
        let r = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("mole")])
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "egress", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("create under"), "\(r.serialize())")
        #expect(kernel.agent(AgentId("mole")) == nil)
    }

    @Test func creatingUnderAnUnrelatedAgentNeedsBothHalves() async {
        let kernel = freshKernel()
        await seed(kernel, "victim", parent: "kernel_state")
        var can = baseline
        can["victim"] = .array([.string("create_agent")])
        await seed(kernel, "author", parent: "kernel_state", ["can_call": .object(can)])
        // Egress granted, ingress not yet: the parent must accept a child.
        let r = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("a")])
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "ingress", "\(r.serialize())")
        // Open the parent's ingress → the edge exists.
        var callable = baseline
        callable["author"] = .array([.string("create_agent")])
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("victim"),
                    "callable_by": .object(callable),
                ]))
        ])[0].result
        let ok = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("b")])
        #expect(ok["id"].asString == "b", "\(ok.serialize())")
    }

    @Test func loaderTokenInAConferredGrantIsBounded() async {
        let kernel = freshKernel()
        await seed(kernel, "mid", parent: "kernel_state")
        var can = baseline
        can["kernel_state"] = .array([.string("create_agent")])
        await seed(kernel, "deep", parent: "mid", ["can_call": .object(can)])
        // The granter itself cannot reach the root's task table…
        let denied = await KernelTaskLocals.$currentSender.withValue(AgentId("deep")) {
            await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0]
                .result
        }
        #expect(denied["reason"].asString == "unauthorized", "\(denied.serialize())")
        // …so it cannot hand that reach to a child either.
        let r = await createAs(
            kernel, "deep",
            [
                "parent_id": .string("deep"), "id": .string("kid"),
                "can_call": .object(["$loader": .string("*")]),
            ])
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "delegation", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("$loader"), "\(r.serialize())")
        #expect(kernel.agent(AgentId("kid")) == nil)
    }

    @Test func aGranterThatHoldsTheRootMayConferLoader() async {
        let kernel = freshKernel()
        var can = baseline
        can["kernel_state"] = .string("*")
        await seed(kernel, "holder", parent: "kernel_state", ["can_call": .object(can)])
        let r = await createAs(
            kernel, "holder",
            [
                "parent_id": .string("holder"), "id": .string("kid"),
                "can_call": .object(["$loader": .string("*")]),
            ])
        #expect(r["id"].asString == "kid", "\(r.serialize())")
    }

    // MARK: - denial shape (C20)

    /// Python builds all four unauthorized denials through `refusal(...,
    /// hint=..., side=...)`, and `refusal` writes `error, reason, hint` before
    /// the extras — so `side` is LAST. Swift emitted `side` before `hint`.
    @Test func gateDenialKeysAreErrorReasonHintSide() async {
        let kernel = freshKernel()
        await seed(kernel, "victim", parent: "kernel_state")
        var can = baseline
        can["kernel_state"] = .array([.string("create_agent")])
        await seed(kernel, "author", parent: "kernel_state", ["can_call": .object(can)])

        // create_agent, placement egress.
        let egressCreate = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("mole")])
        #expect(
            egressCreate.asObject?.keys.elements == ["error", "reason", "hint", "side"],
            "\(egressCreate.serialize())")

        // create_agent, placement ingress (egress opened, parent still closed).
        var open = baseline
        open["author"] = .array([.string("create_agent")])
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("author"),
                    "can_call": .object(["victim": .array([.string("create_agent")])]),
                ]))
        ])[0].result
        let ingressCreate = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("mole2")])
        #expect(ingressCreate["side"].asString == "ingress", "\(ingressCreate.serialize())")
        #expect(
            ingressCreate.asObject?.keys.elements == ["error", "reason", "hint", "side"],
            "\(ingressCreate.serialize())")

        // Ordinary verb, egress: `{sender} may not call {verb!r} on {target}`.
        await seed(kernel, "caller", parent: "kernel_state")
        let egress = await KernelTaskLocals.$currentSender.withValue(AgentId("caller")) {
            await kernel.send([
                Item(AgentId("victim"), .object(["type": .string("reflect")]))
            ])[0].result
        }
        #expect(
            egress.asObject?.keys.elements == ["error", "reason", "hint", "side"],
            "\(egress.serialize())")
        #expect(
            egress["error"].asString == "caller may not call 'reflect' on victim",
            "\(egress.serialize())")

        // Ordinary verb, ingress: `{target} does not accept {verb!r} from {sender}`.
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("caller"),
                    "can_call": .object(["victim": .array([.string("reflect")])]),
                ]))
        ])[0].result
        let ingress = await KernelTaskLocals.$currentSender.withValue(AgentId("caller")) {
            await kernel.send([
                Item(AgentId("victim"), .object(["type": .string("reflect")]))
            ])[0].result
        }
        #expect(
            ingress.asObject?.keys.elements == ["error", "reason", "hint", "side"],
            "\(ingress.serialize())")
        #expect(
            ingress["error"].asString == "victim does not accept 'reflect' from caller",
            "\(ingress.serialize())")
    }

    /// Conferral refusals quote with `{x!r}` and render an absent parent as
    /// python's `None` (swift said "none").
    @Test func conferralRefusalsUsePythonReprAndNone() async {
        let kernel = freshKernel()
        // A GRANDCHILD: its `$parent` baseline resolves to `mid`, so it holds no
        // reach on the root and every probe below really fails.
        await seed(kernel, "mid", parent: "kernel_state")
        await seed(kernel, "granter", parent: "mid")

        let token = await createAs(
            kernel, "granter",
            [
                "parent_id": .string("granter"), "id": .string("k1"),
                "can_call": .object(["$everything": .string("*")]),
            ])
        #expect(
            (token["error"].asString ?? "").hasPrefix("granter cannot confer '$everything': "),
            "\(token.serialize())")

        // The root has no parent — python interpolates the bare None.
        let lateral = await createAs(
            kernel, "granter",
            [
                "parent_id": .string("granter"), "id": .string("k2"),
                "can_call": .object(["kernel_state": .array([.string("ps")])]),
            ])
        #expect(
            lateral["error"].asString
                == "granter cannot delegate 'ps' on kernel_state: kernel_state is not "
                + "your child "
                + "(its parent is None) and not in your can_call — you may only confer "
                + "can_call you already hold; create the agents you will wire as YOUR children",
            "\(lateral.serialize())")
        #expect(lateral["parent"] == .null, "\(lateral.serialize())")

        // A `$loader` the granter does not hold.
        let loader = await createAs(
            kernel, "granter",
            [
                "parent_id": .string("granter"), "id": .string("k3"),
                "can_call": .object(["$loader": .array([.string("ps")])]),
            ])
        #expect(
            (loader["error"].asString ?? "").hasPrefix(
                "granter cannot delegate 'ps' on $loader: "), "\(loader.serialize())")

        // A target that is not an agent at all.
        let missing = await createAs(
            kernel, "granter",
            [
                "parent_id": .string("granter"), "id": .string("k4"),
                "can_call": .object(["ghost": .array([.string("ps")])]),
            ])
        #expect(
            (missing["error"].asString ?? "").hasPrefix(
                "granter cannot delegate 'ps' on ghost: no such agent"),
            "\(missing.serialize())")
    }

    @Test func anUnknownStructuralTokenIsRefused() async {
        let kernel = freshKernel()
        await seed(kernel, "a", parent: "kernel_state")
        let r = await createAs(
            kernel, "a",
            [
                "parent_id": .string("a"), "id": .string("kid"),
                "can_call": .object(["$everything": .string("*")]),
            ])
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        #expect(r["side"].asString == "delegation", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("$everything"), "\(r.serialize())")
    }

    /// C22 — a JOINER is gated with the execution's ORIGINAL PAYLOAD, not with a
    /// `{type: <verb>}` rebuilt from the verb alone. `create_agent` is the case
    /// that proves it: the gate's operand is `parent_id`, which lives in the
    /// ARGUMENTS, so a joiner rebuilt from the verb was asked a different
    /// question than the call it is about to be handed.
    @Test func aJoinerIsGatedWithTheExecutionsOriginalPayload() async {
        let kernel = freshKernel()
        await seed(kernel, "alpha", parent: "kernel_state")
        await seed(kernel, "beta", parent: "kernel_state")

        // `alpha` creates INSIDE ITS OWN SUBTREE — baseline, no grant needed —
        // under a name, so the row is joinable.
        let made = await KernelTaskLocals.$currentSender.withValue(AgentId("alpha")) {
            await kernel.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("create_agent"), "parent_id": .string("alpha"),
                        "id": .string("tool"), "handler_module": .string("noop.tools"),
                    ]), id: "job-c22")
            ])[0].result
        }
        #expect(made["id"].asString == "tool", "\(made.serialize())")

        // `beta` re-sends THE SAME NAME. The row is settled, so this is a join —
        // and the gate must be asked about placing under `alpha`, which beta may
        // not do. The refusal names `alpha`, which is only knowable from the
        // stored payload.
        let joined = await KernelTaskLocals.$currentSender.withValue(AgentId("beta")) {
            await kernel.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("create_agent"), "parent_id": .string("beta"),
                        "id": .string("other"), "handler_module": .string("noop.tools"),
                    ]), id: "job-c22")
            ])[0].result
        }
        #expect(joined["reason"].asString == "unauthorized", "\(joined.serialize())")
        #expect(
            joined["error"].asString?.contains("alpha") == true,
            "the joiner was gated on its OWN payload, not the execution's: \(joined.serialize())")
    }
}
