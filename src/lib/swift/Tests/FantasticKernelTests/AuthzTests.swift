// Pure unit tests for the call-security grant engine — mirror of the python
// `tests/test_grants.py` and rust `authz/tests.rs`. Deny-all is the law; a grant
// on the sender opens it.

import FantasticJSON
import Testing

@testable import FantasticKernel

// sender "worker", parent "boss", one child "kid", loader "root"
private func auth(_ canCall: JSON, _ verb: String, _ target: String) -> Decision {
    authorizeCall(
        canCall: canCall, verb: verb, targetId: target,
        senderId: "worker", parentId: "boss", childIds: ["kid"], loaderId: "root")
}

@Test func absentGrantDeniesEverything() {
    let d = auth(.null, "send", "peer")
    #expect(!d.allowed)
    #expect(!d.reason.isEmpty && !d.hint.isEmpty)
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

@Test func unixDefaultReachesSelfParentAndChildren() {
    let g = defaultCanCall()
    #expect(auth(g, "send", "worker").allowed)  // $self
    #expect(auth(g, "send", "boss").allowed)  // $parent
    #expect(auth(g, "send", "kid").allowed)  // $children
    #expect(!auth(g, "send", "stranger").allowed)
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
