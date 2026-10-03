// `$subtree:<X>` (callable_by) and the HERITABLE `can_call` entry — the two grant
// forms for agents spawned dynamically under one owner. Mirror of python
// `tests/test_grants.py` (subtree/heritable) and the shared corpus
// `contract/agentic/specs/subtree_grants.yaml`.

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

private func heritable(_ verbs: [String]) -> JSON {
    .object(["verbs": .array(verbs.map { .string($0) }), "for": .string("descendants")])
}

private func list(_ verbs: String...) -> JSON { .array(verbs.map { .string($0) }) }

// ─── the matcher, pure ─────────────────────────────────────────────

/// holder `served`; the live tree is hub → kid → grandkid.
private let tree: [String: String] = ["kid": "hub", "grandkid": "kid"]
private func inTree(_ other: String, _ x: String) -> Bool {
    var node: String? = other
    while let n = node {
        if n == x { return true }
        node = tree[n]
    }
    return false
}

private func ingress(
    _ grant: JSON, _ verb: String, _ other: String, enforce: Bool = false,
    resolver: ((String, String) -> Bool)? = inTree
) -> Bool {
    authorizeCall(
        grant: grant, verb: verb, otherId: other, holderId: "served", parentId: "kernel_state",
        childIds: [], loaderId: "kernel_state", enforcePrivileged: enforce, inSubtree: resolver
    ).allowed
}

@Test func subtreeMatchesTheOwnerAndEveryLiveDescendant() {
    let g: JSON = .object(["$subtree:hub": list("list")])
    #expect(ingress(g, "list", "hub"))
    #expect(ingress(g, "list", "kid"))
    #expect(ingress(g, "list", "grandkid"))
    #expect(!ingress(g, "list", "outsider"))
    #expect(!ingress(g, "mkdir", "kid"))
}

@Test func subtreeNeverMatchesAForeignId() {
    let g: JSON = .object(["$subtree:hub": .string("*")])
    // even a resolver that would say yes is never asked about a `guid|agent`
    #expect(!ingress(g, "list", "g1|hub", resolver: { _, _ in true }))
    #expect(!ingress(g, "list", "g1|kid", resolver: { _, _ in true }))
}

@Test func subtreeOfAnAbsentOwnerOrWithNoResolverMatchesNothing() {
    #expect(!ingress(.object(["$subtree:ghost": .string("*")]), "list", "kid"))
    #expect(
        !ingress(.object(["$subtree:": .string("*")]), "list", "kid", resolver: { _, _ in true }))
    #expect(!ingress(.object(["$subtree:hub": .string("*")]), "list", "kid", resolver: nil))
}

@Test func subtreeIsStructuralForPrivilegedVerbs() {
    // `*` never covers a privileged verb where enforcePrivileged applies; a NAMED one does.
    #expect(!ingress(.object(["$subtree:hub": .string("*")]), "allow", "kid", enforce: true))
    #expect(!ingress(.object(["$subtree:hub": list("*")]), "allow", "kid", enforce: true))
    #expect(ingress(.object(["$subtree:hub": list("allow")]), "allow", "kid", enforce: true))
    #expect(ingress(.object(["$subtree:hub": .string("*")]), "list", "kid", enforce: true))
    // ingress posture: the wildcard covers it (egress already guards escalation)
    #expect(ingress(.object(["$subtree:hub": .string("*")]), "allow", "kid"))
}

@Test func aHeritableValueNeverGrantsItsHolder() {
    let g: JSON = .object(["served": heritable(["list", "*"])])
    let d = authorizeCall(
        grant: g, verb: "list", otherId: "served", holderId: "hub", parentId: "kernel_state",
        childIds: ["kid"], loaderId: "kernel_state")
    #expect(!d.allowed)
}

@Test func heritableEgressNamesOrWildcardsTheVerb() {
    func her(_ grants: [JSON], _ verb: String, _ target: String = "served") -> Bool {
        authorizeHeritable(
            ancestorGrants: grants, verb: verb, targetId: target, loaderId: "kernel_state"
        ).allowed
    }
    let named: JSON = .object(["served": heritable(["list", "allow"])])
    #expect(her([named], "list"))
    #expect(her([named], "allow"))  // a NAMED privileged verb is covered
    #expect(!her([named], "mkdir"))
    #expect(!her([named], "list", "other"))  // keyed by the target's real id
    let wild: JSON = .object(["served": heritable(["*"])])
    #expect(her([wild], "list"))
    #expect(!her([wild], "allow"))  // `*` never covers a privileged verb
    // a plain list or a malformed map under the key is not heritable
    #expect(!her([.object(["served": list("list")])], "list"))
    #expect(
        !her(
            [.object(["served": .object(["verbs": list("list"), "for": .string("siblings")])])],
            "list"))
    // first allow wins, nearest first; a refusing ancestor does not stop the walk
    #expect(her([.object([:]), named], "list"))
}

