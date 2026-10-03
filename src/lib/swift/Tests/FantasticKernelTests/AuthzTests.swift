// Pure unit tests for the call-security grant engine — mirror of the python
// `tests/test_grants.py` and rust `authz/tests.rs`. Deny-all is the law; a grant
// on the sender opens it.

import FantasticJSON
import OrderedCollections
import Testing

@testable import FantasticKernel

// holder "worker", parent "boss", one child "kid", loader "root". Egress posture.
private func auth(_ grant: JSON, _ verb: String, _ other: String) -> Decision {
    authorizeCall(
        grant: grant, verb: verb, otherId: other,
        holderId: "worker", parentId: "boss", childIds: ["kid"], loaderId: "root")
}

// Ingress posture — privileged-no-wildcard is NOT enforced.
private func authIngress(_ grant: JSON, _ verb: String, _ other: String) -> Decision {
    authorizeCall(
        grant: grant, verb: verb, otherId: other,
        holderId: "worker", parentId: "boss", childIds: ["kid"], loaderId: "root",
        enforcePrivileged: false)
}

@Test func absentGrantDeniesEverything() {
    let d = auth(.null, "send", "peer")
    #expect(!d.allowed)
    #expect(!d.hint.isEmpty)  // a denial always teaches (from above)
}

@Test func emptyGrantDenies() {
    #expect(!auth(.object([:]), "send", "peer").allowed)
}

@Test func selfTokenResolvesToSenderId() {
    let g: JSON = .object(["$self": .string("*")])
    #expect(auth(g, "ping", "worker").allowed)
    #expect(!auth(g, "ping", "boss").allowed)
}

@Test func parentTokenResolvesToParentId() {
    let g: JSON = .object(["$parent": .string("*")])
    #expect(auth(g, "ping", "boss").allowed)
    #expect(!auth(g, "ping", "worker").allowed)
}

@Test func childrenTokenMatchesOnlyDirectChildren() {
    let g: JSON = .object(["$children": .string("*")])
    #expect(auth(g, "ping", "kid").allowed)
    #expect(!auth(g, "ping", "stranger").allowed)
}

@Test func loaderTokenResolvesToPassedLoaderId() {
    let g: JSON = .object(["$loader": .string("*")])
    #expect(auth(g, "load", "root").allowed)
    #expect(!auth(g, "load", "boss").allowed)
}

@Test func baselineReachesSelfParentAndChildren() {
    for g in [defaultCanCall(), defaultCallableBy()] {
        #expect(auth(g, "send", "worker").allowed)  // $self
        #expect(auth(g, "send", "boss").allowed)  // $parent
        #expect(auth(g, "send", "kid").allowed)  // $children
        #expect(!auth(g, "send", "stranger").allowed)
    }
}

@Test func ingressWildcardAcceptsPrivilegedVerbs() {
    // enforcePrivileged: false (ingress) — a callee's $children:* legitimately
    // accepts a privileged verb; escalation is blocked at egress.
    let g: JSON = .object(["$children": .string("*")])
    #expect(!auth(g, "create_agent", "kid").allowed)  // egress default
    #expect(authIngress(g, "create_agent", "kid").allowed)  // ingress
}

@Test func literalTargetWildcardVerb() {
    let g: JSON = .object(["memory": .string("*")])
    #expect(auth(g, "set", "memory").allowed)
    #expect(auth(g, "read", "memory").allowed)
    #expect(!auth(g, "set", "other").allowed)
}

@Test func perVerbListGrantsExactlyThose() {
    let g: JSON = .object(["memory": .array([.string("read")])])
    #expect(auth(g, "read", "memory").allowed)
    #expect(!auth(g, "set", "memory").allowed)
}

@Test func verbWildcardInsideList() {
    let g: JSON = .object(["memory": .array([.string("*")])])
    #expect(auth(g, "anything", "memory").allowed)
}

@Test func createAgentDeniedUnderDefaultGrant() {
    // create_agent on the parent (not self) is privileged → wildcard doesn't cover it.
    let g = defaultCanCall()
    #expect(!auth(g, "create_agent", "boss").allowed)
}

@Test func forkIntoOwnSubtreeIsNotPrivileged() {
    // create_agent on $self (adding a child to your OWN subtree) is baseline.
    let g: JSON = .object(["$self": .string("*")])
    #expect(auth(g, "create_agent", "worker").allowed)
    #expect(!auth(g, "create_agent", "boss").allowed)
}

@Test func explicitKernelGrantOpensMutation() {
    let g: JSON = .object(["root": .array([.string("create_agent")])])
    #expect(auth(g, "create_agent", "root").allowed)
    #expect(!auth(g, "delete_agent", "root").allowed)
}

// ─── the READ side of the gate: watch + the state stream ────────────

