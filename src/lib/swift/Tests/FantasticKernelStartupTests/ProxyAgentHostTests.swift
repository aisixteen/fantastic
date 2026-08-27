// proxy_agent: a host-implemented agent.
//
// The UniFFI-era compatibility surface (`ProxyAgent` typealias,
// `Kernel.registerProxyAgent`, `_ProxyAgentRegistrable`, the hook pair and the
// `HostAdapter`) is DELETED — it existed only to keep an Apple app's import
// lines unchanged across the XCFramework → native Swift migration. There is one
// registration path now: `registerHost(_:_:)`.
//
// These tests use a UNIQUE agent id each and never call `clearHosts()`. The
// registry is process-global, and a global wipe inside a parallel suite deletes
// hosts belonging to tests that are still running — measured 2026-08-27, that
// was the whole cause of this file's long-standing intermittent failure.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import FantasticProxyAgent
import Foundation
import Testing

@Suite("proxy_agent host")
struct ProxyAgentHostTests {
    final class StubHost: ProxyAgentHost, @unchecked Sendable {
        let calls = OSAllocatedCallCounter()
        func handle(payloadJson: String) -> String {
            calls.bump("handle")
            return #"{"ok":true,"echoed":"\#(payloadJson.count)"}"#
        }
        func onBoot() { calls.bump("onBoot") }
        func onDelete() { calls.bump("onDelete") }
    }

    private func bootProxy(_ id: String) async throws -> Kernel {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("proxy_agent.tools"),
                "id": .string(id),
            ]))])[0].result
        return kernel
    }

    @Test func aRegisteredHostReceivesTheVerb() async throws {
        let id = "ui-receives"
        let kernel = try await bootProxy(id)
        let host = StubHost()
        registerHost(AgentId(id), host)
        defer { unregisterHost(AgentId(id)) }

        let reply = await kernel.send([
            Item(AgentId(id), .object(["type": .string("render"), "x": .integer(1)]))
        ])[0].result
        #expect(reply["ok"].asBool == true, "\(reply)")
        #expect(host.calls.count(for: "handle") == 1)
    }

    @Test func unregisterDropsTheHostAndTheAgentSaysSo() async throws {
        let id = "ui-drops"
        let kernel = try await bootProxy(id)
        registerHost(AgentId(id), StubHost())
        #expect(hostFor(AgentId(id)) != nil)

        unregisterHost(AgentId(id))
        #expect(hostFor(AgentId(id)) == nil)

        // With no host the agent REFUSES rather than silently answering: a proxy
        // whose host has gone is not an agent that returns null.
        let r = await kernel.send([Item(AgentId(id), .object(["type": .string("render")]))])[0].result
        #expect(r["error"].asString != nil, "\(r)")
    }
}

final class OSAllocatedCallCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var counts: [String: Int] = [:]
    func bump(_ key: String) {
        lock.lock(); defer { lock.unlock() }
        counts[key, default: 0] += 1
    }
    func count(for key: String) -> Int {
        lock.lock(); defer { lock.unlock() }
        return counts[key] ?? 0
    }
}
