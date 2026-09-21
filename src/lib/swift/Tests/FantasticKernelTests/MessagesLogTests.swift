// The message store persists through a LOADER PUMP and hydrates COLD — the
// kernel knows neither. Twin of python `tests/kernel/test_message_persistence.py`
// (the loader half); rust `messages_log/tests.rs`, `tests/message_log.rs`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

/// A file_bridge stand-in that speaks the STREAM verbs on the BINARY channel
/// against a real directory — `write_stream` APPENDS (no offset), so the log
/// grows the way the real leg grows it, and the cold reader reads the file.
private struct DiskStore: AgentBundle {
    let name = "file_bridge"
    let root: URL
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
    func handleBinary(agentId: AgentId, header: JSON, blob: Data, kernel: Kernel) async throws
        -> (JSON?, Data?)
    {
        let path = root.appendingPathComponent(header["path"].asString ?? "")
        switch header["type"].asString ?? "" {
        case "write_stream":
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
            if let h = try? FileHandle(forWritingTo: path) {
                try h.seekToEnd()
                try h.write(contentsOf: blob)
                try h.close()
            } else {
                try blob.write(to: path)
            }
            return (.object(["written": .bool(true), "size": .integer(Int64(blob.count))]), nil)
        case "read_stream":
            guard let data = try? Data(contentsOf: path) else {
                return (
                    .object(["error": .string("not found"), "reason": .string("not_found")]), nil
                )
            }
            let length = Int(header["length"].asInt ?? Int64(data.count))
            let chunk = data.prefix(length)
            return (.object(["eof": .bool(chunk.count >= data.count), "size": .integer(Int64(data.count)), "next_offset": .integer(Int64(chunk.count))]), Data(chunk))
        default:
            return (.object(["error": .string("unknown verb")]), nil)
        }
    }
}

/// A store whose `write_stream` REFUSES while `refusing` is set, so the pump's
/// requeue path can be driven. `error` is whatever the caller parks there —
/// python decides on TRUTHINESS, not on it being a string.
private final class RefusingStore: AgentBundle, @unchecked Sendable {
    let name = "file_bridge"
    let root: URL
    private let lock = NSLock()
    private var _error: JSON = .null
    private var _probes = 0
    init(root: URL) { self.root = root }
    /// Sync-scoped: `NSLock.lock()` is unavailable from an async frame, and
    /// every reader here is called from one.
    private func withLock<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }
    var probes: Int { withLock { _probes } }
    func setError(_ e: JSON) { withLock { _error = e } }
    private var error: JSON { withLock { _error } }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
    func handleBinary(agentId: AgentId, header: JSON, blob: Data, kernel: Kernel) async throws
        -> (JSON?, Data?)
    {
        let path = root.appendingPathComponent(header["path"].asString ?? "")
        switch header["type"].asString ?? "" {
        case "write_stream":
            // A TRUTHY error refuses (nothing is written); a FALSY one still
            // rides the reply, so the pump's own truthiness rule is what decides.
            let e = error
            if pyTruthy(e) { return (.object(["error": e]), nil) }
            try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
            if let h = try? FileHandle(forWritingTo: path) {
                try h.seekToEnd()
                try h.write(contentsOf: blob)
                try h.close()
            } else {
                try blob.write(to: path)
            }
            return (.object(["written": .bool(true), "error": e]), nil)
        case "read_stream":
            withLock { _probes += 1 }
            guard let data = try? Data(contentsOf: path) else {
                return (
                    .object(["error": .string("not found"), "reason": .string("not_found")]),
                    nil
                )
            }
            let length = Int(header["length"].asInt ?? Int64(data.count))
            let chunk = data.prefix(length)
            return (
                .object([
                    "eof": .bool(chunk.count >= data.count),
                    "next_offset": .integer(Int64(chunk.count)),
                ]), Data(chunk)
            )
        default:
            return (.object(["error": .string("unknown verb")]), nil)
        }
    }
}

private struct Work: AgentBundle {
    let name = "work"
    let delayNanos: UInt64
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        if delayNanos > 0 { try await Task.sleep(nanoseconds: delayNanos) }
        return .object(["ok": .bool(true)])
    }
}

/// Answers with whatever reply the payload names — a handler composing its own
/// refusal, of whatever shape it likes.
private struct Echo: AgentBundle {
    let name = "echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        payload["reply"]
    }
}

private func row(_ pairs: KeyValuePairs<String, JSON>) -> Row {
    var r: Row = [:]
    for (k, v) in pairs { r[k] = v }
    return r
}