@Test func anUngrantedAgentCannotWatchAnother() async {
    // A watcher receives its target's raw inbound payloads VERBATIM — the model
    // prompt, the document body, whatever was sent. So an ungated `watch` is a
    // read side-channel around `callable_by`: an agent that may not CALL an
    // agent could still read everything sent TO it.
    //
    // Found 2026-08-27 in a production audit: a browser leg granted exactly
    // `assistant:[send]` could mirror ANY agent's inbox in its session and read
    // the packed confidential context out of the model's prompt.
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    let secret = Agent(id: "secret", handlerModule: nil, parentId: nil)
    kernel.register(secret)

    let denial = KernelTaskLocals.$currentSender.withValue(AgentId("nosy")) {
        kernel.watch(src: AgentId("secret"), watcher: AgentId("nosy"))
    }
    #expect(denial != nil, "an ungranted watch was allowed")
    #expect(denial?["reason"].asString == "unauthorized")
    // ...and nothing was registered, so no payload can ever reach it.
    #expect(!secret.watcherIds().contains(AgentId("nosy")))
}

@Test func anOperatorWatchStaysUngated() {
    // `currentSender` nil is the origin root of trust — the bootstrap wiring a
    // renderer, a test driver. Same rule as send.
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    let src = Agent(id: "src", handlerModule: nil, parentId: nil)
    kernel.register(src)
        // The watcher REGISTERS FIRST: `watch` mirrors into an inbox, it does
        // not create one for the caller.
        _ = kernel.ensureInbox(AgentId("cli"))
    #expect(kernel.watch(src: AgentId("src"), watcher: AgentId("cli")) == nil)
    #expect(src.watcherIds().contains(AgentId("cli")))
}

@Test func theStateStreamIsNotCoveredByTheBaseline() {
    // The broadest read the kernel offers. The structural baseline grants
    // `$parent: "*"`, so WITHOUT `state_subscribe` being privileged every direct
    // child of the root could tap the whole stream while holding no grant that
    // names it. A `*` under the root's NAMED id is the operator's own statement
    // and covers it (audit 2026-09-28: whoever writes a `*` owns it).
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    // Two-sided: the root's `callable_by` names `named` too — a structural
    // `$children: "*"` would not carry a privileged verb on either side.
    var rootMeta: OrderedDictionary<String, JSON> = [:]
    rootMeta["callable_by"] = .object(["named": .string("*")])
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: rootMeta)
    kernel.register(root)
    kernel.setRoot(root)
    let leaf = Agent(id: "leaf", handlerModule: nil, parentId: AgentId("kernel_state"))
    kernel.register(leaf)
    let denial = KernelTaskLocals.$currentSender.withValue(AgentId("leaf")) {
        kernel.authorizeStateSubscribe(AgentId("leaf"))
    }
    #expect(denial != nil, "the baseline covered the whole state stream")
    #expect(denial?["reason"].asString == "unauthorized")
    var meta: OrderedDictionary<String, JSON> = [:]
    meta["can_call"] = .object(["kernel_state": .string("*")])
    let named = Agent(
        id: "named", handlerModule: nil, parentId: AgentId("kernel_state"), meta: meta)
    kernel.register(named)
    #expect(
        KernelTaskLocals.$currentSender.withValue(AgentId("named")) {
            kernel.authorizeStateSubscribe(AgentId("named"))
        } == nil, "a named `*` is the operator's own statement")
    // An OPERATOR tap is ungated.
    #expect(kernel.authorizeStateSubscribe(AgentId("cli")) == nil)
}

/// Measured 2026-09-28: `$parent: "*"` let every direct child of the root stop the
/// kernel, kill ANY task and read the whole ledger. A structural token's wildcard
/// covers none of them; a grant that names the verb does.
@Test func theBaselineNeverReachesTheKernelControls() {
    for verb in [
        "kernel_stop", "kernel_start", "shutdown_agents", "ps", "cancel", "ledger",
        "ack_deadletter", "persist_record", "forget_record", "load_tree",
    ] {
        #expect(!auth(defaultCanCall(), verb, "boss").allowed, "\(verb)")
        #expect(auth(.object(["boss": .array([.string(verb)])]), verb, "boss").allowed, "\(verb)")
    }
}

/// Measured 2026-10-03 (audit BV-1): a web_ws/web_rest leg's `$parent:*` let any
/// client it admitted `stop` its web host for every client, or `unmount` a sealed
/// sibling's routes. By verb name, whatever the bundle: a structural token's
/// wildcard covers none of them; a NAMED verb or a NAMED id's `*` does.
@Test func theBaselineNeverReachesAnAgentsLifecycleOrRoutes() {
    for verb in ["stop", "mount", "unmount", "boot"] {
        #expect(privilegedVerbs.contains(verb), "\(verb)")
        #expect(!auth(defaultCanCall(), verb, "boss").allowed, "\(verb)")
        #expect(!auth(defaultCanCall(), verb, "kid").allowed, "\(verb)")
        let named: JSON = .object(["$parent": .array([.string(verb)])])
        #expect(auth(named, verb, "boss").allowed, "\(verb)")
        #expect(auth(.object(["boss": .string("*")]), verb, "boss").allowed, "\(verb)")
        // ingress: a callee's `$children:*` still accepts it
        #expect(authIngress(.object(["$children": .string("*")]), verb, "kid").allowed, "\(verb)")
    }
    #expect(privilegedVerbs.count == 20)
}

