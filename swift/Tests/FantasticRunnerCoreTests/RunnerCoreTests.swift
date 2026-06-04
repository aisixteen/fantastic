// Shared runner-lifecycle dispatch tests — driven through a MOCK
// `RunnerTransport` so the verb skeleton (reflect/boot/shutdown/start/
// stop), the transport-owned extra-verb routing, and the unknown-verb
// error string are all exercised WITHOUT any subprocess or ssh tunnel.
// These cover the dispatch behaviour both real runners used to embed in
// their own `handle`; transport-specific wire tests stay with each
// runner target (and are macOS-gated there).

import FantasticJSON
import FantasticKernel
import Testing

@testable import FantasticRunnerCore

// MARK: - Mock transport

/// Records which verbs were routed to it + returns tagged replies so the
/// dispatch routing is observable. `shutdownAll` flips a flag.
private final class MockTransport: RunnerTransport, @unchecked Sendable {
    private(set) var shutdownCalled = false
    let extraVerb: String?

    init(extraVerb: String? = nil) {
        self.extraVerb = extraVerb
    }

    func reflect() async -> JSON { .object(["called": .string("reflect")]) }
    func start() async -> JSON { .object(["called": .string("start")]) }
    func stop() async -> JSON { .object(["called": .string("stop")]) }
    func shutdownAll() async { shutdownCalled = true }

    func handleVerb(_ verb: String) async -> JSON? {
        guard verb == extraVerb else { return nil }
        return .object(["called": .string(verb)])
    }
}

// MARK: - Tests

@Test func reflectRoutesToTransport() async {
    let t = MockTransport()
    let reply = await RunnerCore.handle(verb: "reflect", transport: t)
    #expect(reply["called"].asString == "reflect")
}

@Test func startRoutesToTransport() async {
    let t = MockTransport()
    let reply = await RunnerCore.handle(verb: "start", transport: t)
    #expect(reply["called"].asString == "start")
}

@Test func stopRoutesToTransport() async {
    let t = MockTransport()
    let reply = await RunnerCore.handle(verb: "stop", transport: t)
    #expect(reply["called"].asString == "stop")
}

@Test func bootIsNoOpOkReply() async {
    let t = MockTransport()
    let reply = await RunnerCore.handle(verb: "boot", transport: t)
    #expect(reply["ok"].asBool == true)
    #expect(t.shutdownCalled == false)
}

@Test func shutdownDrainsThenOk() async {
    let t = MockTransport()
    let reply = await RunnerCore.handle(verb: "shutdown", transport: t)
    #expect(reply["ok"].asBool == true)
    #expect(t.shutdownCalled == true)
}

@Test func extraVerbRoutesToHandleVerb() async {
    // Mirrors local's `list` / ssh's `status`.
    let t = MockTransport(extraVerb: "list")
    let reply = await RunnerCore.handle(verb: "list", transport: t)
    #expect(reply["called"].asString == "list")
}

@Test func unknownVerbErrorIsByteIdentical() async {
    let t = MockTransport()
    let reply = await RunnerCore.handle(verb: "frobnicate", transport: t)
    #expect(reply["error"].asString == "unknown verb frobnicate")
}

@Test func extraVerbThatTransportRejectsFallsThroughToUnknown() async {
    // Transport only handles `status`; `list` must hit the unknown path.
    let t = MockTransport(extraVerb: "status")
    let reply = await RunnerCore.handle(verb: "list", transport: t)
    #expect(reply["error"].asString == "unknown verb list")
}
