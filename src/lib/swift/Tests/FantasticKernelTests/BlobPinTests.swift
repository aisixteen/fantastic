// The blob channel's failure replies, pinned against a SCRIPTED store.
//
// `BlobChannelTests` drives a well-behaved store. The replies a misbehaving or
// refusing store produces — a read that makes no forward progress, a gate denial
// on the read, a file that is not UTF-8 (and WHERE it stops decoding), a refused
// write, no store wired at all — are python `kernel_state._load_blob` /
// `_persist_blob` texts and shapes, and they are what a merge of the two
// read-to-EOF pumps would have to keep.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

import FantasticKernelState

@testable import FantasticKernel

/// A file store whose `agents/desk/blob.txt` reads and writes answer from a
/// script; every other path behaves (the kernel writes its records through it).
private final class ScriptedStore: AgentBundle, @unchecked Sendable {
    let name = "file_bridge"
    private let lock = NSLock()
    private var files: [String: Data] = [:]
    private var reads: [(JSON, Blob?)]
    private let write: JSON?
    private(set) var offsets: [Int64] = []

    init(reads: [(JSON, Blob?)] = [], write: JSON? = nil) {
        self.reads = reads
        self.write = write
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }

    func handleBinary(agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel) async throws
        -> (JSON?, Blob?)
    {
        // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
        let blob = bodyAt(body, "bytes") ?? Data()
        let path = header["path"].asString ?? ""
        let scripted = path == "agents/desk/blob.txt"
        switch header["type"].asString ?? "" {
        case "write_stream":
            if scripted, let write { return (write, nil) }
            lock.withLock { files[path] = blob }
            return (.object(["written": .bool(true), "size": .integer(Int64(blob.count))]), nil)
        case "read_stream":
            if scripted {
                return lock.withLock {
                    offsets.append(header["offset"].asInt ?? -1)
                    return reads.isEmpty
                        ? (.object(["error": .string("script exhausted")]), nil)
                        : reads.removeFirst()
                }
            }
            guard let d = lock.withLock({ files[path] }) else {
                return (.object(["error": .string("not found")]), nil)
            }
            return (
                .object([
                    "eof": .bool(true), "size": .integer(Int64(d.count)),
                    "next_offset": .integer(Int64(d.count)),
                ]), Blob("bytes", d)
            )
        default:
            return (.object(["error": .string("unknown verb")]), nil)
        }
    }
}

/// A disk-mode kernel whose root discovers `store` (or none) as its provider,
/// plus the agent `desk` whose namespace the blobs live in.
private func makeKernel(_ store: ScriptedStore?) async -> Kernel {
    let reg = BundleRegistry()
    if let store { reg.register("file_bridge.tools", store) }
    reg.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
        .appendingPathComponent("blobpin-\(UUID().uuidString)")
        .appendingPathComponent(".fantastic")
    let k = Kernel(storage: .disk(storeRoot), bundles: reg)
    let root = Agent(
        id: AgentId(ROOT_ID), handlerModule: ROOT_HANDLER_MODULE, parentId: nil, rootPath: storeRoot
    )
    k.register(root)
    k.setRoot(root)
    if store != nil {
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."],
                    "id": .string("store"), "root": .string(storeRoot.path),
                ]))
        ])
    }
    _ = await k.send([
        Item(
            AgentId("kernel"),
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel"),
                "id": .string("desk"),
            ]))
    ])
    return k
}

private func load(_ k: Kernel) async -> String {
    await k.send([
        Item(
            AgentId("kernel"),
            .object([
                "type": .string("load_blob"), "agent_id": .string("desk"),
                "name": .string("blob.txt"),
            ]))
    ])[0].result.serialize()
}

private func persist(_ k: Kernel, _ content: String?) async -> String {
    var p: OrderedDictionary<String, JSON> = [
        "type": .string("persist_blob"), "agent_id": .string("desk"), "name": .string("blob.txt"),
    ]
    if let content { p["content"] = .string(content) }
    return await k.send([Item(AgentId("kernel"), .object(p))])[0].result.serialize()
}

private func chunk(eof: Bool, next: Int64?, _ bytes: [UInt8]) -> (JSON, Blob?) {
    var h: OrderedDictionary<String, JSON> = ["eof": .bool(eof)]
    if let next { h["next_offset"] = .integer(next) }
    return (.object(h), Blob("bytes", Data(bytes)))
}

@Suite("Blob channel — failure replies")
struct BlobPinTests {

    @Test func noStoreWiredIsAnErrorNotRam() async {
        let k = await makeKernel(nil)
        // the content check answers first when the store is the only problem
        #expect(await persist(k, nil) == #"{"error":"persist_blob: content (str) required"}"#)
        #expect(
            await persist(k, "x")
                == #"{"error":"persist_blob: no store wired — persistence not available"}"#)
        #expect(
            await load(k) == #"{"error":"load_blob: no store wired — persistence not available"}"#)
    }