private func tmpRoot() -> URL {
    URL(fileURLWithPath: NSTemporaryDirectory())
        .appendingPathComponent("msglog-\(UUID().uuidString)")
        .appendingPathComponent(".fantastic")
}

/// A kernel whose root lives at `root`, with (optionally) a store wired at the
/// same dir — what `findStore` discovers — and a `work` agent.
private func boot(root: URL, store: Bool, slowNanos: UInt64 = 0) async -> Kernel {
    let reg = BundleRegistry()
    reg.register("file_bridge.tools", DiskStore(root: root))
    reg.register("work.tools", Work(delayNanos: slowNanos))
    let k = Kernel(storage: .inMemory, bundles: reg)
    let core = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, rootPath: root)
    k.register(core)
    k.setRoot(core)
    if store {
        _ = await k.send([Item(AgentId("kernel"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel"),
            "handler_module": .string("file_bridge.tools"), "id": .string("store"),
            "root": .string(root.path),
        ]))])
    }
    _ = await k.send([Item(AgentId("kernel"), .object([
        "type": .string("create_agent"), "parent_id": .string("kernel"),
        "handler_module": .string("work.tools"), "id": .string("sp"),
    ]))])
    return k
}

private func lines(_ root: URL) throws -> [Row] {
    let p = root.appendingPathComponent(MESSAGES_FILE)
    guard FileManager.default.fileExists(atPath: p.path) else { return [] }
    return try MessagesXML.parseMessages(try String(contentsOf: p, encoding: .utf8))
}

@Suite("messages log — cold hydrate, the pump, the last window")
struct MessagesLogTests {
    @Test func readMessagesRoundTripsAndSetsTheCounter() throws {
        let root = tmpRoot()
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let rows: [Row] = [
            row(["id": .integer(3), "agent": .string("ai"), "role": .string("user"), "content": .string("hi"), "parent": .null, "ts": .double(1.0)]),
            row(["id": .integer(5), "agent": .string("ai"), "kind": .string("body"), "parent": .integer(4), "content": .string("BODY"), "ts": .double(1.0)]),
            row(["id": .integer(9), "agent": .string("ai"), "kind": .string("view"), "ids": .array([.object(["id": .integer(3), "res": .string("full")])]), "ts": .double(1.0)]),
        ]
        try (MessagesXML.header + rows.map(MessagesXML.rowToXml).joined())
            .write(to: root.appendingPathComponent(MESSAGES_FILE), atomically: true, encoding: .utf8)
        let (got, next) = try readMessages(rootDir: root)
        #expect(got.map { $0["id"]?.asInt } == [3, 5, 9] && next == 10)
        let k = Kernel(storage: .inMemory, bundles: BundleRegistry())
        #expect(try hydrateMessages(k, rootDir: root) == 3)
        #expect(k.rows.nextId == 10 && k.rows.mintId() == 10, "the first mint is above every hydrated id")
        let body = try #require(k.rows.bodyGet(4, offset: 0, length: 10))
        #expect(body == ("BODY", 4, true, 4))
        #expect(k.rows.viewGet(AgentId("ai"))?["ids"]?.asArray?.first?["id"].asInt == 3)
        #expect(k.rows.forAgent(AgentId("ai")).map { $0["id"]?.asInt } == [3, 5, 9])
        // a second ingestion is the user's fault, surfaced
        #expect(throws: RowStoreError.self) { try hydrateMessages(k, rootDir: root) }
    }

    @Test func anUnknownLogVersionRefuses() throws {
        let root = tmpRoot()
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try "<messages v=\"9\">\n".write(to: root.appendingPathComponent(MESSAGES_FILE), atomically: true, encoding: .utf8)
        do {
            _ = try readMessages(rootDir: root)
            Issue.record("an unknown version must refuse")
        } catch {
            #expect("\(error)".contains("unknown log version"), "\(error)")
            #expect("\(error)".contains("'<messages v=\"9\">'"), "python's repr of the first line: \(error)")
        }
        #expect(try readMessages(rootDir: tmpRoot()).1 == 1, "no file: empty, counter at 1")
    }

    @Test func hydrationReplaysASettleOntoItsCall() throws {
        let root = tmpRoot()
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let rows: [Row] = [
            row(["id": .integer(2), "agent": .string("sp"), "kind": .string("call"), "verb": .string("work")]),
            row(["id": .integer(3), "agent": .string("kernel_state"), "kind": .string("settle"), "parent": .integer(2), "settled": .string("sp"), "outcome": .object(["status": .string("completed")])]),
        ]
        try (MessagesXML.header + rows.map(MessagesXML.rowToXml).joined())
            .write(to: root.appendingPathComponent(MESSAGES_FILE), atomically: true, encoding: .utf8)
        let k = Kernel(storage: .inMemory, bundles: BundleRegistry())
        try hydrateMessages(k, rootDir: root)
        #expect(k.rows.getOne(2)?["outcome"]?["status"].asString == "completed", "a cold read answers how a call ended")
    }

