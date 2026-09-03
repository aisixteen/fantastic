// A bad snapshot is refused BEFORE the live tree is torn down, and a store file
// is read WHOLE.
//
// Measured 2026-09-02 (python-canonical, ported):
//
// - A parent CYCLE resolved on both ends (each `parent_id` named a record in the
//   set) and passed validation, then hydrated nothing — silently dropping every
//   record in the cycle after the old tree was already wiped.
// - More than one root was never checked at all.
// - `loadBlob` and `rehydrateLedger` issued ONE `read_stream` and used the first
//   chunk, so any sidecar past the 64 KiB default came back truncated and a
//   larger ledger always "started empty".

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

private func kernelWithRoot() -> Kernel {
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
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
    let doc = JSON.object(["version": .integer(1), "agents": .array(recs)])
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
