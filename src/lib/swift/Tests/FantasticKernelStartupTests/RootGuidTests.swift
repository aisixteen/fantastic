// Kernel instance `guid` contract (mirrors the py/rust contract):
// the ROOT agent owns a stable 32-hex guid, minted ONCE by the ROOT LOADER'S
// `boot` (python `kernel_state._boot`, C14) — not by the bootstrap — read back
// verbatim on every later boot, and surfaced as a top-level `guid` on root
// reflect. It is minted AFTER the record writer subscribes, so the `updated`
// event it publishes is what carries the new record to a wired store.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("Root guid")
struct RootGuidTests {
    private func isThirtyTwoHex(_ s: String?) -> Bool {
        guard let s = s, s.count == 32 else { return false }
        return s.allSatisfy { $0.isHexDigit && ($0.isNumber || $0.isLowercase) }
    }

    @Test func virginDirMintsPersistsAndReflectsGuid() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let kernel = try await startKernel(workdir: tmp.path)
        // NOTHING MINTED YET — no boot has run (python's own assertion).
        #expect(kernel.root?.metaValue(forKey: "guid") == nil)
        _ = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        let r = await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect")]))])[0].result
        let guid = r["guid"].asString
        #expect(isThirtyTwoHex(guid), "root reflect guid not 32-hex: \(guid ?? "nil")")
        // A second boot is idempotent — the guid is NOT regenerated.
        _ = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        #expect(kernel.root?.metaValue(forKey: "guid")?.asString == guid)

        // Materialize (python's `persist(k)`) and confirm the on-disk root
        // record carries it — with no store wired the writer keeps it in RAM,
        // which is the auto-flush law, not a gap.
        try persist(kernel.root!)
        let recFile = tmp.appendingPathComponent(".fantastic/agent.yaml")
        let parsed = try YamlIO.load(String(decoding: try Data(contentsOf: recFile), as: UTF8.self))
        #expect(parsed["guid"].asString == guid)
    }

    @Test func guidStableAcrossReboot() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let k1 = try await startKernel(workdir: tmp.path)
        _ = await k1.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        let g1 = await k1.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString
        try persist(k1.root!)

        // Second boot of the SAME dir must read the guid back verbatim.
        let k2 = try await startKernel(workdir: tmp.path)
        let g2 = await k2.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString

        #expect(isThirtyTwoHex(g1))
        #expect(g1 == g2, "guid regenerated across reboot: \(g1 ?? "nil") != \(g2 ?? "nil")")
    }

    @Test func existingGuidlessRecordGetsMinted() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        // Pre-seed a root record WITHOUT a guid (as an older impl would write).
        let dot = tmp.appendingPathComponent(".fantastic")
        try FileManager.default.createDirectory(at: dot, withIntermediateDirectories: true)
        let recFile = dot.appendingPathComponent("agent.yaml")
        let legacy: JSON = .object([
            "id": .string("kernel_state"), "handler_module": .string("kernel_state.tools"),
            "parent_id": .null,
        ])
        try legacy.serializePretty(indent: 2).write(to: recFile, atomically: true, encoding: .utf8)

        let kernel = try await startKernel(workdir: tmp.path)
        _ = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        let guid = await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString
        #expect(isThirtyTwoHex(guid))

        // And it lands on disk for the next boot.
        try persist(kernel.root!)
        let reparsed = try YamlIO.load(String(decoding: try Data(contentsOf: recFile), as: UTF8.self))
        #expect(reparsed["guid"].asString == guid)
    }

    @Test func explicitGuidWins() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let dot = tmp.appendingPathComponent(".fantastic")
        try FileManager.default.createDirectory(at: dot, withIntermediateDirectories: true)
        let recFile = dot.appendingPathComponent("agent.yaml")
        let explicit = "deadbeefdeadbeefdeadbeefdeadbeef"
        let rec: JSON = .object([
            "id": .string("kernel_state"), "handler_module": .null, "parent_id": .null,
            "guid": .string(explicit),
        ])
        try rec.serializePretty(indent: 2).write(to: recFile, atomically: true, encoding: .utf8)

        let kernel = try await startKernel(workdir: tmp.path)
        let guid = await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString
        #expect(guid == explicit, "explicit guid was not preserved")
    }

    @Test func inMemoryBootMintsTheGuidToo() async throws {
        // An in-memory kernel mints through the SAME path (the root loader's
        // boot) — just with nothing to persist it to. There is no second,
        // "ephemeral" mint at bootstrap; two mints meant two identities.
        let kernel = try await startKernelInMemory(portHint: 0)
        #expect(kernel.root?.metaValue(forKey: "guid") == nil)
        _ = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        let guid = await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString
        #expect(isThirtyTwoHex(guid), "in-memory root reflect guid not 32-hex: \(guid ?? "nil")")
    }
}

/// Python `_testkit.persist`, for one agent: write its record to its own
/// agent.yaml — the cold materialization a test needs with no store wired.
private func persist(_ agent: Agent) throws {
    try FileManager.default.createDirectory(
        at: agent.rootPath, withIntermediateDirectories: true)
    try Persistence.recordBytes(agent.record().ordered).write(
        to: agent.agentFile, options: .atomic)
}

private func makeTempDir() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
        "fantastic-guid-test-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
