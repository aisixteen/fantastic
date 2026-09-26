// A bridge frame's qualified `caller` (`<guid>|<agent id>`) is checked on
// INGRESS, before the kernel sees it: malformed or claiming THIS kernel's guid ⇒
// refused. Twin of python `io_bridge/tests/test_caller_check.py`.

import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

@Suite("bridge caller check")
struct BridgeCallerCheckTests {
    private func kernel(_ guid: String) -> Kernel {
        let k = Kernel()
        let root = Agent(
            id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
            meta: ["guid": .string(guid)])
        k.register(root)
        k.setRoot(root)
        return k
    }

    private let bridge = KernelBridgeBundle(family: .ws)

    @Test func aRemoteCallersQualifiedAddressPasses() {
        #expect(bridge.checkCaller(kernel("local-g"), .string("remote-g|ai")) == nil)
    }

    @Test func noCallerIsNotAClaim() {
        #expect(bridge.checkCaller(kernel("local-g"), .null) == nil)
    }

    @Test func aRemoteClaimingThisKernelsGuidIsRefused() {
        let out = bridge.checkCaller(kernel("local-g"), .string("local-g|ai"))
        #expect(out?["reason"].asString == "unauthorized")
        #expect(out?["error"].asString?.contains("THIS kernel's guid") == true)
    }

    @Test func aMalformedCallerIsRefused() {
        for bad: JSON in [.string("ai"), .string("g|"), .string("|ai"), .string("a|b|c"), .integer(42)] {
            #expect(
                bridge.checkCaller(kernel("local-g"), bad)?["reason"].asString == "unauthorized",
                "\(bad.serialize())")
        }
    }
}