@Test func heritableWildcardNeverCoversWatchOnTheRoot() {
    func her(_ grants: [JSON], _ verb: String, _ target: String) -> Bool {
        authorizeHeritable(
            ancestorGrants: grants, verb: verb, targetId: target, loaderId: "kernel_state"
        ).allowed
    }
    // audit KV-2: watch on the root needs an explicit named grant
    let wild: JSON = .object(["kernel_state": heritable(["*"])])
    #expect(!her([wild], "watch", "kernel_state"))
    #expect(her([wild], "list_agents", "kernel_state"))
    #expect(her([.object(["kernel_state": heritable(["watch"])])], "watch", "kernel_state"))
    // every other watch is an ordinary verb
    #expect(her([.object(["served": heritable(["*"])])], "watch", "served"))
}

@Test func heritableShapeIsExact() {
    #expect(heritableVerbs(heritable(["list"])) == ["list"])
    #expect(heritableVerbs(heritable([])) == nil)
    #expect(heritableVerbs(.object(["verbs": list("list")])) == nil)
    #expect(
        heritableVerbs(
            .object(["verbs": list("list"), "for": .string("descendants"), "x": .bool(true)]))
            == nil)
    #expect(
        heritableVerbs(.object(["verbs": .array([.integer(1)]), "for": .string("descendants")]))
            == nil)
    #expect(
        heritableVerbs(.object(["verbs": .string("list"), "for": .string("descendants")])) == nil)
}

// ─── the kernel gate ───────────────────────────────────────────────

private func mkKernel() async -> Kernel {
    let registry = BundleRegistry()
    registry.register("noop.tools", Noop())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    for (id, parent, extra) in [
        (
            "served", "kernel_state",
            [
                "callable_by": JSON.object([
                    "$subtree:hub": list("list", "allow", "allowed"),
                    "$subtree:hub_wild": list("list", "allow"),
                ])
            ]
        ),
        ("outsider", "kernel_state", ["can_call": .object(["served": list("list")])]),
        (
            "hub", "kernel_state",
            ["can_call": .object(["served": heritable(["list", "allow", "allowed"])])]
        ),
        ("kid", "hub", [:]),
        ("hub_wild", "kernel_state", ["can_call": .object(["served": heritable(["*"])])]),
        ("wild_kid", "hub_wild", [:]),
    ] as [(String, String, [String: JSON])] {
        let r = await create(kernel, id, parent: parent, extra)
        #expect(r["id"].asString == id, "\(r.serialize())")
    }
    return kernel
}

private func create(
    _ kernel: Kernel, _ id: String, parent: String, _ extra: [String: JSON] = [:]
) async -> JSON {
    var p: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"), "parent_id": .string(parent), "id": .string(id),
        "handler_module": .string("noop.tools"),
    ]
    for (k, v) in extra.sorted(by: { $0.key < $1.key }) { p[k] = v }
    return await kernel.send([Item(AgentId("kernel"), .object(p))])[0].result
}

private func reach(_ kernel: Kernel, _ from: String, _ to: String, _ verb: String) async -> JSON {
    await kernel.sendAs(
        sender: AgentId(from), target: AgentId(to), payload: .object(["type": .string(verb)]))
}

@Suite("subtree + heritable grants")
struct SubtreeGrantTests {
    @Test func aDescendantReachesThroughBothForms() async {
        let k = await mkKernel()
        #expect(await reach(k, "kid", "served", "list")["ok"].asBool == true)
        // a NAMED privileged verb crosses both forms
        #expect(await reach(k, "kid", "served", "allow")["ok"].asBool == true)
        #expect(await reach(k, "kid", "served", "allowed")["ok"].asBool == true)
    }

