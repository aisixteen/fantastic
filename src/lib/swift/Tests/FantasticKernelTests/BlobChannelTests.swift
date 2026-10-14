// #10 / #37 — the blob channel and the durable ledger.
//
// The owner guard is the point: `agent_id` is the namespace and it arrives in
// the PAYLOAD, so without it any agent granted `persist_blob` could write any
// other agent's sidecar.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

import FantasticKernelState

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
        agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
    ) async throws -> (JSON?, Blob?) {
        // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
        let blob = bodyAt(body, "bytes") ?? Data()
        let path = header["path"].asString ?? ""
        switch header["type"].asString ?? "" {
        case "write_stream":
            files.put(path, String(data: blob, encoding: .utf8) ?? "")
            return (
                .object(["written": .bool(true), "size": .integer(Int64(blob.count))]), nil
            )
        case "read_stream":
            guard let c = files.get(path) else {
                return (.object(["error": .string("not found")]), nil)
            }
            let data = Data(c.utf8)
            return (
                .object([
                    "eof": .bool(true), "size": .integer(Int64(data.count)),
                    "next_offset": .integer(Int64(data.count)),
                ]), Blob("bytes", data)
            )
        default:
            return (.object(["error": .string("unknown verb")]), nil)
        }
    }
}

@Suite("Blob channel")
struct BlobChannelTests {
    private func makeKernel(_ files: Files) async -> Kernel {
        let reg = BundleRegistry()
        reg.register("file_bridge.tools", Store(files: files))
        // THE BLOB VERBS ARE THE LOADER'S, not the substrate's (C13) — the root
        // answers them through its `kernel_state.tools` bundle.
        reg.register(ROOT_HANDLER_MODULE, KernelStateBundle())
        let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("blobtest-\(UUID().uuidString)")
            .appendingPathComponent(".fantastic")
        // DISK mode: the blob verbs need a kernel that persists at all.
        let k = Kernel(storage: .disk(storeRoot), bundles: reg)
        let root = Agent(
            id: AgentId(ROOT_ID), handlerModule: ROOT_HANDLER_MODULE, parentId: nil,
            rootPath: storeRoot)
        k.register(root)
        k.setRoot(root)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("store"),
                    "root": .string(storeRoot.path),
                ]))
        ])
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("desk"),
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
        // MEASURED 2026-08-27. The AI backend commits the user message, then
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

        // Same killed scope, but through the kernel's OWN persistence function
        // (`persistOwnBlob` — a plain call into the store, not a send): the
        // write lands.
        let saved = await KernelTaskLocals.$currentKilled.withValue(killed) {
            await k.asSelf(AgentId("desk")) {
                await k.persistOwnBlob(name: "committed.txt", content: "durable")
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

    @Test func aGrantedAgentWritesAnyNamespace() async {
        // GRANTS ARE THE ONLY RULE (owner, 2026-10-06): an agent that reaches
        // `persist_blob` names any namespace; the grant is the authority.
        let files = Files()
        let k = await makeKernel(files)
        let forged = await KernelTaskLocals.$origin.withValue(AgentId("attacker")) {
            await k.persistBlob(
                .object([
                    "agent_id": .string("desk"), "name": .string("state.yaml"),
                    "content": .string("owned: true\n"),
                ]))
        }
        #expect(forged["error"] == .null, "\(forged)")
        #expect(files.paths.contains("agents/desk/state.yaml"), "\(files.paths)")
    }

    @Test func reservedAndEscapingNamesAreRefused() async {
        let files = Files()
        let k = await makeKernel(files)
        for bad in ["agent.yaml", "a/b.yaml", ".hidden", "x..y"] {
            let r = await k.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("persist_blob"), "agent_id": .string("desk"),
                        "name": .string(bad), "content": .string("x"),
                    ]))
            ])[0].result
            // PYTHON'S BARE `{error}` — the loader's own reply, not a kernel
            // refusal: no code, no `reason`, no `arg`, and the name rendered
            // with `repr()`.
            #expect(
                r["error"].asString?.hasPrefix("persist_blob: name '\(bad)' ") == true,
                "\(bad) was accepted: \(r)")
            #expect(r["reason"] == .null, "\(r)")
        }
        // The three refusals, verbatim.
        let seg = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("persist_blob"), "agent_id": .string("desk"),
                    "name": .string("a/b.yaml"), "content": .string("x"),
                ]))
        ])[0].result
        #expect(
            seg["error"].asString
                == "persist_blob: name 'a/b.yaml' must be a single filename (no path segments)",
            "\(seg)")
        // A BACKSLASH IS A LEGAL POSIX FILENAME BYTE. Python tests `name !=
        // Path(name).name`, so `a\\b` is ONE segment and passes.
        let backslash = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("persist_blob"), "agent_id": .string("desk"),
                    "name": .string("a\\b.yaml"), "content": .string("x"),
                ]))
        ])[0].result
        #expect(backslash["ok"].asBool == true, "\(backslash)")
    }

    @Test func blobRefusalsArePythonsBareError() async {
        // C13: every one of these is the LOADER's reply — a bare `{error}`,
        // with `reason: unauthorized` on the owner denial alone and no `side`.
        let files = Files()
        let k = await makeKernel(files)
        func call(_ p: OrderedDictionary<String, JSON>) async -> JSON {
            await k.send([Item(AgentId("kernel"), .object(p))])[0].result
        }
        let noId = await call(["type": .string("persist_blob"), "name": .string("s.yaml")])
        #expect(noId == .object(["error": .string("persist_blob: agent_id (str) required")]))
        let noName = await call([
            "type": .string("load_blob"), "agent_id": .string("desk"),
        ])
        #expect(noName == .object(["error": .string("load_blob: name (non-empty str) required")]))
        // CONTENT IS CHECKED AFTER THE NAME AND BEFORE THE STORE.
        let noContent = await call([
            "type": .string("persist_blob"), "agent_id": .string("desk"),
            "name": .string("s.yaml"),
        ])
        #expect(noContent == .object(["error": .string("persist_blob: content (str) required")]))
    }

    /// PERSISTENCE IO IS THE KERNEL'S OWN BOOKKEEPING (owner 2026-10-04): every
    /// store byte (a blob write, a blob read, the record write) is a PLAIN CALL
    /// into the store's handler (`storeCall`) — not a gated send, so it leaves no
    /// call row on the store and no settle row. The `persist_blob` call that
    /// asked for it is the record. Python `Kernel.bookkeeping`.
    @Test func storeWritesAreTheKernelsOwnCalls() async {
        let files = Files()
        let k = await makeKernel(files)
        let w = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("persist_blob"), "agent_id": .string("desk"),
                    "name": .string("rows.yaml"), "content": .string("a: 1\n"),
                ]))
        ])[0].result
        #expect(w["ok"].asBool == true, "\(w)")
        #expect(files.paths.contains("agents/desk/rows.yaml"), "\(files.paths)")
        let r = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("load_blob"), "agent_id": .string("desk"),
                    "name": .string("rows.yaml"),
                ]))
        ])[0].result
        #expect(r["content"] == .string("a: 1\n"), "\(r)")
        // No call row on the store: the kernel called its handler directly.
        let storeRows = k.rows.all().filter { $0["agent"]?.asString == "store" }
        #expect(storeRows.isEmpty, "\(storeRows)")
    }

    /// C79 — the AgentBundle DEFAULT `handleBinary` dispatches the HEADER
    /// through the text `handle`. Python has no separate binary channel at all,
    /// so an octet POST of `{type:"say"}` to a bundle with no chunk verb reaches
    /// its handler; swift answered `no binary edge — verb "say" does not accept
    /// binary frames`, a refusal python never produces.
    @Test func defaultHandleBinaryDispatchesTheHeader() async {
        let files = Files()
        let k = await makeKernel(files)
        // `desk` is a file_bridge-shaped mock; `Store.handle` answers any
        // non-stream verb `{ok:true}` — but `Store` OVERRIDES handleBinary, so
        // the default is exercised through the root's own bundle instead.
        let out = await k.send([
            Item(AgentId("kernel_state"), .object(["type": .string("boot")]), rawReply: true)
        ])[0]
        #expect(out.status != .failed, "\(out.result.serialize())")
        #expect(
            out.result["error"].asString?.contains("no binary edge") != true,
            "the default refused instead of dispatching: \(out.result.serialize())")
        #expect(out.body == nil, "a header dispatch returns no chunk")
    }
}
