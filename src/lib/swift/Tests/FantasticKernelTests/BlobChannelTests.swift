// #10 / #37 — the blob channel and the durable ledger.
//
// The owner guard is the point: `agent_id` is the namespace and it arrives in
// the PAYLOAD, so without it any agent granted `persist_blob` could write any
// other agent's sidecar.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

private final class Files: @unchecked Sendable {
    private let lock = NSLock()
    private var f: [String: String] = [:]
    func get(_ p: String) -> String? {
        lock.lock(); defer { lock.unlock() }
        return f[p]
    }
    func put(_ p: String, _ c: String) {
        lock.lock(); defer { lock.unlock() }
        f[p] = c
    }
    func remove(_ p: String) {
        lock.lock(); defer { lock.unlock() }
        f[p] = nil
    }
    var paths: [String] {
        lock.lock(); defer { lock.unlock() }
        return Array(f.keys)
    }
}

/// A mock file_bridge that speaks the STREAM verbs on the BINARY channel — the
/// string `read`/`write` verbs are gone, so the kernel's blob + record I/O rides
/// `write_stream`/`read_stream` with raw bytes.
private struct Store: AgentBundle {
    let name = "file_bridge"
    let files: Files
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let path = payload["path"].asString ?? ""
        switch payload["type"].asString ?? "" {
        case "delete":
            files.remove(path)
            return .object(["deleted": .bool(true)])
        default:
            // Any non-stream verb (e.g. the ledger test's probe) just settles.
            return .object(["ok": .bool(true)])
        }
    }
    func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data) {
        let path = header["path"].asString ?? ""
        switch header["type"].asString ?? "" {
        case "write_stream":
            files.put(path, String(data: blob, encoding: .utf8) ?? "")
            return (
                .object(["written": .bool(true), "size": .integer(Int64(blob.count))]), Data()
            )
        case "read_stream":
            guard let c = files.get(path) else {
                return (.object(["error": .string("not found")]), Data())
            }
            let data = Data(c.utf8)
            return (
                .object([
                    "eof": .bool(true), "size": .integer(Int64(data.count)),
                    "next_offset": .integer(Int64(data.count)),
                ]), data
            )
        default:
            return (.object(["error": .string("unknown verb")]), Data())
        }
    }
}

