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
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
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
    #expect(kernel.watch(src: AgentId("src"), watcher: AgentId("cli")) == nil)
    #expect(src.watcherIds().contains(AgentId("cli")))
}

@Test func theStateStreamIsNotCoveredByAWildcard() {
    // The broadest read the kernel offers. The structural baseline grants
    // `$parent: "*"`, so WITHOUT `state_subscribe` being privileged every direct
    // child of the root could tap the whole stream while holding no grant that
    // names it.
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    // A wildcard on the root — deliberately generous, and still not enough.
    var meta: OrderedDictionary<String, JSON> = [:]
    meta["can_call"] = .object(["core": .string("*")])
    let leaf = Agent(
        id: "leaf", handlerModule: nil, parentId: AgentId("core"), meta: meta)
    kernel.register(leaf)

    let denial = KernelTaskLocals.$currentSender.withValue(AgentId("leaf")) {
        kernel.authorizeStateSubscribe(AgentId("leaf"))
    }
    #expect(denial != nil, "a wildcard covered the whole state stream")
    #expect(denial?["reason"].asString == "unauthorized")
    // An OPERATOR tap is ungated.
    #expect(kernel.authorizeStateSubscribe(AgentId("cli")) == nil)
}