/// Audit 2026-10-03 (KV-2): `watch` whose src is the ROOT (`loaderId`) needs a
/// NAMED grant; every other `watch` is an ordinary verb.
@Test func watchingTheRootLosesAStructuralWildcard() {
    // holder "worker", child of the root "boss"
    func onRoot(_ g: JSON, _ other: String) -> Bool {
        authorizeCall(
            grant: g, verb: "watch", otherId: other,
            holderId: "worker", parentId: "boss", childIds: ["kid"], loaderId: "boss"
        ).allowed
    }
    #expect(!onRoot(defaultCanCall(), "boss"))
    #expect(onRoot(.object(["boss": .array([.string("watch")])]), "boss"))
    #expect(onRoot(.object(["boss": .string("*")]), "boss"))
    #expect(onRoot(.object(["$parent": .array([.string("watch")])]), "boss"))
    #expect(onRoot(defaultCanCall(), "kid"))
    #expect(auth(defaultCanCall(), "watch", "boss").allowed)  // "boss" is not the root here
}

/// py `test_watch_gate.py::test_watching_the_root_needs_a_named_grant` — measured
/// before the fix (audit 2026-10-03, KV-2): every direct child of the root watched
/// the root inbox on the baseline `$parent:*` alone and read every kernel verb's
/// payload, other agents' `persist_blob` contents included.
@Test func watchingTheRootNeedsANamedGrant() {
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    var rootMeta: OrderedDictionary<String, JSON> = [:]
    rootMeta["callable_by"] = defaultCallableBy()
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: rootMeta)
    kernel.register(root)
    kernel.setRoot(root)
    func watchAs(_ grant: JSON, _ src: String) -> JSON? {
        var meta: OrderedDictionary<String, JSON> = [:]
        meta["can_call"] = grant
        let nosy = Agent(
            id: "nosy", handlerModule: nil, parentId: AgentId("kernel_state"), meta: meta)
        kernel.register(nosy)
        root.insertChild(nosy)
        _ = kernel.ensureInbox(AgentId("nosy"))
        return KernelTaskLocals.$currentSender.withValue(AgentId("nosy")) {
            kernel.watch(src: AgentId(src), watcher: AgentId("nosy"))
        }
    }
    let denial = watchAs(defaultCanCall(), "kernel_state")
    #expect(denial?["reason"].asString == "unauthorized")
    #expect(denial?["side"].asString == "egress")
    #expect(denial?["error"].asString == "nosy may not call 'watch' on kernel_state")
    #expect(!root.watcherIds().contains(AgentId("nosy")))
    let spelled: JSON = .object([
        "$self": .string("*"), "$parent": .array([.string("*")]), "$children": .string("*"),
    ])
    #expect(watchAs(spelled, "kernel_state") != nil)
    for named: JSON in [
        .object(["$self": .string("*"), "kernel_state": .array([.string("watch")])]),
        .object(["$self": .string("*"), "kernel_state": .string("*")]),
        .object(["$self": .string("*"), "$parent": .array([.string("watch")])]),
    ] {
        #expect(watchAs(named, "kernel_state") == nil, "\(named.serialize())")
    }
    // every other watch is unchanged: the baseline still watches its own child
    var kidMeta: OrderedDictionary<String, JSON> = [:]
    kidMeta["callable_by"] = defaultCallableBy()
    let kid = Agent(id: "nosy_kid", handlerModule: nil, parentId: AgentId("nosy"), meta: kidMeta)
    kernel.register(kid)
    var nosyMeta: OrderedDictionary<String, JSON> = [:]
    nosyMeta["can_call"] = defaultCanCall()
    let nosy = Agent(
        id: "nosy", handlerModule: nil, parentId: AgentId("kernel_state"), meta: nosyMeta)
    nosy.insertChild(kid)
    kernel.register(nosy)
    let ok = KernelTaskLocals.$currentSender.withValue(AgentId("nosy")) {
        kernel.watch(src: AgentId("nosy_kid"), watcher: AgentId("nosy"))
    }
    #expect(ok == nil)
}

/// `*` under a NAMED id covers everything, privileged verbs included.
@Test func aNamedWildcardCoversPrivilegedVerbs() {
    let g: JSON = .object(["root": .string("*")])
    for verb in ["create_agent", "delete_agent", "kernel_stop", "cancel", "allow"] {
        #expect(auth(g, verb, "root").allowed, "\(verb)")
    }
}

/// A foreign caller is named `<guid>|<agent>`, or every agent of that one kernel
/// with `<guid>|*`; the structural tokens never match it.
@Test func aForeignCallerMatchesOnlyByName() {
    #expect(authIngress(.object(["g1|ai": .string("*")]), "read", "g1|ai").allowed)
    #expect(!authIngress(.object(["g1|ai": .string("*")]), "read", "g1|other").allowed)
    #expect(authIngress(.object(["g1|*": .string("*")]), "read", "g1|anyone").allowed)
    #expect(!authIngress(.object(["g1|*": .string("*")]), "read", "g2|anyone").allowed)
    #expect(!authIngress(defaultCallableBy(), "read", "boss|x").allowed)
}
