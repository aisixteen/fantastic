// THE DURABLE MESSAGE LOG through the REAL daemon path: a disk-backed kernel
// (`startKernel`), the real `file_bridge` store discovered at `.fantastic`,
// the pump, a shutdown flush, and a FRESH kernel on the same workdir reading
// it all back with the counter continuing. Miniature of `E78_kernel_restart`.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("message log — restart through the real store")
struct MessageLogRestartTests {
    private func workdir() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("msgrestart-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    /// THE ANTI-SPIN PROPERTY, re-keyed to the tape. The pump appending a row
    /// through the store is itself a call; persisting THAT would mint another,
    /// forever (19 flushes in 3 idle seconds, measured live before the skip
    /// rule). With the pump running and NOTHING happening, the log stops
    /// growing. Python `test_an_idle_kernel_with_a_store_writes_nothing`.
    @Test func anIdleKernelWithAStoreWritesNothing() async throws {
        let wd = try workdir()
        let k = try await startKernel(workdir: wd.path, portHint: 0)
        let made = await k.send([Item(AgentId("kernel_state"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "handler_module": .string("file_bridge.tools"), "id": .string("store"),
            "root": .string(wd.appendingPathComponent(".fantastic").path),
            "ingress_rule": .string("allow_all_ingress"),
        ]))])[0].result
        #expect(made["error"].isNull, "\(made.serialize())")
        k.installMessagePump(after: k.rows.nextId - 1)
        let log = wd.appendingPathComponent(".fantastic/messages.xml")
        func size() -> Int {
            (try? FileManager.default.attributesOfItem(atPath: log.path)[.size] as? Int)
                .flatMap { $0 } ?? 0
        }
        for _ in 0..<3 {
            _ = await k.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])
        }
        try? await Task.sleep(nanoseconds: 600_000_000)
        let base = size()
        // NOTHING BUT PERSISTENCE TRAFFIC IS LEFT.
        try? await Task.sleep(nanoseconds: 1_200_000_000)
        #expect(size() == base, "persistence traffic logged itself: \(base) → \(size())")
        // …and `ledger.json` is never written again, by anyone.
        #expect(
            !FileManager.default.fileExists(
                atPath: wd.appendingPathComponent(".fantastic/ledger.json").path))
        await k.flushPendingMessages()
        _ = await k.send([Item(AgentId("web"), .object(["type": .string("stop")]))])
    }

    @Test func rowsSurviveARestartThroughTheDiscoveredStore() async throws {
        let wd = try workdir()
        let k1 = try await startKernel(workdir: wd.path, portHint: 0)
        try hydrateMessages(k1, rootDir: wd.appendingPathComponent(".fantastic"))
        // THE STORE: a file_bridge rooted at `.fantastic` — what the loader
        // discovers as its provider (E78 wires exactly this).
        let made = await k1.send([Item(AgentId("kernel_state"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "handler_module": .string("file_bridge.tools"), "id": .string("store"),
            "root": .string(wd.appendingPathComponent(".fantastic").path),
            // SEALED BY DEFAULT: an io leg with no rule denies every inbound
            // call, the loader's own included — E78 opens it exactly so.
            "ingress_rule": .string("allow_all_ingress"),
        ]))])[0].result
        #expect(made["error"].isNull, "\(made.serialize())")
        #expect(k1.findStore()?.value == "store")
        k1.installMessagePump(after: k1.rows.nextId - 1)
        let ai = AgentId("ai")
        let a = try k1.rows.put(ai, ["role": .string("user"), "content": .string("what is the code?")])
        _ = await k1.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])
        let b = try k1.rows.put(ai, ["role": .string("assistant"), "content": .string("reading it now"), "parent": .integer(a)])
        let nextBefore = k1.rows.nextId
        await k1.flushPendingMessages()
        let log = wd.appendingPathComponent(".fantastic/messages.xml")
        let text = try String(contentsOf: log, encoding: .utf8)
        #expect(text.hasPrefix(MessagesXML.header), "\(text.prefix(80))")
        #expect(!text.contains("agent=\"store\""), "the pump's own store traffic never enters the log")
        #expect(text.contains("kind=\"call\" verb=\"ps\""), "the ps call row landed: \(text)")
        _ = await k1.send([Item(AgentId("web"), .object(["type": .string("stop")]))])

        let k2 = try await startKernel(workdir: wd.path, portHint: 0)
        let n = try hydrateMessages(k2, rootDir: wd.appendingPathComponent(".fantastic"))
        #expect(n >= 2)
        #expect(k2.rows.nextId == nextBefore, "the counter continues above every hydrated id")
        let back = k2.rows.forAgent(ai)
        #expect(back.map { $0["id"]?.asInt } == [a, b])
        #expect(back[1]["parent"]?.asInt == a)
        // The hydrated `rows_read` answers the same rows over the verb.
        let r = await k2.send([Item(AgentId("kernel"), .object(["type": .string("rows_read"), "agent": .string("ai")]))])[0].result
        #expect((r["rows"].asArray ?? []).compactMap { $0["id"].asInt } == [a, b], "\(r.serialize())")
        _ = await k2.send([Item(AgentId("web"), .object(["type": .string("stop")]))])
    }

    // ── ONE TAPE: the ledger IS the rows (2026-09-24) ──────────────────────

    /// A root with the real store wired and the pump running — the tape is the
    /// only durable record now, so a restart test has to actually write it.
    private func bootWithTape(_ wd: URL) async throws -> Kernel {
        let k = try await startKernel(workdir: wd.path, portHint: 0)
        try hydrateMessages(k, rootDir: wd.appendingPathComponent(".fantastic"))
        let made = await k.send([Item(AgentId("kernel_state"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "handler_module": .string("file_bridge.tools"), "id": .string("store"),
            "root": .string(wd.appendingPathComponent(".fantastic").path),
            "ingress_rule": .string("allow_all_ingress"),
        ]))])[0].result
        #expect(made["error"].isNull, "\(made.serialize())")
        // THE REAL BOOT: the root loader rebuilds `executions` from the tape,
        // subscribes the pump and starts the record writer — the same order a
        // daemon takes.
        _ = await k.send([Item(AgentId("kernel_state"), .object(["type": .string("boot")]))])
        return k
    }

    /// Land everything, drop the process, come back up off `messages.xml` —
    /// including the ledger rebuild the root's boot performs.
    private func reboot(_ k1: Kernel, _ wd: URL) async throws -> Kernel {
        await k1.flushRecords()
        await k1.flushPendingMessages()
        _ = await k1.send([Item(AgentId("web"), .object(["type": .string("stop")]))])
        let k2 = try await startKernel(workdir: wd.path, portHint: 0)
        try hydrateMessages(k2, rootDir: wd.appendingPathComponent(".fantastic"))
        _ = await k2.send([Item(AgentId("kernel_state"), .object(["type": .string("boot")]))])
        return k2
    }

    /// ONE TAPE. `messages.xml` holds the call WITH its dispatch payload and the
    /// settle WITH the whole result body — no second file, nothing RAM-only —
    /// and the `ledger` verb answers the FULL outcome after a restart.
    @Test func theTapeCarriesThePayloadAndTheFullOutcome() async throws {
        let wd = try workdir()
        let k1 = try await bootWithTape(wd)
        let out = await k1.send([
            Item(AgentId("kernel"), .object(["type": .string("ps")]), id: "tape-1")
        ])[0]
        #expect(out.status == .completed)
        let k2 = try await reboot(k1, wd)

        let rows = try readMessages(rootDir: wd.appendingPathComponent(".fantastic")).0
        let call = try #require(rows.first { $0["name"]?.asString == "tape-1" })
        #expect(call["payload"]?["type"] == .string("ps"), "\(call)")
        #expect(call["named"] == .bool(true) && call["retries"] == .integer(0))
        #expect(call["attempts"] == .integer(1))
        #expect(
            Array(call.keys).filter {
                ["id", "kind", "verb", "parent", "origin", "payload", "named", "retries",
                 "attempts", "name", "agent", "ts", "outcome"].contains($0)
            }.prefix(3) == ["id", "kind", "verb"], "\(Array(call.keys))")
        let settle = try #require(
            rows.first { $0["kind"]?.asString == "settle" && $0["parent"] == call["id"] })
        #expect(settle["outcome"]?["status"] == .string("completed"), "\(settle)")
        #expect(settle["outcome"]?["result"]["tasks"] != nil, "the WHOLE body: \(settle)")
        #expect(settle["attempts"] == .integer(1))

        // THE `ledger` VERB KEEPS ITS SURFACE, reading the rebuilt executions.
        let led = await k2.send([
            Item(AgentId("kernel"), .object(["type": .string("ledger")]))
        ])[0].result
        let row = try #require(
            (led["executions"].asArray ?? []).first { $0["name"].asString == "tape-1" })
        #expect(row["status"].asString == "completed", "\(row.serialize())")
        #expect(row["outcome"]["result"]["tasks"] != nil, "the outcome body survived: \(row.serialize())")
        _ = await k2.send([Item(AgentId("web"), .object(["type": .string("stop")]))])
    }

    /// C9's full property across a REBOOT, out of one file: a NAMED execution
    /// dedups after a restart — the recorded outcome comes back and the handler
    /// never runs again.
    @Test func aNamedExecutionDedupsAfterARestart() async throws {
        let wd = try workdir()
        let k1 = try await bootWithTape(wd)
        let first = await k1.send([
            Item(AgentId("kernel"), .object(["type": .string("ps")]), id: "dedup-1")
        ])[0]
        #expect(first.status == .completed)
        let firstPid = first.pid?.value
        let k2 = try await reboot(k1, wd)

        let again = await k2.send([
            Item(AgentId("kernel"), .object(["type": .string("ps")]), id: "dedup-1")
        ])[0]
        #expect(again.status == .completed)
        #expect(again.attempts == 1)
        #expect(again.pid?.value == firstPid, "a re-send returned a NEW execution")
        // In flight at the crash ⇒ nothing rebuilt: an unknown name is fresh.
        #expect(k2.ledger.resolveName("never-ran") == nil)
        _ = await k2.send([Item(AgentId("web"), .object(["type": .string("stop")]))])
    }

    /// The ONE reduction: a `bytes` value in a payload or a result is written as
    /// its LENGTH. Base64 is banned; text and JSON stay verbatim.
    @Test func rawBytesOnTheTapeAreALengthNotBase64() async throws {
        let wd = try workdir()
        let k1 = try await bootWithTape(wd)
        // A SECOND file_bridge, NOT the discovered store — the pump keeps the
        // store's own operator-context calls out of the log, and this row has to
        // be on the tape to be read back.
        _ = await k1.send([Item(AgentId("kernel_state"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "handler_module": .string("file_bridge.tools"), "id": .string("blobs"),
            "root": .string(wd.appendingPathComponent("blobs").path),
            "ingress_rule": .string("allow_all_ingress"),
        ]))])
        let out = await k1.send([
            Item(
                AgentId("blobs"),
                .object(["type": .string("write_stream"), "path": .string("probe.bin")]),
                id: "bin-1", binary: Data([0, 1, 2, 3, 4]))
        ])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        _ = try await reboot(k1, wd)

        let rows = try readMessages(rootDir: wd.appendingPathComponent(".fantastic")).0
        let call = try #require(rows.first { $0["name"]?.asString == "bin-1" })
        #expect(call["payload"]?["bytes"] == .object(["len": .integer(5)]), "\(call)")
        // EVERYTHING ELSE VERBATIM.
        #expect(call["payload"]?["path"] == .string("probe.bin"))
    }

    // ── the two shutdown-loss repros (2026-09-24) ──────────────────────────

    /// A shutdown that lands ON the debounce boundary used to CANCEL the flush
    /// loop mid-append and drop the batch with it. The flush now holds a lock
    /// the catch-up waits on, and a cancelled flush re-queues its rows.
    @Test(arguments: 0..<20)
    func aRowPutJustBeforeShutdownAlwaysLands(run: Int) async throws {
        let wd = try workdir()
        let k = try await bootWithTape(wd)
        let rid = try k.rows.put(
            AgentId("ai"), ["role": .string("user"), "content": .string("row \(run)")])
        // SWEEP the debounce boundary: the loss window is the handful of ms the
        // flush spends inside the store's `write_stream`.
        try await Task.sleep(nanoseconds: 150_000_000 + UInt64(run) * 500_000)
        await k.shutdown()
        let rows = try readMessages(rootDir: wd.appendingPathComponent(".fantastic")).0
        #expect(
            rows.contains { $0["id"]?.asInt == rid },
            "run \(run): the last row before shutdown never reached the log")
    }

    /// The record writer's final flush used to run mid-teardown-walk, with the
    /// store already gone. It runs in `flushPending` now — before the walk,
    /// while the provider is alive — so a create+update immediately followed by
    /// a shutdown is on disk.
    @Test func aRecordUpdatedJustBeforeShutdownAlwaysLands() async throws {
        let wd = try workdir()
        let k = try await bootWithTape(wd)
        _ = await k.send([Item(AgentId("kernel_state"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "handler_module": .string("file_bridge.tools"), "id": .string("sp_late"),
            "root": .string(wd.appendingPathComponent("late").path),
            "ingress_rule": .string("allow_all_ingress"),
        ]))])
        _ = await k.send([Item(AgentId("kernel_state"), .object([
            "type": .string("update_agent"), "id": .string("sp_late"),
            "grants": .object(["can_call": .array([.string("store")])]),
        ]))])
        await k.shutdown()
        let found =
            (FileManager.default.enumerator(at: wd, includingPropertiesForKeys: nil)?
            .compactMap { $0 as? URL }
            .filter { $0.lastPathComponent == "agent.json" && $0.path.contains("sp_late") }) ?? []
        let f = try #require(found.first, "the record never reached disk")
        let doc = try JSON.parse(try Data(contentsOf: f))
        #expect(doc["grants"]["can_call"] == .array([.string("store")]), "\(doc.serialize())")
    }
}
