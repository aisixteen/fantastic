// AgentRecord Codable tests — most importantly, the `meta` flatten.

import FantasticJSON
import OrderedCollections
import Testing

@testable import FantasticKernel

@Suite("AgentRecord Codable")
struct AgentRecordCodableTests {
    @Test func decodesMinimalRecord() throws {
        let json = #"{"id":"alpha"}"#
        let rec = try decode(json)
        #expect(rec.id == "alpha")
        #expect(rec.handlerModule == nil)
        #expect(rec.parentId == nil)
        #expect(rec.meta.isEmpty)
    }

    @Test func decodesFullRecord() throws {
        let json = #"""
            {"id":"web_1","handler_module":"web.tools","parent_id":"kernel_state","port":31415}
            """#
        let rec = try decode(json)
        #expect(rec.id == "web_1")
        #expect(rec.handlerModule == "web.tools")
        #expect(rec.parentId == "kernel_state")
        #expect(rec.meta["port"]?.asInt == 31415)
    }

    @Test func decodeFlattenedMetaCapturesUnknownKeys() throws {
        // Anything that isn't a kernel-managed field lands in `meta`.
        let json = #"""
            {"id":"x","display_name":"Demo","color":"#ff0099","options":{"k":1}}
            """#
        let rec = try decode(json)
        #expect(rec.meta["display_name"]?.asString == "Demo")
        #expect(rec.meta["color"]?.asString == "#ff0099")
        #expect(rec.meta["options"]?["k"].asInt == 1)
        // Reserved keys NOT in meta.
        #expect(rec.meta["id"] == nil)
    }

    @Test func decodeMissingIdThrows() {
        let json = #"{"handler_module":"x"}"#
        #expect(throws: DecodingError.self) {
            _ = try decode(json)
        }
    }

    @Test func encodeRoundTripPreservesMeta() throws {
        var meta: OrderedDictionary<String, JSON> = [:]
        meta["display_name"] = .string("My Agent")
        meta["port"] = .integer(8080)

        let rec = AgentRecord(
            id: "demo",
            handlerModule: "file_bridge.tools",
            parentId: "kernel_state",
            meta: meta
        )
        let data = try JSONEncoder().encode(rec)
        let decoded = try JSONDecoder().decode(AgentRecord.self, from: data)
        #expect(decoded.id == "demo")
        #expect(decoded.handlerModule == "file_bridge.tools")
        #expect(decoded.parentId == "kernel_state")
        #expect(decoded.meta["display_name"]?.asString == "My Agent")
        #expect(decoded.meta["port"]?.asInt == 8080)
    }

    @Test func encodeDoesNotEmitNilOptionals() throws {
        let rec = AgentRecord(id: "minimal")
        let data = try JSONEncoder().encode(rec)
        let str = String(data: data, encoding: .utf8) ?? ""
        #expect(!str.contains("handler_module"))
        #expect(!str.contains("parent_id"))
    }

    @Test func encodeReservedKeysInMetaAreSkipped() throws {
        // If someone smuggles a reserved key into meta, it must NOT
        // overwrite the top-level field on encode. Defensive — the
        // Rust kernel guards this same way via its persistence layer.
        var meta: OrderedDictionary<String, JSON> = [:]
        meta["id"] = .string("WRONG")
        meta["handler_module"] = .string("WRONG.tools")
        let rec = AgentRecord(
            id: "real_id",
            handlerModule: "real.tools",
            meta: meta
        )
        let data = try JSONEncoder().encode(rec)
        let parsed = try JSON.parse(data)
        #expect(parsed["id"].asString == "real_id")
        #expect(parsed["handler_module"].asString == "real.tools")
    }

    // MARK: - record key order (C15)

    /// Python's `record`: `id`, `handler_module` when set, the meta in ITS
    /// order, and `parent_id` LAST — written last so a `parent_id` that leaked
    /// into meta cannot shadow where the tree says the agent sits.
    @Test func testParentIdEncodesLast() {
        var meta: OrderedDictionary<String, JSON> = [:]
        meta["display_name"] = .string("Kid")
        meta["can_call"] = .object(["$self": .string("*"), "kernel_state": .array([.string("ps")])])
        let rec = AgentRecord(
            id: "kid", handlerModule: "noop.tools", parentId: "kernel_state", meta: meta)
        #expect(
            rec.ordered.keys.elements
                == ["id", "handler_module", "display_name", "can_call", "parent_id"])
        // Nested maps keep their own order too.
        #expect(rec.ordered["can_call"]?.asObject?.keys.elements == ["$self", "kernel_state"])

        // No handler_module ⇒ no key at all (python writes it only when set).
        let bare = AgentRecord(id: "x", parentId: "kernel_state", meta: meta)
        #expect(bare.ordered.keys.elements == ["id", "display_name", "can_call", "parent_id"])

        // THE ROOT HAS NO PARENT, whatever meta says — python pops it.
        var rooty = meta
        rooty["parent_id"] = .string("smuggled")
        let root = AgentRecord(id: "kernel_state", meta: rooty)
        #expect(root.ordered["parent_id"] == nil)
    }

    /// The same order survives the merge-write onto an existing `agent.json`:
    /// python's `existing.update(record)` overlays the record's keys in record
    /// order, and a key the record does NOT carry is left alone rather than
    /// deleted.
    @Test func testPersistedRecordParentIdIsLast() async throws {
        let files = RecordFiles()
        let reg = BundleRegistry()
        reg.register("file_bridge.tools", RecordStore(files: files))
        reg.register("noop.tools", RecordNoop())
        let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("recorder-\(UUID().uuidString)")
            .appendingPathComponent(".fantastic")
        let k = Kernel(storage: .disk(storeRoot), bundles: reg)
        let root = Agent(
            id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil, rootPath: storeRoot)
        k.register(root)
        k.setRoot(root)
        // RECORDS PERSIST THROUGH THE WRITER, not at the verb (C14): the root
        // loader subscribes to the state stream and drains, debounced. Here the
        // drain is driven directly — the test seam python's one-shot mode uses.
        k.installPersistWriter(loader: root.id)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("store"),
                    "root": .string(storeRoot.path),
                ]))
        ])
        // A sidecar field an operator already wrote — it must survive and keep
        // the position it has on disk.
        files.put("agents/kid/agent.json", #"{"note": "keep me", "id": "kid"}"#)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("noop.tools"), "id": .string("kid"),
                    "display_name": .string("Kid"),
                ]))
        ])
        await k.flushRecords()
        let written = try #require(files.get("agents/kid/agent.json"))
        let keys = try JSON.parse(Data(written.utf8)).asObject?.keys.elements
        #expect(keys?.first == "note", "the on-disk key keeps its position: \(written)")
        #expect(keys?.last == "parent_id", "\(written)")
        #expect(keys?.contains("handler_module") == true, "\(written)")
    }

    // MARK: helper

    private func decode(_ string: String) throws -> AgentRecord {
        let data = string.data(using: .utf8)!
        return try JSONDecoder().decode(AgentRecord.self, from: data)
    }
}

import Foundation

/// An in-RAM store for the record merge-write test.
private final class RecordFiles: @unchecked Sendable {
    private let lock = NSLock()
    private var f: [String: String] = [:]
    func get(_ p: String) -> String? {
        lock.lock()
        defer { lock.unlock() }
        return f[p]
    }
    func put(_ p: String, _ c: String) {
        lock.lock()
        defer { lock.unlock() }
        f[p] = c
    }
    /// C43's tmp+rename commit protocol needs this to be REAL, not a no-op
    /// `{"ok":true}` — else a persisted record would only ever exist under
    /// its `.tmp.<hex>` name.
    @discardableResult
    func rename(_ from: String, _ to: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let c = f[from] else { return false }
        f[to] = c
        f.removeValue(forKey: from)
        return true
    }
}

private struct RecordStore: AgentBundle {
    let name = "file_bridge"
    let files: RecordFiles
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        if payload["type"].asString == "rename" {
            let ok = files.rename(
                payload["old_path"].asString ?? "", payload["new_path"].asString ?? "")
            return .object(["renamed": .bool(ok)])
        }
        return .object(["ok": .bool(true)])
    }
    func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data?) {
        let path = header["path"].asString ?? ""
        switch header["type"].asString ?? "" {
        case "write_stream":
            files.put(path, String(data: blob, encoding: .utf8) ?? "")
            return (.object(["written": .bool(true)]), nil)
        case "read_stream":
            guard let c = files.get(path) else {
                return (.object(["error": .string("not found")]), nil)
            }
            let data = Data(c.utf8)
            return (
                .object([
                    "eof": .bool(true), "size": .integer(Int64(data.count)),
                    "next_offset": .integer(Int64(data.count)),
                ]), data
            )
        default:
            return (.object(["error": .string("unknown verb")]), nil)
        }
    }
}

private struct RecordNoop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}
