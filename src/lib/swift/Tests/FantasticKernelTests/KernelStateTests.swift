// Save / load snapshot tests.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

@Suite("KernelState save/load")
struct KernelStateTests {
    @Test func snapshotIncludesAllAgents() async {
        let kernel = makeKernel()
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent",
                    "parent_id": "kernel_state",
                    "handler_module": "echo.tools",
                    "id": "alpha",
                ])
        ])[0].result
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent",
                    "parent_id": "kernel_state",
                    "handler_module": "echo.tools",
                    "id": "beta",
                ])
        ])[0].result
        let snap = kernel.snapshotState()
        #expect(snap.version == 1)
        #expect(snap.records.count == 3)  // core + alpha + beta
        // Sorted by id.
        let ids = snap.records.map { $0.id }
        #expect(ids == ids.sorted())
    }

    @Test func saveJSONRoundTrip() async throws {
        let kernel = makeKernel()
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent",
                    "parent_id": "kernel_state",
                    "handler_module": "echo.tools",
                    "id": "x",
                ])
        ])[0].result
        let json = try kernel.saveJSON()
        // Round-trip through a fresh kernel.
        let registry = BundleRegistry()
        registry.register("echo.tools", EchoBundle())
        let kernel2 = Kernel(storage: .inMemory, bundles: registry)
        try kernel2.loadJSON(json)
        let listed = await kernel2.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let ids = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(ids.contains("kernel_state"))
        #expect(ids.contains("x"))
    }

    /// python `Kernel.save()` writes `{version, records}`, and `load` takes
    /// that envelope or a bare record list.
    @Test func theSnapshotIsPythonsShape() throws {
        let kernel = makeKernel()
        let saved = try JSON.parse(kernel.saveJSON())
        #expect(saved.asObject.map { Array($0.keys) } == ["version", "records"], "\(saved)")
        let registry = BundleRegistry()
        registry.register("echo.tools", EchoBundle())
        for doc in [
            #"{"version":1,"records":[{"id":"kernel_state"},{"id":"y","parent_id":"kernel_state","handler_module":"echo.tools"}]}"#,
            #"[{"id":"kernel_state"},{"id":"y","parent_id":"kernel_state","handler_module":"echo.tools"}]"#,
        ] {
            let k2 = Kernel(storage: .inMemory, bundles: registry)
            try k2.loadJSON(doc)
            #expect(k2.agent(AgentId("y")) != nil, "\(doc)")
        }
    }

    @Test func loadRejectsFutureVersion() throws {
        let kernel = makeKernel()
        let badJSON = #"{"version":99,"records":[{"id":"kernel_state"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }

    @Test func loadRejectsDuplicates() throws {
        let kernel = makeKernel()
        let badJSON =
            #"{"version":1,"records":[{"id":"kernel_state"},{"id":"kernel_state","parent_id":"kernel_state"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }

    @Test func loadRejectsDanglingParent() throws {
        let kernel = makeKernel()
        let badJSON =
            #"{"version":1,"records":[{"id":"kernel_state"},{"id":"child","parent_id":"nonexistent"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }

    @Test func loadRejectsNoRoot() throws {
        let kernel = makeKernel()
        let badJSON =
            #"{"version":1,"records":[{"id":"a","parent_id":"b"},{"id":"b","parent_id":"a"}]}"#
        #expect(throws: KernelError.self) {
            try kernel.loadJSON(badJSON)
        }
    }

    /// The record writer's DIRTY SET SURVIVES a flush with no store
    /// wired (python returns before clearing the queues), so everything created
    /// before the provider exists lands the moment one does. And every DRAINED
    /// write seeds that bundle's readme copy-if-missing, not only the create
    /// that first made the record.
    @Test func recordsCreatedBeforeTheStorePersistOnceItIsWired() async throws {
        let files = WriterFiles()
        let reg = BundleRegistry()
        reg.register("file_bridge.tools", WriterStore(files: files))
        reg.register("noop.tools", WriterNoop())
        let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("writer-\(UUID().uuidString)")
            .appendingPathComponent(".fantastic")
        let k = Kernel(storage: .disk(storeRoot), bundles: reg)
        let root = Agent(
            id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil, rootPath: storeRoot)
        k.register(root)
        k.setRoot(root)
        k.installPersistWriter(loader: root.id)

        // BEFORE any store exists.
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("noop.tools"), "id": .string("early"),
                ]))
        ])
        await k.flushRecords()
        #expect(files.paths.isEmpty, "nothing can be written with no provider: \(files.paths)")

        // Wire one. The queued record lands on the next drain.
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("store"),
                    "root": .string(storeRoot.path),
                ]))
        ])
        await k.flushRecords()
        #expect(files.paths.contains("agents/early/agent.yaml"), "\(files.paths)")
        // The record only — no readme copy beside it.
        #expect(!files.paths.contains("agents/early/readme.md"), "\(files.paths)")
    }

    /// Persisted `agent.yaml` bytes are python's `messages_yaml.dump(record)`
    /// — block style, BMP text verbatim (`allow_unicode`), a non-BMP scalar
    /// forces a double-quoted scalar with an uppercase `\UXXXXXXXX` escape, and
    /// the document ends in a newline.
    @Test func testPersistedRecordBytesArePythonYamlDump() async throws {
        let (k, files) = try await makeWriterKernel()
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("noop.tools"), "id": .string("sensor"),
                    "sentence": .string("Sensor readings \u{2014} one integer per file \u{1F600}."),
                ]))
        ])
        await k.flushRecords()
        let bytes = try #require(files.get("agents/sensor/agent.yaml"))
        #expect(
            bytes.contains("sentence: \"Sensor readings \u{2014} one integer per file \\U0001F600.\"\n"),
            "\(bytes)")
        #expect(bytes.hasPrefix("id: sensor\n"), "\(bytes)")
        #expect(bytes.hasSuffix("\n"), "\(bytes)")
    }

    /// `composeEphemeralChild` wires a child exactly like
    /// `create_agent` (registered, findable via `allAgents()`, an `added`
    /// state event fires) but WITHOUT going through the gate/boot machinery
    /// — python's `Cli(kernel, parent=...)` bypasses `create_agent`
    /// entirely because `ephemeral` is a construction-time flag no verb
    /// payload can set.
    @Test func composeEphemeralChildWiresLikeCreateAgentWithoutBooting() async {
        let kernel = makeKernel()
        let root = try! #require(kernel.root)
        let box = EventBox()
        let token = kernel.subscribe { evt, _ in box.append(evt) }
        defer { kernel.unsubscribe(token) }

        let cli = kernel.composeEphemeralChild(
            id: AgentId("cli"), handlerModule: "echo.tools", parent: root,
            meta: ["display_name": .string("cli"), "singleton": .bool(true)])

        #expect(cli.ephemeral)
        #expect(kernel.agent(AgentId("cli")) === cli)
        #expect(kernel.allAgents().contains { $0.id == AgentId("cli") })
        #expect(cli.metaValue(forKey: "singleton") == .bool(true))
        let events = box.all()
        #expect(
            events.contains {
                $0["agent_id"] == .string("cli") && $0["kind"] == .string("added")
            }, "\(events.map { $0.serialize() })")
    }

    /// A record commits through a `<af>.tmp.<8 hex>` write + `rename`,
    /// never a direct `write_stream` onto the live path — so a clean commit
    /// leaves no tmp debris beside the final `agent.yaml`.
    @Test func testRecordCommitsViaTmpAndRename() async throws {
        let (k, files) = try await makeWriterKernel()
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("noop.tools"), "id": .string("late"),
                ]))
        ])
        await k.flushRecords()
        #expect(files.paths.contains("agents/late/agent.yaml"), "\(files.paths)")
        #expect(
            !files.paths.contains { $0.contains(".tmp.") },
            "no tmp debris survives a clean commit: \(files.paths)")
    }

    /// The shared writer-kernel rig `recordsCreatedBeforeTheStorePersistOnceItIsWired`,
    /// `testPersistedRecordBytesArePythonJsonDumps` and
    /// `testRecordCommitsViaTmpAndRename` all boot: a disk-mode kernel with a
    /// fake in-memory `file_bridge.tools` store already wired as `store`.
    private func makeWriterKernel() async throws -> (Kernel, WriterFiles) {
        let files = WriterFiles()
        let reg = BundleRegistry()
        reg.register("file_bridge.tools", WriterStore(files: files))
        reg.register("noop.tools", WriterNoop())
        let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("writer-\(UUID().uuidString)")
            .appendingPathComponent(".fantastic")
        let k = Kernel(storage: .disk(storeRoot), bundles: reg)
        let root = Agent(
            id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil, rootPath: storeRoot)
        k.register(root)
        k.setRoot(root)
        k.installPersistWriter(loader: root.id)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("store"),
                    "root": .string(storeRoot.path),
                ]))
        ])
        await k.flushRecords()
        return (k, files)
    }
}

