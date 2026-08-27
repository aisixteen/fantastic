// Kernel instance `guid` contract (mirrors the py/rust contract):
// the ROOT agent owns a stable 32-hex guid, minted ONCE at boot,
// persisted to `.fantastic/agent.json`, read back verbatim on every
// later boot, and surfaced as a top-level `guid` on root reflect.

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
        let r = await kernel.send([Item(AgentId("core"), .object(["type": .string("reflect")]))])[0].result
        let guid = r["guid"].asString
        #expect(isThirtyTwoHex(guid), "root reflect guid not 32-hex: \(guid ?? "nil")")

        // Persisted to the root record file as a top-level key, matching reflect.
        let recFile = tmp.appendingPathComponent(".fantastic/agent.json")
        let parsed = try JSON.parse(try Data(contentsOf: recFile))
        #expect(parsed["guid"].asString == guid)
    }

    @Test func guidStableAcrossReboot() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let k1 = try await startKernel(workdir: tmp.path)
        let g1 = await k1.send([Item(AgentId("core"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString

        // Second boot of the SAME dir must read the guid back verbatim.
        let k2 = try await startKernel(workdir: tmp.path)
        let g2 = await k2.send([Item(AgentId("core"), .object(["type": .string("reflect")]))])[0].result["guid"]
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
        let recFile = dot.appendingPathComponent("agent.json")
        let legacy: JSON = .object([
            "id": .string("core"), "handler_module": .null, "parent_id": .null,
        ])
        try legacy.serializePretty(indent: 2).write(to: recFile, atomically: true, encoding: .utf8)

        let kernel = try await startKernel(workdir: tmp.path)
        let guid = await kernel.send([Item(AgentId("core"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString
        #expect(isThirtyTwoHex(guid))

        // And it landed on disk for the next boot.
        let reparsed = try JSON.parse(try Data(contentsOf: recFile))
        #expect(reparsed["guid"].asString == guid)
    }

    @Test func explicitGuidWins() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let dot = tmp.appendingPathComponent(".fantastic")
        try FileManager.default.createDirectory(at: dot, withIntermediateDirectories: true)
        let recFile = dot.appendingPathComponent("agent.json")
        let explicit = "deadbeefdeadbeefdeadbeefdeadbeef"
        let rec: JSON = .object([
            "id": .string("core"), "handler_module": .null, "parent_id": .null,
            "guid": .string(explicit),
        ])
        try rec.serializePretty(indent: 2).write(to: recFile, atomically: true, encoding: .utf8)

        let kernel = try await startKernel(workdir: tmp.path)
        let guid = await kernel.send([Item(AgentId("core"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString
        #expect(guid == explicit, "explicit guid was not preserved")
    }

    @Test func inMemoryBootHasEphemeralGuid() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let guid = await kernel.send([Item(AgentId("core"), .object(["type": .string("reflect")]))])[0].result["guid"]
            .asString
        #expect(isThirtyTwoHex(guid), "in-memory root reflect guid not 32-hex: \(guid ?? "nil")")
    }
}

private func makeTempDir() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
        "fantastic-guid-test-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
