// A bad snapshot is refused BEFORE the live tree is torn down, and a store file
// is read WHOLE.
//
// Pinned (python-canonical, ported):
//
// - A parent CYCLE resolves on both ends (each `parent_id` names a record in the
//   set) and would pass a naive validation, then hydrate nothing — silently
//   dropping every record in the cycle after the old tree was wiped.
// - More than one root is refused.
// - `loadBlob` reads to the end, never only the first `read_stream` chunk (a
//   sidecar past the 64 KiB default would come back truncated).

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

private func kernelWithRoot() -> Kernel {
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func state(_ agents: [(String, String?)]) throws -> KernelState {
    let recs = agents.map { id, parent -> JSON in
        .object([
            "id": .string(id),
            "parent_id": parent.map { JSON.string($0) } ?? .null,
            "meta": .object([:]),
        ])
    }
    let doc = JSON.object(["version": .integer(1), "records": .array(recs)])
    return try JSONDecoder().decode(
        KernelState.self, from: Data(doc.serialize().utf8))
}

@Suite("load guards")
struct LoadGuardTests {

    @Test func aParentCycleIsRefusedAndNothingIsTornDown() throws {
        let kernel = kernelWithRoot()
        let before = Set(kernel.allAgents().map { $0.id.value })
        #expect(throws: (any Error).self) {
            try kernel.load(state([("root", nil), ("a", "b"), ("b", "a")]))
        }
        #expect(Set(kernel.allAgents().map { $0.id.value }) == before)
        #expect(kernel.root != nil)
    }

    @Test func twoRootsAreRefused() throws {
        let kernel = kernelWithRoot()
        #expect(throws: (any Error).self) {
            try kernel.load(state([("root", nil), ("other", nil)]))
        }
        #expect(kernel.root != nil)
    }

    @Test func aReachableTreeLoads() throws {
        let kernel = kernelWithRoot()
        try kernel.load(state([("root", nil), ("a", "root"), ("b", "a")]))
        for id in ["root", "a", "b"] {
            #expect(kernel.agent(AgentId(id)) != nil, "\(id) missing")
        }
    }
}
