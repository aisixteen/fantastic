// A bridge frame's qualified `caller` (`<guid>|<agent id>`) is checked on
// INGRESS, before the kernel sees it. Every inbound call names its caller, and the
// guid must be the ONE peer the leg is pinned to (`peer_guid`): a peer cannot pose
// as a third kernel's agent (audit 2026-09-28). No pin, no inbound call. Twin of
// python `io_bridge/tests/test_caller_check.py`.

import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

@Suite("bridge caller check")
struct BridgeCallerCheckTests {
    private let bridge = KernelBridgeBundle(family: .ws)

    @Test func aCallerFromThePinnedPeerPasses() {
        #expect(bridge.checkCaller(pin: "peer-g", .string("peer-g|ai")) == nil)
    }

    @Test func aCallerFromAnyOtherKernelIsRefused() {
        let out = bridge.checkCaller(pin: "peer-g", .string("third-g|ai"))
        #expect(out?["reason"].asString == "unauthorized")
        #expect(out?["error"].asString?.contains("pinned peer") == true)
    }

    @Test func noPinServesNoInboundCall() {
        let out = bridge.checkCaller(pin: nil, .string("peer-g|ai"))
        #expect(out?["reason"].asString == "unauthorized")
        #expect(out?["hint"].asString?.contains("peer_guid") == true)
    }

    @Test func aMissingOrMalformedCallerIsRefused() {
        for bad: JSON in [
            .null, .string("ai"), .string("g|"), .string("|ai"), .string("a|b|c"), .integer(42),
        ] {
            #expect(
                bridge.checkCaller(pin: "peer-g", bad)?["reason"].asString == "unauthorized",
                "\(bad.serialize())")
        }
    }

    @Test func thePinIsValidatedAtWrite() {
        #expect(bridge.validateMeta(["peer_guid": "peer-g"], record: [:]) == nil)
        for bad: JSON in [.string(""), .string("a|b"), .integer(7)] {
            #expect(bridge.validateMeta(["peer_guid": bad], record: [:]) != nil, "\(bad.serialize())")
        }
    }
}
