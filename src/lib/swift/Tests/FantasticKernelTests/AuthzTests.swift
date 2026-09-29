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
    // Found 2026-08-27 in a dataroom audit: a browser leg granted exactly
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
