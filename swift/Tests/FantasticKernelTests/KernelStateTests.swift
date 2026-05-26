// Save / load snapshot tests.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

@Suite("KernelState save/load")
struct KernelStateTests {
    @Test func snapshotIncludesAllAgents() async {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "alpha",
            ])
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "beta",
            ])
        let snap = kernel.snapshotState()
        #expect(snap.version == 1)
        #expect(snap.agents.count == 3)  // core + alpha + beta
        // Sorted by id.
        let ids = snap.agents.map { $0.id }
        #expect(ids == ids.sorted())
    }

    @Test func saveJSONRoundTrip() async throws {
        let kernel = makeKernel()
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "echo.tools",
                "id": "x",
            ])
        let json = try kernel.saveJSON()
        // Round-trip through a fresh kernel.
        let registry = BundleRegistry()
        registry.register("echo.tools", EchoBundle())
        let kernel2 = Kernel(storage: .inMemory, bundles: registry)
        try kernel2.loadJSON(json)
        let listed = await kernel2.send("core", ["type": "list_agents"])
        let ids = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(ids.contains("core"))
        #expect(ids.contains("x"))
    }

    @Test func loadRejectsFutureVersion() throws {
        let kernel = makeKernel()
        let badJSON = #"{"version":99,"agents":[{"id":"core"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }

    @Test func loadRejectsDuplicates() throws {
        let kernel = makeKernel()
        let badJSON =
            #"{"version":1,"agents":[{"id":"core"},{"id":"core","parent_id":"core"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }

    @Test func loadRejectsDanglingParent() throws {
        let kernel = makeKernel()
        let badJSON =
            #"{"version":1,"agents":[{"id":"core"},{"id":"child","parent_id":"nonexistent"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }

    @Test func loadRejectsNoRoot() throws {
        let kernel = makeKernel()
        let badJSON =
            #"{"version":1,"agents":[{"id":"a","parent_id":"b"},{"id":"b","parent_id":"a"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }
}