    @Test func aReadIsPumpedToEofAcrossChunks() async {
        let store = ScriptedStore(reads: [
            chunk(eof: false, next: 3, Array("abc".utf8)),
            chunk(eof: false, next: 5, Array("de".utf8)),
            chunk(eof: true, next: nil, Array("f".utf8)),
        ])
        let k = await makeKernel(store)
        #expect(await load(k) == #"{"name":"blob.txt","content":"abcdef"}"#)
        #expect(store.offsets == [0, 3, 5])
    }

    @Test func aReadWithNoForwardProgressStallsLoudly() async {
        let store = ScriptedStore(reads: [
            chunk(eof: false, next: 4, Array("abcd".utf8)),
            chunk(eof: false, next: 4, Array("ef".utf8)),
        ])
        let k = await makeKernel(store)
        #expect(
            await load(k)
                == #"{"error":"load_blob: blob.txt stalled at offset 4 — the store returned no "#
                + #"forward progress","name":"blob.txt","content":null}"#)
        let none = ScriptedStore(reads: [chunk(eof: false, next: nil, Array("ab".utf8))])
        #expect(
            await load(await makeKernel(none))
                == #"{"error":"load_blob: blob.txt stalled at offset 0 — the store returned no "#
                + #"forward progress","name":"blob.txt","content":null}"#)
    }

    @Test func aGateDenialOnTheReadIsSurfacedAsUnauthorized() async {
        let denied = ScriptedStore(reads: [
            (
                .object([
                    "error": .string("store: no"), "reason": .string("unauthorized"),
                    "hint": .string("ask"),
                ]), nil
            )
        ])
        #expect(
            await load(await makeKernel(denied))
                == #"{"error":"load_blob: store: no","reason":"unauthorized","hint":"ask"}"#)
        let bare = ScriptedStore(reads: [
            (
                .object([
                    "error": .string("store: no"), "reason": .string("unauthorized"),
                    "hint": .string(""),
                ]), nil
            )
        ])
        #expect(
            await load(await makeKernel(bare))
                == #"{"error":"load_blob: store: no","reason":"unauthorized"}"#)
        // any OTHER refusal before a byte arrived is an absent file: empty, not an error
        let absent = ScriptedStore(reads: [(.object(["error": .string("not found")]), nil)])
        #expect(await load(await makeKernel(absent)) == #"{"name":"blob.txt","content":null}"#)
        // a refusal AFTER bytes arrived returns what was read (python's `if chunks`)
        let partial = ScriptedStore(reads: [
            chunk(eof: false, next: 2, Array("ab".utf8)),
            (.object(["error": .string("gone"), "reason": .string("unauthorized")]), nil),
        ])
        #expect(await load(await makeKernel(partial)) == #"{"name":"blob.txt","content":"ab"}"#)
    }

    /// Strict UTF-8, with python's `UnicodeDecodeError.start` as the offset.
    @Test(
        arguments: [
            ([0x61, 0x62, 0xFF, 0x63], 2),  // an invalid lead byte
            ([0x61, 0xC3], 1),  // a sequence cut at end of file
            ([0xC3, 0xA9, 0xE2, 0x82], 2),  // é, then a cut 3-byte sequence
            ([0xC0, 0xAF], 0),  // an overlong lead
            ([0xED, 0xA0, 0x80], 0),  // a surrogate
            ([0xF4, 0x90, 0x80, 0x80], 0),  // above U+10FFFF
            ([0x61, 0xE2, 0x28, 0xA1], 1),  // a bad continuation byte
        ] as [([UInt8], Int)])
    func aNonUtf8BlobNamesTheFirstBadByte(bytes: [UInt8], at: Int) async {
        let store = ScriptedStore(reads: [chunk(eof: true, next: nil, bytes)])
        #expect(
            await load(await makeKernel(store))
                == #"{"error":"load_blob: blob.txt is not utf-8 at byte \#(at)","reason":"bad_encoding","#
                + #""name":"blob.txt","content":null}"#)
    }

    @Test func aRefusedWriteCarriesTheStoresReasonAndHintOnlyWhenTruthy() async {
        let full = ScriptedStore(
            write: .object([
                "error": .string("disk full"), "reason": .string("transport_error"),
                "hint": .string("free space"),
            ]))
        #expect(
            await persist(await makeKernel(full), "x")
                == #"{"error":"persist_blob: disk full","reason":"transport_error","hint":"free space"}"#
        )
        let bare = ScriptedStore(
            write: .object(["error": .string("nope"), "reason": .string(""), "hint": .null]))
        #expect(await persist(await makeKernel(bare), "x") == #"{"error":"persist_blob: nope"}"#)
        let ok = ScriptedStore(write: .object(["size": .integer(1)]))
        #expect(
            await persist(await makeKernel(ok), "x") == #"{"ok":true,"name":"blob.txt","size":1}"#)
    }
}