private final class EventBox: @unchecked Sendable {
    private let lock = NSLock()
    private var events: [JSON] = []
    func append(_ e: JSON) {
        lock.lock()
        events.append(e)
        lock.unlock()
    }
    func all() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }
}

private final class WriterFiles: @unchecked Sendable {
    private let lock = NSLock()
    private var f: [String: String] = [:]
    func put(_ p: String, _ c: String) {
        lock.lock()
        f[p] = c
        lock.unlock()
    }
    func get(_ p: String) -> String? {
        lock.lock()
        defer { lock.unlock() }
        return f[p]
    }
    /// The fake `rename` verb: move a key; the tmp+rename commit protocol
    /// needs this to be REAL, not a no-op `{"ok":true}` — else a persisted
    /// record would only ever exist under its `.tmp.<hex>` name.
    @discardableResult
    func rename(_ from: String, _ to: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let c = f[from] else { return false }
        f[to] = c
        f.removeValue(forKey: from)
        return true
    }
    var paths: [String] {
        lock.lock()
        defer { lock.unlock() }
        return f.keys.sorted()
    }
}

private struct WriterNoop: AgentBundle {
    let name = "noop"
    var readme: String? { "# noop\n" }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? { .null }
}

private struct WriterStore: AgentBundle {
    let name = "file_bridge"
    let files: WriterFiles
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        if payload["type"].asString == "rename" {
            let ok = files.rename(
                payload["old_path"].asString ?? "", payload["new_path"].asString ?? "")
            return .object(["renamed": .bool(ok)])
        }
        return .object(["ok": .bool(true)])
    }
    func handleBinary(agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel) async throws
        -> (JSON?, Blob?)
    {
        // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
        let blob = bodyAt(body, "bytes") ?? Data()
        let path = header["path"].asString ?? ""
        switch header["type"].asString ?? "" {
        case "write_stream":
            // APPEND unless told to truncate — the real provider's contract.
            let incoming = String(data: blob, encoding: .utf8) ?? ""
            let existing = (header["truncate"].asBool == true) ? "" : (files.get(path) ?? "")
            files.put(path, existing + incoming)
            return (.object(["written": .bool(true)]), nil)
        case "read_stream":
            guard let body = files.get(path) else {
                return (
                    .object(["error": .string("not found"), "reason": .string("not_found")]),
                    nil
                )
            }
            return (.object(["eof": .bool(true)]), Blob("bytes", Data(body.utf8)))
        default:
            return (.object(["error": .string("unknown verb")]), nil)
        }
    }
}
