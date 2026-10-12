// One precondition, one code (#109).
//
// Five verbs share ONE precondition — the leg has a live transport — and used to
// answer it in three shapes: `list_peers` with `reason:"not_connected"`,
// `forward`/`watch_remote`/`unwatch_remote` with `reason:"not_attached"` and the
// text "no bridge attached", `set_identity`/`watch_directory` with
// `not_attached` and the repair buried in prose. `not_attached` is not in the
// closed vocabulary at all, so a caller branching on `NOT_CONNECTED` handled one
// of them and silently mishandled the rest.

import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

@Suite("bridge not-connected refusals")
struct KernelBridgeRefusalTests {
    private func kernelWithLeg() async -> Kernel {
        let registry = BundleRegistry()
        registry.register("relay_connector.tools", KernelBridgeBundle(family: .relay))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "relay_connector.tools", "id": "leg",
                "transport": "relay",
            ])])[0].result
        return kernel
    }

    @Test(arguments: [
        ["type": "forward", "target": "x", "payload": ["type": "reflect"]] as JSON,
        ["type": "watch_remote", "target": "x"] as JSON,
        ["type": "unwatch_remote", "target": "x"] as JSON,
        ["type": "list_peers"] as JSON,
        ["type": "watch_directory"] as JSON,
        // `unwatch_directory` is NOT here: python answers `{ok:true}` pre-boot
        // (stopping something never started is a no-op), and swift now matches —
        // `RelayTransportTests.directoryRepliesMatchPython` pins that half.
        ["type": "set_identity", "role": "manager"] as JSON,
    ])
    func everyPreBootRefusalSharesOneCode(payload: JSON) async throws {
        let kernel = await kernelWithLeg()
        let r = await kernel.send([Item("leg", payload)])[0].result
        #expect(r["reason"].asString == "not_connected", "\(payload["type"].asString ?? ""): \(r)")
        #expect(r["hint"].asString == "call boot first", "\(r)")
    }
}
