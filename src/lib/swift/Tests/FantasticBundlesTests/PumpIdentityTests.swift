// The PUMP runs both ends AS THE FILE_BRIDGE itself (owner 2026-10-04).
//
// Identity comes only from dispatch: `pump` sends its `read_stream` and
// `write_stream` from inside its own dispatch, so both far ends judge THE BRIDGE —
// its `can_call` must reach them and their `callable_by` must accept it. A
// caller's own grants on the ends play no part: granting `pump` on a bridge lends
// exactly the reach that bridge has.
//
// Twin of python `file_bridge/tests/test_pump_identity.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private let DATA = Data("G-DATA".utf8)

private func op(
    _ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil, binary: Blob? = nil
) async -> Outcome {
    let item = Item(AgentId(target), payload, binary: binary)
    guard let sender else { return await k.send([item])[0] }
    return await KernelTaskLocals.$sender.withValue(AgentId(sender)) {
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

/// G holds a file; C may call F's `pump` and `read`; F reaches G only when
/// `fReachesG` — both halves of the F→G edge. Every file lives under a fresh
/// running dir.
private func wired(fReachesG: Bool) async -> (Kernel, URL) {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("pump-\(UUID().uuidString.prefix(8))")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let k = makeKernelWithAll(rootPath: dir.appendingPathComponent(".fantastic"))
    let edge: JSON = ["read_stream", "write_stream"]
    await bridge(k, "g", ["callable_by": fReachesG ? .object(["f": edge]) : .object([:])])
    await bridge(
        k, "f",
        [
            "can_call": fReachesG ? .object(["g": edge]) : .object([:]),
            "callable_by": ["c": ["pump", "read", "read_stream"]],
        ])
    k.register(
        Agent(
            id: AgentId("c"), handlerModule: nil, parentId: AgentId("kernel_state"),
            meta: ["can_call": ["f": "*"]]))
    let w = await op(
        k, "g", ["type": "write_stream", "path": "data.txt", "truncate": true],
        binary: Blob("bytes", DATA))
    #expect(w.result["written"] == .integer(Int64(DATA.count)), "\(w.asJSON.serialize())")
    return (k, dir)
}

private func pump(_ k: Kernel, as sender: String, _ args: OrderedDictionary<String, JSON>) async
    -> JSON
{
    var p: OrderedDictionary<String, JSON> = ["type": "pump"]
    for (key, v) in args { p[key] = v }
    return await op(k, "f", .object(p), as: sender).result
}

@Suite("file_bridge pump — both ends run as the bridge")
struct PumpIdentityTests {

    @Test func aPumpReadsAndWritesAsTheBridge() async throws {
        let (k, dir) = await wired(fReachesG: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        // C reaches G not at all — the pump's ends are F's sends, not C's
        let direct = await op(k, "g", ["type": "read_stream", "path": "data.txt"], as: "c")
        #expect(direct.result["reason"] == "unauthorized", "\(direct.asJSON.serialize())")
        let out = await pump(
            k, as: "c", ["source": "g", "source_path": "data.txt", "sink_path": "copy.txt"])
        #expect(out["bytes"] == .integer(Int64(DATA.count)), "\(out.serialize())")
        #expect(try Data(contentsOf: dir.appendingPathComponent("f/copy.txt")) == DATA)
    }

    @Test func aPumpCannotReachAnEndTheBridgeCannot() async throws {
        let (k, dir) = await wired(fReachesG: false)
        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await pump(
            k, as: "c", ["source": "g", "source_path": "data.txt", "sink_path": "copy.txt"])
        #expect(out["reason"] == "transport_error", "\(out.serialize())")
        let err = out["error"].asString ?? ""
        #expect(err.hasPrefix("pump: read from 'g' failed: {"), Comment(rawValue: err))
        #expect(err.contains("f may not call 'read_stream' on g"), Comment(rawValue: err))
        #expect(
            !FileManager.default.fileExists(atPath: dir.appendingPathComponent("f/copy.txt").path),
            "nothing was copied")
    }
}
