// Verifies the UniFFI-shape proxy_agent API:
//   - `ProxyAgent` typealias resolves to `ProxyAgentHost`
//   - `Kernel.registerProxyAgent(agentId:host:)` instance method
//   - `Kernel.unregisterProxyAgent(agentId:)` instance method
// These exist to keep the Apple app's import lines unchanged
// across the Rust XCFramework → native Swift migration.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import FantasticProxyAgent
import Foundation
import Testing

@Suite("ProxyAgent UniFFI-shape API")
struct ProxyAgentAPIShimTests {
    final class StubHost: ProxyAgent, @unchecked Sendable {
        // ProxyAgent is now a typealias for ProxyAgentHost.
        let calls = OSAllocatedCallCounter()
        func handle(payloadJson: String) -> String {
            calls.bump("handle")
            return #"{"ok":true,"echoed":"\#(payloadJson.count)"}"#
        }
        func onBoot() { calls.bump("onBoot") }
        func onDelete() { calls.bump("onDelete") }
    }

    @Test func typealiasProxyAgentResolvesToProxyAgentHost() {
        // Compile-time check: assigning a StubHost (which conforms to
        // `ProxyAgent`) into a `ProxyAgentHost` variable works.
        let host: ProxyAgentHost = StubHost()
        _ = host
    }

    @Test func kernelRegisterProxyAgentMethodForwards() async throws {
        clearHosts()
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("proxy_agent.tools"),
                "id": .string("ui"),
            ]))
        let host = StubHost()
        // UniFFI-shape method call:
        kernel.registerProxyAgent(agentId: "ui", host: host)

        // Verify dispatch goes through.
        let reply = await kernel.send(
            AgentId("ui"), .object(["type": .string("render"), "x": .integer(1)]))
        #expect(reply["ok"].asBool == true)
        #expect(host.calls.count(for: "handle") == 1)
    }

    @Test func kernelUnregisterProxyAgentMethodReturnsHadHost() async throws {
        clearHosts()
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("proxy_agent.tools"),
                "id": .string("ui"),
            ]))
        let host = StubHost()
        kernel.registerProxyAgent(agentId: "ui", host: host)
        #expect(kernel.unregisterProxyAgent(agentId: "ui") == true)
        // Second call returns false (nothing to drop).
        #expect(kernel.unregisterProxyAgent(agentId: "ui") == false)
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