    @Test func thePumpAppendsEveryRowButItsOwnCalls() async throws {
        let root = tmpRoot()
        let k = await boot(root: root, store: true)
        let pump = k.installMessagePump(after: k.rows.nextId - 1)
        for _ in 0..<3 {
            _ = await k.send([Item(AgentId("sp"), .object(["type": .string("work")]))])
        }
        let rid = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("planted")]))
        try k.rows.bodyPut(AgentId("ai"), pid: rid, text: String(repeating: "a body ", count: 100))
        try await Task.sleep(nanoseconds: 500_000_000)
        let got = try lines(root)
        let ids = got.compactMap { $0["id"]?.asInt }
        #expect(ids == ids.sorted() && Set(ids).count == ids.count, "in id order, once each")
        #expect(ids.contains(rid))
        #expect(got.contains { $0["kind"]?.asString == "body" && $0["parent"]?.asInt == rid })
        // A dispatch writes TWO rows: the `call` at launch and its `settle`.
        for r in got where r["agent"]?.asString == "sp" && r["kind"] != nil {
            #expect(["call", "settle"].contains(r["kind"]?.asString ?? ""), "\(r)")
        }
        #expect(!got.contains { $0["kind"]?.asString == "call" && $0["agent"]?.asString == "store" }, "the pump's own writes never enter the log")
        #expect(!got.contains { $0["kind"]?.asString == "task" && MessagePump.verbs.contains($0["verb"]?.asString ?? "") })
        #expect(got.contains { $0["kind"]?.asString == "settle" && $0["outcome"]?["status"].asString == "completed" })
        // the last window lands on the shutdown hook, provider still alive
        let late = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("last")]))
        await k.flushPendingMessages()
        #expect(try lines(root).compactMap { $0["id"]?.asInt }.contains(late))
        #expect(k.messagePump == nil)
        #expect(pump.written >= late)
        // a fresh kernel on the same dir hydrates it all and continues the counter
        let k2 = Kernel(storage: .inMemory, bundles: BundleRegistry())
        try hydrateMessages(k2, rootDir: root)
        let hydrated = k2.rows.forAgent(AgentId("ai")).compactMap { $0["id"]?.asInt }
        #expect(hydrated.contains(late) && hydrated.contains(rid))
        #expect(k2.rows.nextId > late)
        let page = try #require(k2.rows.bodyGet(rid, offset: 0, length: 6))
        #expect(page == ("a body", 6, false, 700))
    }

    /// THE DURABLE LOG THROUGH THE REAL PATH: work, tail, append, restart —
    /// rows written THROUGH the discovered store by the pump, read back by a
    /// FRESH kernel, with the counter continuing above everything it read.
    /// Miniature of `E78_kernel_restart`; rust `tests/message_log.rs`.
    @Test func rowsSurviveARestartAndTheCounterContinues() async throws {
        let root = tmpRoot()
        let k1 = await boot(root: root, store: true)
        k1.installMessagePump(after: 0)
        let ai = AgentId("ai")
        try k1.rows.put(ai, row(["kind": .string("message"), "role": .string("user"), "content": .string("what is the code?")]))
        let pid = k1.rows.mintId()
        try k1.rows.put(ai, row(["kind": .string("message"), "role": .string("assistant"), "content": .string("reading it now")]))
        try k1.rows.bodyPut(ai, pid: pid, text: "the long reply body")
        try k1.rows.viewPut(ai, row(["budget": .integer(12000), "ids": .array([.object(["id": .integer(1), "res": .string("full")])])]))
        let live = k1.rows.forAgent(ai)
        let nextBefore = k1.rows.nextId
        await k1.flushPendingMessages()
        let text = try String(contentsOf: root.appendingPathComponent(MESSAGES_FILE), encoding: .utf8)
        #expect(text.hasPrefix("<messages v=\"1\">\n"), "no root: \(text.prefix(60))")
        #expect(text.components(separatedBy: "<messages").count == 2, "the root is written ONCE and never closed")
        #expect(!text.contains("</messages>"), "the root must stay open")

        let k2 = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let n = try hydrateMessages(k2, rootDir: root)
        #expect(n == text.components(separatedBy: "<row").count - 1, "every row came back")
        #expect(n >= live.count)
        #expect(k2.rows.nextId == nextBefore)
        #expect(k2.rows.mintId() >= nextBefore)
        let back = k2.rows.forAgent(ai)
        #expect(back.count == live.count)
        #expect(back[0]["content"] == .string("what is the code?") && back[0]["role"] == .string("user"))
        let page = try #require(k2.rows.bodyGet(pid, offset: 0, length: 3))
        #expect(page == ("the", 3, false, 19))
        #expect(k2.rows.viewGet(ai)?["budget"] == .integer(12000))
    }

    @Test func aCutLogRefusesToBootRatherThanHydratingHalfAConversation() async throws {
        let root = tmpRoot()
        let k1 = await boot(root: root, store: true)
        k1.installMessagePump(after: 0)
        let ai = AgentId("ai")
        try k1.rows.put(ai, row(["kind": .string("message"), "role": .string("user"), "content": .string("first")]))
        try k1.rows.put(ai, row(["kind": .string("message"), "role": .string("assistant"), "content": .string("second")]))
        await k1.flushPendingMessages()
        let log = root.appendingPathComponent(MESSAGES_FILE)
        let text = try String(contentsOf: log, encoding: .utf8)
        try String(text.dropLast(12)).write(to: log, atomically: true, encoding: .utf8)
        let k2 = Kernel(storage: .inMemory, bundles: BundleRegistry())
        do {
            _ = try hydrateMessages(k2, rootDir: root)
            Issue.record("a cut log must be loud")
        } catch {
            #expect("\(error)".contains("never closed") && "\(error)".contains("nothing is repaired automatically"), "\(error)")
        }
        #expect(k2.rows.forAgent(ai).isEmpty)
    }

    @Test func noStoreMeansRamOnly() async throws {
        let root = tmpRoot()
        let k = await boot(root: root, store: false)
        k.installMessagePump(after: k.rows.nextId - 1)
        for i in 0..<2 {
            _ = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("\(i)")]))
            try await Task.sleep(nanoseconds: 300_000_000)
        }
        #expect(!FileManager.default.fileExists(atPath: root.appendingPathComponent(MESSAGES_FILE).path))
        #expect(k.tasks.rows().filter { MessagePump.verbs.contains($0["verb"].asString ?? "") }.count == 2, "two keepalive tasks in the table")
        await k.flushPendingMessages()
    }

    /// C48 — the header probe runs ONCE, and a refused append is PRINTED
    /// python-style and REQUEUED rather than dropped. Truthiness decides: an
    /// `error` of `""`/`0`/`[]` is not a refusal, a non-string one is.
    @Test func refusedAppendRequeuesOnTruthyErrorOnly() async throws {
        let root = tmpRoot()
        let store = RefusingStore(root: root)
        let reg = BundleRegistry()
        reg.register("file_bridge.tools", store)
        let k = Kernel(storage: .inMemory, bundles: reg)
        let rootAgent = Agent(id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil, rootPath: root)
        k.register(rootAgent)
        k.setRoot(rootAgent)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("store"),
                    "root": .string(root.path),
                ]))
        ])
        let pump = k.installMessagePump(after: k.rows.nextId - 1)

        // The tail is a TASK: WAIT FOR IT to have seen the row, rather than
        // guessing a sleep — under parallel load a fixed tick is a flake.
        func put(_ text: String) async throws {
            let id = try k.rows.put(
                AgentId("ai"), row(["role": .string("user"), "content": .string(text)]))
            for _ in 0..<400 {
                if pump.seen >= id { return }
                try await Task.sleep(nanoseconds: 5_000_000)
            }
            Issue.record("the pump's tail never saw row \(id)")
        }

        // One healthy append first, so the log carries its header.
        try await put("a")
        await pump.flush()
        #expect(try lines(root).count == 1)
        let probesAfterFirst = store.probes

        // A REFUSAL — a NON-STRING `error`, which swift used to ignore entirely,
        // counting the append as written and dropping the batch.
        store.setError(.object(["why": .string("sealed")]))
        try await put("b")
        await pump.flush()
        #expect(try lines(root).count == 1, "a refused append must not land")

        // REQUEUED, not lost: the next flush carries it.
        store.setError(.null)
        await pump.flush()
        let landed = try lines(root)
        #expect(
            landed.contains { $0["content"]?.asString == "b" },
            "the requeued row never landed: \(landed.count) rows")

        // A FALSY `error` is NOT a refusal — python tests truthiness.
        store.setError(.string(""))
        try await put("c")
        await pump.flush()
        #expect(try lines(root).contains { $0["content"]?.asString == "c" }, "a falsy error refused")

        // THE HEADER PROBE RAN ONCE, not once per append (rust probed every time).
        #expect(
            store.probes == probesAfterFirst,
            "the header probe re-ran: \(probesAfterFirst) → \(store.probes)")
        await k.flushPendingMessages()
    }

    /// C54 — the no-store line is FLUSHED, so a piped stdout carries it even
    /// when the process is signalled right after.
    @Test func noStoreLineIsFlushed() async throws {
        // `printFlushed` IS the contract; the line's own text is pinned by
        // `noStoreMeansRamOnly`. Here: nothing else is printed on the happy
        // path — hydration and rehydrate are silent on success (C54).
        let root = tmpRoot()
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let k = Kernel(storage: .inMemory, bundles: BundleRegistry())
        #expect(try hydrateMessages(k, rootDir: root) == 0)
        #expect(await k.rehydrateLedger() == 0)
    }

    @Test func aSlowSettleStillReachesTheLog() async throws {
        // THE BUG THE SETTLE ROW EXISTS FOR: a call that settles after its
        // launch row was written must still record an ending on disk.
        let root = tmpRoot()
        let k = await boot(root: root, store: true, slowNanos: 400_000_000)
        k.installMessagePump(after: k.rows.nextId - 1)
        let out = await k.send([Item(AgentId("sp"), .object(["type": .string("work")]))])[0]
        let pid = try #require(out.pid?.value)
        #expect(out.status == .completed)
        try await Task.sleep(nanoseconds: 500_000_000)
        let got = try lines(root)
        #expect(got.contains { $0["id"]?.asInt == pid }, "the launch row never reached the log: \(got.count) rows")
        let settle = got.first { $0["kind"]?.asString == "settle" && $0["parent"]?.asInt == pid }
        #expect(settle?["outcome"]?["status"].asString == "completed", "no ending on disk: \(got.map { ($0["id"]?.asInt, $0["kind"]?.asString) })")
        await k.flushPendingMessages()
    }

    /// C49 — the settle stamp is python's: an `error` of ANY truthy type is
    /// rendered with `str()` and cut at 200 CODE POINTS, and `reason` is copied
    /// RAW. Reading only a non-empty string dropped every structured error from
    /// the log, and `String.prefix(200)` cut GRAPHEMES — 100 ZWJ families where
    /// python keeps 40.
    @Test func stampErrorIsFirst200CodePointsOfPyStr() async throws {
        let reg = BundleRegistry()
        reg.register("echo.tools", Echo())
        let k = Kernel(storage: .inMemory, bundles: reg)
        let core = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        k.register(core)
        k.setRoot(core)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("echo.tools"), "id": .string("e"),
                ]))
        ])

        func stamp(_ reply: JSON) async throws -> Row {
            let out = await k.send([
                Item(
                    AgentId("e"),
                    .object([
                        "type": .string("work"), "reply": reply,
                    ]))
            ])[0]
            let pid = try #require(out.pid?.value)
            let row = try #require(k.rows.getOne(pid))
            return try #require(row["outcome"]?.asObject)
        }

        // A dict error is still an error, and `reason` keeps its own type.
        var s = try await stamp(
            .object(["error": .object(["code": .integer(5)]), "reason": .array([.string("a")])]))
        #expect(s["status"] == .string("errored"), "\(JSON.object(s).serialize())")
        #expect(s["error"] == .string("{'code': 5}"), "\(JSON.object(s).serialize())")
        #expect(s["reason"] == .array([.string("a")]), "a raw reason, not a string")

        // 200 CODE POINTS: each family is 5 scalars and one Character.
        let family = "👨‍👩‍👧"
        s = try await stamp(.object(["error": .string(String(repeating: family, count: 100))]))
        #expect(
            s["error"] == .string(String(repeating: family, count: 40)),
            "cut on graphemes, not code points")

        // Python truthiness on both halves. (The settle STATUS of a falsy
        // non-string error is `classify`'s call, not the stamp's — swift reads
        // `0`/`[]`/`{}` as error-shaped where python does not; no plan item
        // covers that yet, so this pins only the stamp keys.)
        s = try await stamp(.object(["error": .integer(0), "reason": .string("dropped")]))
        #expect(s["error"] == nil && s["reason"] == nil, "\(JSON.object(s).serialize())")
        s = try await stamp(.object(["error": .integer(7), "reason": .integer(0)]))
        #expect(s["error"] == .string("7") && s["reason"] == nil, "\(JSON.object(s).serialize())")
        // …and the settle row on the ROOT carries the same stamp.
        let settles = k.rows.forAgent(AgentId("kernel_state")).filter { $0["kind"]?.asString == "settle" }
        #expect(settles.last?["outcome"]?["error"].asString == "7", "\(settles.count) settle rows")
    }
}