    @Test func theHolderIsNotCoveredByItsOwnHeritableEntry() async {
        let k = await mkKernel()
        let r = await reach(k, "hub", "served", "list")
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "egress", "\(r.serialize())")
    }

    @Test func aVerbTheEntryDoesNotNameDiesAtEgress() async {
        let k = await mkKernel()
        let r = await reach(k, "kid", "served", "mkdir")
        #expect(r["side"].asString == "egress", "\(r.serialize())")
    }

    @Test func anAgentSpawnedAfterTheGrantsIsCovered() async {
        let k = await mkKernel()
        #expect(await create(k, "grandkid", parent: "kid")["id"].asString == "grandkid")
        #expect(await reach(k, "grandkid", "served", "list")["ok"].asBool == true)
        #expect(k.inSubtree("grandkid", of: "hub"))
        #expect(!k.inSubtree("grandkid", of: "ghost"))
        #expect(!k.inSubtree("ghost", of: "hub"))
    }

    @Test func anOutsiderIsRefusedByTheTarget() async {
        let k = await mkKernel()
        let r = await reach(k, "outsider", "served", "list")
        #expect(r["side"].asString == "ingress", "\(r.serialize())")
    }

    @Test func aWildcardEntryNeverCoversAPrivilegedVerb() async {
        let k = await mkKernel()
        #expect(await reach(k, "wild_kid", "served", "list")["ok"].asBool == true)
        let r = await reach(k, "wild_kid", "served", "allow")
        #expect(r["side"].asString == "egress", "\(r.serialize())")
    }

    @Test func heritableEgressCoversPlacement() async {
        // create_agent under `served` — privileged, so only a NAMED heritable verb counts
        let k = await mkKernel()
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("served"),
                    "callable_by": .object(["$subtree:hub": list("create_agent")]),
                ]))
        ])
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("hub"),
                    "can_call": .object(["served": heritable(["create_agent"])]),
                ]))
        ])
        let payload: JSON = .object([
            "type": .string("create_agent"), "parent_id": .string("served"),
            "id": .string("planted"), "handler_module": .string("noop.tools"),
        ])
        let ok = await k.sendAs(sender: "kid", target: "kernel", payload: payload)
        #expect(ok["id"].asString == "planted", "\(ok.serialize())")
        let holder = await k.sendAs(sender: "hub", target: "kernel", payload: payload)
        #expect(holder["side"].asString == "egress", "\(holder.serialize())")
    }

    // ── conferral ──

    @Test func aHolderConfersASubsetToItsChild() async {
        let k = await mkKernel()
        let r = await k.sendAs(
            sender: "hub", target: "kernel",
            payload: .object([
                "type": .string("create_agent"), "parent_id": .string("hub"),
                "id": .string("hub_child"),
                "handler_module": .string("noop.tools"),
                "can_call": .object(["served": heritable(["list"])]),
            ]))
        #expect(r["id"].asString == "hub_child", "\(r.serialize())")
        // `*` confers `*`
        let w = await k.sendAs(
            sender: "hub_wild", target: "kernel",
            payload: .object([
                "type": .string("create_agent"), "parent_id": .string("hub_wild"),
                "id": .string("w_child"),
                "handler_module": .string("noop.tools"),
                "can_call": .object(["served": heritable(["*", "list"])]),
            ]))
        #expect(w["id"].asString == "w_child", "\(w.serialize())")
    }

    @Test func aBeneficiaryCannotConferTheEntry() async {
        let k = await mkKernel()
        let r = await k.sendAs(
            sender: "kid", target: "kernel",
            payload: .object([
                "type": .string("create_agent"), "parent_id": .string("kid"),
                "id": .string("kid_child"),
                "handler_module": .string("noop.tools"),
                "can_call": .object(["served": heritable(["list"])]),
            ]))
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "delegation", "\(r.serialize())")
        #expect(
            r["error"].asString
                == "kid cannot confer a heritable 'list' on served: you do not hold that "
                + "heritable entry — only the operator, or a holder of the same entry, "
                + "confers one (a subset of it)", "\(r.serialize())")
        #expect(r["hint"].asString == fromAbove)
        #expect(k.agent("kid_child") == nil)
    }

    @Test func aHolderCannotConferMoreThanItHolds() async {
        let k = await mkKernel()
        func confer(_ sender: String, _ verbs: [String]) async -> JSON {
            await k.sendAs(
                sender: AgentId(sender), target: "kernel",
                payload: .object([
                    "type": .string("create_agent"), "parent_id": .string(sender),
                    "id": .string("\(sender)_c"), "handler_module": .string("noop.tools"),
                    "can_call": .object(["served": heritable(verbs)]),
                ]))
        }
        #expect(await confer("hub", ["list", "disallow"])["side"].asString == "delegation")
        #expect(await confer("hub", ["*"])["side"].asString == "delegation")  // `*` needs `*`
        // `*` held covers an ordinary verb, never a privileged one
        #expect(await confer("hub_wild", ["allow"])["side"].asString == "delegation")
    }

    // ── write-time shape ──

    @Test func malformedShapesAreRefusedAtWrite() async {
        let k = await mkKernel()
        let cases: [(String, String, JSON, String)] = [
            (
                "outsider", "can_call", .object(["$subtree:hub": list("list")]),
                "can_call['$subtree:hub']: `$subtree:` is callable_by-only — it names who may call you, never whom you may call"
            ),
            (
                "served", "callable_by", .object(["$subtree:": list("list")]),
                "callable_by['$subtree:']: `$subtree:` names one LOCAL agent id (`$subtree:<id>`, no `|`)"
            ),
            (
                "served", "callable_by", .object(["$subtree:g1|x": list("list")]),
                "callable_by['$subtree:g1|x']: `$subtree:` names one LOCAL agent id (`$subtree:<id>`, no `|`)"
            ),
            (
                "hub", "can_call",
                .object(["served": .object(["verbs": list("list"), "for": .string("siblings")])]),
                "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \"for\": \"descendants\"}"
            ),
            (
                "hub", "can_call", .object(["$parent": heritable(["list"])]),
                "can_call['$parent']: a heritable entry names a literal target id, never a token"
            ),
            (
                "served", "callable_by", .object(["hub": heritable(["list"])]),
                "callable_by['hub']: a heritable entry {\"verbs\": [...], \"for\": \"descendants\"} is can_call-only"
            ),
            (
                "hub", "can_call", .object(["served": .object(["verbs": list("list")])]),
                "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \"for\": \"descendants\"}"
            ),
            (
                "hub", "can_call",
                .object([
                    "served": .object([
                        "verbs": list("list"), "for": .string("descendants"), "why": .string("x"),
                    ])
                ]),
                "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \"for\": \"descendants\"}"
            ),
            (
                "hub", "can_call", .object(["served": heritable([])]),
                "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \"for\": \"descendants\"}"
            ),
            (
                "hub", "can_call",
                .object([
                    "served": .object(["verbs": .string("list"), "for": .string("descendants")])
                ]),
                "can_call['served'] must be exactly {\"verbs\": [\"<verb>\", …], \"for\": \"descendants\"}"
            ),
            (
                "hub", "can_call", .object(["served": .integer(3)]),
                "can_call['served'] must be \"*\", a list of verb strings, or (can_call) {\"verbs\": [...], \"for\": \"descendants\"}"
            ),
        ]
        for (id, field, grant, text) in cases {
            let r = await k.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("update_agent"), "id": .string(id), field: grant,
                    ]))
            ])[0].result
            #expect(
                r["reason"].asString == "invalid_meta", "\(grant.serialize()) → \(r.serialize())")
            #expect(r["error"].asString == text, "\(r.serialize())")
        }
        // create_agent refuses the same shapes
        let c = await create(
            k, "bad", parent: "kernel_state", ["callable_by": .object(["hub": heritable(["list"])])]
        )
        #expect(c["reason"].asString == "invalid_meta", "\(c.serialize())")
        #expect(k.agent("bad") == nil)
    }

    // ── persistence ──

    @Test func aHeritableEntryRoundTripsVerbatim() async throws {
        let k = await mkKernel()
        let want = heritable(["list", "allow", "allowed"])
        let fresh = Kernel(storage: .inMemory, bundles: k.bundles)
        try fresh.loadJSON(k.saveJSON())
        // the snapshot keeps the map (its key order is the encoder's, not the record's)
        let reloaded = fresh.agent("hub")?.metaValue(forKey: "can_call")?["served"] ?? .null
        #expect(heritableVerbs(reloaded) == ["list", "allow", "allowed"], "\(reloaded)")
        #expect(
            fresh.agent("served")?.metaValue(forKey: "callable_by")?["$subtree:hub"]
                == list("list", "allow", "allowed"))
        // the reloaded kernel judges it the same way
        #expect(await reach(fresh, "kid", "served", "list")["ok"].asBool == true)
        #expect(await reach(fresh, "hub", "served", "list")["side"].asString == "egress")

        // the on-disk record (YAML) keeps the map too
        let file = FileManager.default.temporaryDirectory
            .appendingPathComponent("subtree-\(UUID().uuidString).yaml")
        defer { try? FileManager.default.removeItem(at: file) }
        let record = try #require(k.agent("hub")).record().ordered
        try Persistence.recordBytes(record).write(to: file)
        let back = try #require(Persistence.readRecord(file))
        #expect(back["can_call"]?["served"] == want)

        // reflect-by-record shows it verbatim
        let listed = await k.send([
            Item(AgentId("kernel"), .object(["type": .string("list_agents")]))
        ])[0].result
        let hub = (listed["agents"].asArray ?? []).first { $0["id"].asString == "hub" }
        #expect(hub?["can_call"]["served"] == want, "\(listed.serialize())")
    }
}