@Suite("Blob channel + durable ledger")
struct BlobChannelTests {
    private func makeKernel(_ files: Files) async -> Kernel {
        let reg = BundleRegistry()
        reg.register("file_bridge.tools", Store(files: files))
        let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("blobtest-\(UUID().uuidString)")
            .appendingPathComponent(".fantastic")
        // DISK mode: `flushLedger` deliberately no-ops in-memory (the auto-flush
        // law), so the durable half needs a kernel that persists at all.
        let k = Kernel(storage: .disk(storeRoot), bundles: reg)
        let root = Agent(id: "core", handlerModule: nil, parentId: nil, rootPath: storeRoot)
        k.register(root)
        k.setRoot(root)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("store"),
                    "root": .string(storeRoot.path),
                ]))
        ])
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("desk"),
                ]))
        ])
        return k
    }

    @Test func aSidecarRoundTripsAndLandsBesideTheRecord() async {
        let files = Files()
        let k = await makeKernel(files)
        let w = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("persist_blob"), "agent_id": .string("desk"),
                    "name": .string("state.yaml"), "content": .string("k: v\n"),
                ]))
        ])[0].result
        #expect(w["ok"].asBool == true, "\(w)")

        // The CONSUMER never computed this path — the loader did.
        #expect(
            files.paths.contains("agents/desk/state.yaml"),
            "sidecar not where the loader put it: \(files.paths)")

        let r = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("load_blob"), "agent_id": .string("desk"),
                    "name": .string("state.yaml"),
                ]))
        ])[0].result
        #expect(r["content"].asString == "k: v\n")
    }

    @Test func aCancelNeverDestroysAWriteTheKernelAlreadyCommitted() async {
        // MEASURED 2026-08-27. The AI backend commits the user turn, then
        // streams; cancelling mid-stream marked the whole subtree killed, and
        // dispatch is INERT to a killed task — so the persist that had already
        // been decided was swallowed and the chat read back EMPTY. The user's
        // own message disappeared because the reply was cancelled.
        //
        // Bookkeeping is the kernel acting, not the caller: it survives the
        // caller's death. Deterministic here — the flag is set by hand, so this
        // pins the law instead of racing a real stream for it.
        let files = Files()
        let k = await makeKernel(files)
        let killed = KilledFlag()
        killed.mark()
        #expect(killed.isKilled)

        let inert = await KernelTaskLocals.$currentKilled.withValue(killed) {
            await k.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("persist_blob"), "agent_id": .string("desk"),
                        "name": .string("killed.txt"), "content": .string("nope"),
                    ]))
            ])[0].result
        }
        #expect(inert["cancelled"].asBool == true, "a killed subtree must dispatch nothing")
        #expect(!files.paths.contains("agents/desk/killed.txt"))

        // Same killed scope, but entered as BOOKKEEPING: the write lands.
        let saved = await KernelTaskLocals.$currentKilled.withValue(killed) {
            await k.asBookkeeping {
                await k.send([
                    Item(
                        AgentId("kernel"),
                        .object([
                            "type": .string("persist_blob"), "agent_id": .string("desk"),
                            "name": .string("committed.txt"), "content": .string("durable"),
                        ]))
                ])[0].result
            }
        }
        #expect(saved["ok"].asBool == true, "\(saved)")
        #expect(
            files.paths.contains("agents/desk/committed.txt"),
            "a cancel destroyed an already-committed write: \(files.paths)")
    }

    @Test func aNeverWrittenSidecarReadsEmptyNotError() async {
        let files = Files()
        let k = await makeKernel(files)
        let r = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("load_blob"), "agent_id": .string("desk"),
                    "name": .string("nothing.yaml"),
                ]))
        ])[0].result
        // "nothing saved yet" is the normal FIRST state, not a failure.
        #expect(r["content"] == .null, "\(r)")
        #expect(r["error"] == .null, "\(r)")
    }

    @Test func anAgentCannotWriteAnotherAgentsNamespace() async {
        // THE CONFUSED DEPUTY. Measured in python: an agent holding exactly
        // `can_call: {loader:[persist_blob]}` overwrote another agent's
        // state.yaml, and the victim read the forgery back as its own memory.
        let files = Files()
        let k = await makeKernel(files)
        let forged = await KernelTaskLocals.$currentOrigin.withValue(AgentId("attacker")) {
            await k.persistBlob(
                .object([
                    "agent_id": .string("desk"), "name": .string("state.yaml"),
                    "content": .string("owned: true\n"),
                ]))
        }
        #expect(forged["reason"].asString == "unauthorized", "\(forged)")
        #expect(
            !files.paths.contains("agents/desk/state.yaml"),
            "the forged sidecar was written")
    }

    @Test func reservedAndEscapingNamesAreRefused() async {
        let files = Files()
        let k = await makeKernel(files)
        for bad in ["agent.json", "readme.md", "a/b.yaml", ".hidden", "x..y"] {
            let r = await k.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("persist_blob"), "agent_id": .string("desk"),
                        "name": .string(bad), "content": .string("x"),
                    ]))
            ])[0].result
            #expect(r["reason"].asString == "bad_arg", "\(bad) was accepted: \(r)")
        }
    }

    @Test func aCompletedExecutionIsNeverReRunAfterRehydrate() async {
        // C9's full property, through the REAL store: a settled row written out,
        // read back by a fresh ledger, deduping a re-send.
        let files = Files()
        let k = await makeKernel(files)
        _ = await k.send([
            Item(AgentId("desk"), .object(["type": .string("read"), "path": .string("x")]), id: "job-1")
        ])
        await k.flushLedger()

        let doc = files.get(ledgerBlob)
        #expect(doc != nil, "the sidecar never reached the store")
        let parsed = try! JSON.parse(doc!)
        #expect(parsed["v"].asInt == 1, "the sidecar is version-stamped")
        // Keyed by the int id; the caller's name rides the row.
        let named = (parsed["rows"].asObject ?? [:]).values.first { $0["name"].asString == "job-1" }
        #expect(named != nil, "\(parsed.serialize())")

        // Only SETTLED rows cross: every persisted row carries a terminal outcome.
        for (_, row) in parsed["rows"].asObject ?? [:] {
            let st = row["outcome"]["status"].asString ?? ""
            #expect(["completed", "failed", "cancelled", "timed_out"].contains(st))
        }

        // A FRESH ledger rehydrates it DONE.
        let fresh = ExecutionLedger()
        #expect(fresh.rehydrate(parsed) >= 1)
        let jid = fresh.resolveName("job-1") ?? -1
        if case .settled = fresh.open(jid, target: AgentId("desk"), verb: "probe", retries: 0, name: "job-1", origin: nil) {
        } else {
            Issue.record("a rehydrated row must dedup a re-send")
        }
    }

    @Test func aCorruptSidecarStartsEmptyInsteadOfHalfLoading() async {
        // A ledger that silently half-loads would re-arm exactly the side
        // effects it exists to prevent.
        let files = Files()
        let k = await makeKernel(files)
        files.put(ledgerBlob, "{\"v\": 1, \"rows\": {broken")
        #expect(await k.rehydrateLedger() == 0)
        files.put(ledgerBlob, "{\"v\": 99, \"rows\": {}}")
        #expect(await k.rehydrateLedger() == 0)
        // ...and the kernel is still perfectly usable.
        let out = await k.send([Item(AgentId("desk"), .object(["type": .string("read")]))])[0]
        #expect(out.status == .completed, "a bad sidecar bricked the kernel")
    }
}
