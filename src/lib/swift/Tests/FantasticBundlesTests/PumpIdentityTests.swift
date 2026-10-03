// The PUMP runs both ends AS ITS CALLER — the bridge is no deputy.
//
// Audit AV-7 (2026-10-03): `pump` sent its `read_stream` and `write_stream` from
// inside its own dispatch, so the far ends judged THE BRIDGE, never the caller. A
// client allowed only `pump` on F copied a private G's file into F and read it
// there, and the reverse pump overwrote G. Now both ends run as the pump's
// caller: the caller needs its own grants on the source and on the sink (the
// operator, as ever, is ungated).
//
// Twin of python `file_bridge/tests/test_pump_identity.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private let PRIVATE = Data("PRIVATE-G-DATA".utf8)

private func op(
    _ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil, binary: Blob? = nil
) async -> Outcome {
    let item = Item(AgentId(target), payload, binary: binary)
    guard let sender else { return await k.send([item])[0] }
    return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
        await k.send([item])[0]
    }
}

private func bridge(_ k: Kernel, _ id: String, _ meta: OrderedDictionary<String, JSON>) async {
    var p: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state", "handler_module": "file_bridge.tools",
        "id": .string(id), "root": .string(id), "allow": ["."],
        "ingress_rule": "allow_all_ingress",
    ]
    for (key, v) in meta { p[key] = v }
    let out = await op(k, "kernel_state", .object(p))
    #expect(out.result["id"].asString == id, "\(out.result.serialize())")
}

/// G is private to F; F answers C's `pump` and `read`; C reaches nothing else.
/// Every file lives under a fresh running dir.
private func wired() async -> (Kernel, URL) {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("pump-\(UUID().uuidString.prefix(8))")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let k = makeKernelWithAll(rootPath: dir.appendingPathComponent(".fantastic"))
    await bridge(k, "g", ["callable_by": ["f": ["read_stream", "write_stream"]]])
    await bridge(
        k, "f",
        [
            "can_call": ["g": ["read_stream", "write_stream"]],
            "callable_by": ["c": ["pump", "read", "read_stream", "write_stream"]],
        ])
    k.register(
        Agent(
            id: AgentId("c"), handlerModule: nil, parentId: AgentId("kernel_state"),
            meta: ["can_call": ["f": "*"]]))
    let w = await op(
        k, "g", ["type": "write_stream", "path": "secret.txt", "truncate": true],
        binary: Blob("bytes", PRIVATE))
    #expect(w.result["written"] == .integer(Int64(PRIVATE.count)), "\(w.asJSON.serialize())")
    return (k, dir)
}

private func pump(_ k: Kernel, as sender: String, _ args: OrderedDictionary<String, JSON>) async
    -> JSON
{
    var p: OrderedDictionary<String, JSON> = ["type": "pump"]
    for (key, v) in args { p[key] = v }
    return await op(k, "f", .object(p), as: sender).result
}

@Suite("file_bridge pump — both ends run as its caller")
struct PumpIdentityTests {

    @Test func aPumpCallerCannotReadAnEndItCannotReach() async throws {
        let (k, dir) = await wired()
        defer { try? FileManager.default.removeItem(at: dir) }
        let direct = await op(k, "g", ["type": "read_stream", "path": "secret.txt"], as: "c")
        #expect(direct.result["reason"] == "unauthorized", "\(direct.asJSON.serialize())")
        let out = await pump(
            k, as: "c", ["source": "g", "source_path": "secret.txt", "sink_path": "loot.txt"])
        #expect(out["reason"] == "transport_error", "\(out.serialize())")
        let err = out["error"].asString ?? ""
        #expect(err.hasPrefix("pump: read from 'g' failed: {"), Comment(rawValue: err))
        #expect(err.contains("c may not call 'read_stream' on g"), Comment(rawValue: err))
        #expect(
            !FileManager.default.fileExists(atPath: dir.appendingPathComponent("f/loot.txt").path),
            "nothing was copied")
    }

    @Test func aPumpCallerCannotWriteAnEndItCannotReach() async throws {
        let (k, dir) = await wired()
        defer { try? FileManager.default.removeItem(at: dir) }
        let w = await op(
            k, "f", ["type": "write_stream", "path": "mine.txt", "truncate": true],
            binary: Blob("bytes", Data("OVERWRITTEN".utf8)))
        #expect(w.status == .completed, "\(w.asJSON.serialize())")
        let out = await pump(
            k, as: "c", ["source_path": "mine.txt", "sink": "g", "sink_path": "secret.txt"])
        #expect(out["reason"] == "transport_error", "\(out.serialize())")
        let err = out["error"].asString ?? ""
        #expect(err.hasPrefix("pump: write to 'g' failed: {"), Comment(rawValue: err))
        #expect(try Data(contentsOf: dir.appendingPathComponent("g/secret.txt")) == PRIVATE)
    }

    /// Granted on both ends ITSELF, the caller pumps — the bridge's own reach is
    /// not what lets it.
    @Test func aPumpRunsWithTheCallersOwnGrants() async throws {
        let (k, dir) = await wired()
        defer { try? FileManager.default.removeItem(at: dir) }
        k.update(AgentId("c"), ["can_call": ["f": "*", "g": ["read_stream"]]])
        var cb = k.agent(AgentId("g"))?.metaValue(forKey: "callable_by")?.asObject ?? [:]
        cb["c"] = ["read_stream"]
        k.update(AgentId("g"), ["callable_by": .object(cb)])
        let out = await pump(
            k, as: "c", ["source": "g", "source_path": "secret.txt", "sink_path": "copy.txt"])
        #expect(
            out
                == [
                    "source": "secret.txt", "sink": "copy.txt",
                    "bytes": .integer(Int64(PRIVATE.count)), "chunks": 1,
                ], "\(out.serialize())")
        #expect(try Data(contentsOf: dir.appendingPathComponent("f/copy.txt")) == PRIVATE)
    }
}
