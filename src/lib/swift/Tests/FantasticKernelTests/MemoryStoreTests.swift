// The kernel keeps NO conversation and NO durable tape. Its execution rows
// (`call`, `settle`, `task`, `ack`) live in RAM until the in-kernel FLUSHER writes
// them to the root's MEMORY STORE (`memory_store_id`) and drops them; `ledger`,
// dead letters, `ack_deadletter` and named-call dedup read the store back.
// `kernel_start` refuses to run without a store. Also here: the tail, the drain
// and `shutdown_agents`, the id counter's block reservation, and the graceful
// stop landing its last rows.
//
// Twin of python `tests/kernel/test_memory_store.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel
@testable import FantasticKernelState
@testable import FantasticRamStore

/// A probe bundle: advertises `shutdown`, can hold, fail, and count.
private final class MemProbe: AgentBundle, @unchecked Sendable {
    let name = "mem_probe"
    var verbs: [String] { ["reflect", "shutdown", "slow", "work"] }
    private let lock = NSLock()
    private var _shut: [String] = []
    private var _counts: [String: Int] = [:]
    private var _running: Set<String> = []

    var shut: [String] { lock.withLock { _shut } }
    func count(_ id: String) -> Int { lock.withLock { _counts[id] ?? 0 } }
    func running(_ id: String) -> Bool { lock.withLock { _running.contains(id) } }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let id = agentId.value
        switch payload["type"].asString ?? "" {
        case "boot": return nil
        case "reflect":
            return .object([
                "id": .string(id), "sentence": .string("probe"),
                "verbs": .object(["shutdown": .string("wraps up")]),
            ])
        case "shutdown":
            lock.withLock { _shut.append(id) }
            // the shutdown dispatch may still use its tools: a nested send passes
            // the drain
            let r = await kernel.send([Item("kernel", .object(["type": .string("ps")]))])[0]
            return .object(["wrapped": .bool(true), "tools_ok": .bool(r.status == .completed)])
        case "slow":
            lock.withLock { _ = _running.insert(id) }
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["finished": .bool(true)])
        case "work":
            return .object(["ok": .bool(true)])
        case "count":
            let n: Int = lock.withLock {
                _counts[id, default: 0] += 1
                return _counts[id]!
            }
            return .object(["n": .integer(Int64(n))])
        case "fail":
            throw PyError("RuntimeError", "probe failed on purpose")
        default:
            return .object([
                "error": .string("unknown \(pyRepr(payload["type"]))"),
                "reason": .string("unhandled"),
            ])
        }
    }
}

private func makeKernel(_ probe: MemProbe) -> Kernel {
    let r = BundleRegistry()
    r.register("kernel_state.tools", KernelStateBundle())
    r.register("ram_store.tools", RamStoreBundle())
    r.register("mem_probe.tools", probe)
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: "kernel_state.tools", parentId: nil)
    k.register(root)
    k.setRoot(root)
    return k
}

@discardableResult
private func call(
    _ k: Kernel, _ target: String, _ payload: JSON, id: String? = nil, as sender: String? = nil
) async -> Outcome {
    let item = Item(AgentId(target), payload, id: id)
    if let sender {
        return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
            await k.send([item])[0]
        }
    }
    return await k.send([item])[0]
}

private func verb(_ t: String, _ extra: OrderedDictionary<String, JSON> = [:]) -> JSON {
    var o: OrderedDictionary<String, JSON> = ["type": .string(t)]
    for (k, v) in extra { o[k] = v }
    return .object(o)
}

private func mk(_ k: Kernel, _ id: String = "probe", _ meta: OrderedDictionary<String, JSON> = [:])
    async
{
    var p: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"), "parent_id": .string("kernel_state"),
        "id": .string(id), "handler_module": .string("mem_probe.tools"),
        "callable_by": .object([
            "$parent": .string("*"), "rest": .string("*"), "caller": .string("*"),
        ]),
    ]
    for (kk, v) in meta { p[kk] = v }
    let r = await call(k, "kernel_state", .object(p)).result
    #expect(r["id"].asString == id, "\(r.serialize())")
}

/// A ram_store `memory` under the root, named by the root; the kernel started.
private func withStore(_ k: Kernel, operator op: Bool = true, start: Bool = true) async {
    let r = await call(
        k, "kernel_state",
        .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "id": .string("memory"), "handler_module": .string("ram_store.tools"),
            "operator": .bool(op), "admins": .array([.string("kernel")]),
        ])
    ).result
    #expect(r["id"].asString == "memory", "\(r.serialize())")
    k.update(AgentId("kernel_state"), ["memory_store_id": .string("memory")])
    if start {
        let out = await k.start(bootAll: false)
        #expect(out["started"].asBool == true, "\(out.serialize())")
    }
}

/// The kernel's own rows in the store (`<guid>|kernel`).
private func kernelRows(_ k: Kernel) async -> [JSON] {
    let tables = await call(k, "memory", verb("dump")).result["tables"]
    let guid = k.root?.metaValue(forKey: "guid")?.asString ?? ""
    return (tables["rows"].asArray ?? []).filter { $0["owner"].asString == "\(guid)|kernel" }
        .map { $0["row"] }
}

private func waitUntil(_ what: String, _ cond: () -> Bool) async throws {
    for _ in 0..<400 {
        if cond() { return }
        try await Task.sleep(nanoseconds: 5_000_000)
    }
    Issue.record("timed out waiting for \(what)")
}

@Suite("memory store — the kernel's execution rows")
struct MemoryStoreTests {

    // ── the tail ──

    @Test func rowsStreamReplaysAfterThenTailsWithoutDuplicates() async throws {
        let k = makeKernel(MemProbe())
        let a = try k.rowsPut(AgentId("probe"), ["kind": .string("call"), "verb": .string("one")])
        let b = try k.rowsPut(AgentId("probe"), ["kind": .string("call"), "verb": .string("two")])
        let tail = k.rows.subscribe(after: a)
        let consumer = Task { () -> [Int64] in
            var got: [Int64] = []
            for await id in tail.ids {
                got.append(id)
                if got.count == 3 { return got }
            }
            return got
        }
        try await Task.sleep(nanoseconds: 10_000_000)
        let c = try k.rowsPut(AgentId("probe"), ["kind": .string("call"), "verb": .string("three")])
        let d = try k.rowsPut(AgentId("other"), ["kind": .string("call"), "verb": .string("four")])
        let got = await consumer.value
        #expect(got == [b, c, d], "replay (> after) then live, once each")
    }

    @Test func rowsPutAcceptsAPreMintedUnusedIdAndRefusesTheRest() throws {
        let k = makeKernel(MemProbe())
        let pre = k.mintId()
        let rid = try k.rowsPut(AgentId("probe"), ["id": .integer(pre), "kind": .string("task")])
        #expect(rid == pre)
        #expect(throws: RowStoreError.self) {
            try k.rowsPut(AgentId("probe"), ["id": .integer(pre), "kind": .string("task")])
        }
        #expect(throws: RowStoreError.self) {
            try k.rowsPut(AgentId("probe"), ["id": .integer(k.nextId + 5), "kind": .string("task")])
        }
    }

    // ── the drain and the agents half of a stop ──

    @Test func drainingRefusesNewRootsButNotShutdownTraffic() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        k.setDraining(true)
        let out = await call(k, "probe", verb("work"))
        #expect(out.status == .failed && out.result["reason"].asString == "draining")
        #expect(out.pid == nil, "a refused root costs no id")
        let ok = await KernelTaskLocals.withShutdown { await call(k, "probe", verb("work")) }
        #expect(ok.status == .completed)
        k.setDraining(false)
    }

    @Test func shutdownAgentsTellsAdvertisersWaitsThenCancelsTheRest() async throws {
        let probe = MemProbe()
        let k = makeKernel(probe)
        await mk(k, "sp_a")
        await mk(k, "sp_b")
        k.quietTimeout = 0.3
        let slow = Task { await call(k, "sp_b", verb("slow")) }
        try await waitUntil("the slow handler") { probe.running("sp_b") }
        let out = await call(k, "kernel", verb("shutdown_agents")).result
        #expect(out["drained"].asBool == true)
        #expect(
            Set((out["shut"].asArray ?? []).compactMap { $0.asString }) == ["sp_a", "sp_b"],
            "\(out.serialize())")
        #expect(Set(probe.shut) == ["sp_a", "sp_b"], "every advertiser was told")
        #expect(!(out["cancelled"].asArray ?? []).isEmpty, "the slow task was cancelled")
        let settled = await slow.value
        #expect(settled.status == .cancelled)
        let r = await call(k, "sp_a", verb("work"))
        #expect(r.result["reason"].asString == "draining")
        k.setDraining(false)
    }

    // ── kernel_start: no store, no start ──

    @Test func kernelStartFailsLoudWithoutAStore() async {
        let k = makeKernel(MemProbe())
        let out = await call(k, "kernel", verb("kernel_start")).result
        #expect(out["reason"].asString == "missing_arg" && out["arg"].asString == "memory_store_id")
        #expect(out["hint"].asString?.contains("ram_store") == true, "the refusal says what to create")
        #expect(!k.started)
    }

    /// python `self.agents.get(sid)` — a LITERAL id: `kernel` is the root's
    /// send address, never a store, so it names no agent.
    @Test func aMemoryStoreIdOfKernelNamesNoAgent() async {
        let k = makeKernel(MemProbe())
        k.update(AgentId("kernel_state"), ["memory_store_id": .string("kernel")])
        let out = await k.start(bootAll: false)
        #expect(out["reason"].asString == "unknown_agent", "\(out.serialize())")
        #expect(
            out["hint"].asString == "the root's `memory_store_id` names no agent in this tree",
            "\(out.serialize())")
    }

    @Test func kernelStartRefusesAStoreThatDoesNotServeTheOperator() async {
        let k = makeKernel(MemProbe())
        await withStore(k, operator: false, start: false)
        let out = await k.start(bootAll: false)
        #expect(out["reason"].asString == "unauthorized", "\(out.serialize())")
        #expect(out["hint"].asString?.contains("operator") == true)
        #expect(!k.started)
    }

    @Test func kernelStartBootsTheStoreFirstThenEveryone() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        await withStore(k, start: false)
        let out = await call(k, "kernel", verb("kernel_start")).result
        #expect(out["started"].asBool == true && out["store"].asString == "memory")
        let booted = (out["booted"].asArray ?? []).compactMap { $0.asString }
        #expect(Array(booted.prefix(2)) == ["kernel_state", "memory"], "\(out.serialize())")
        #expect(booted.contains("probe"))
        let again = await call(k, "kernel", verb("kernel_start")).result
        #expect(again["already"].asBool == true)
    }

    // ── the flusher ──

    @Test func executionRowsFlushIntoTheStoreAndLeaveRam() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        await withStore(k)
        let out = await call(k, "probe", verb("work"))
        let pid = out.pid!.value
        await k.flushExecutions()
        let rows = await kernelRows(k)
        let c = rows.first { $0["id"].asInt == pid }
        #expect(c?["kind"].asString == "call" && c?["caller"] == .null)
        let settle = rows.first { $0["kind"].asString == "settle" && $0["parent"].asInt == pid }
        // UNNAMED: the settle keeps a digest, never the body (the growth fix)
        #expect(
            settle?["outcome"]
                == .object([
                    "status": .string("completed"), "bytes": .integer(Int64("{\"ok\": true}".count)),
                ]), "\(settle?.serialize() ?? "nil")")
        #expect(rows.contains { $0["kind"].asString == "flush" && ($0["covers"].asArray ?? []).contains(.integer(pid)) })
        #expect(!k.rows.contains(pid), "flushed rows leave RAM")
        #expect(!k.ledger.has(pid), "a settled, flushed execution too")
    }

    @Test func storedRowsKeepNoneAndReduceBytesToALength() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        await withStore(k)
        let out = await k.send([
            Item(
                AgentId("probe"), .object(["type": .string("work"), "none": .null]),
                binary: Data([0, 1, 2]))
        ])[0]
        await k.flushExecutions()
        let c = await kernelRows(k).first { $0["id"].asInt == out.pid!.value }
        #expect(c?["payload"]["bytes"] == .object(["len": .integer(3)]))
        #expect(c?["payload"].asObject?.keys.contains("none") == true)
        #expect(c?["payload"]["none"] == .null)
        let before = await call(k, "memory", verb("dump")).result["tables"]
        _ = await call(k, "memory", verb("inject", ["tables": before]))
        let after = await call(k, "memory", verb("dump")).result["tables"]
        let n = before["rows"].asArray?.count ?? 0
        #expect(Array((after["rows"].asArray ?? []).prefix(n)) == (before["rows"].asArray ?? []))
        #expect(after["fold_refs"] == before["fold_refs"] && after["grants"] == before["grants"])
    }

    @Test func aSettleKeepsTheBodyOnlyWhenNamedOrTheErrorWhenDead() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        await withStore(k)
        let named = await call(k, "probe", verb("work"), id: "n-1")
        let plain = await call(k, "probe", verb("work"))
        let dead = await call(k, "probe", verb("fail"))
        await k.flushExecutions()
        var kept: [Int64: JSON] = [:]
        for r in await kernelRows(k) where r["kind"].asString == "settle" {
            kept[r["parent"].asInt ?? 0] = r["outcome"]
        }
        #expect(
            kept[named.pid!.value]
                == .object(["status": .string("completed"), "result": .object(["ok": .bool(true)])]))
        #expect(
            kept[plain.pid!.value] == .object(["status": .string("completed"), "bytes": .integer(12)]))
        #expect(kept[dead.pid!.value]?["status"].asString == "failed")
        #expect(kept[dead.pid!.value]?["error"].asString?.contains("probe failed on purpose") == true)
        #expect(kept[dead.pid!.value]?.asObject?["result"] == nil)
    }

    @Test func repeatedLedgerCallsGrowTheStoreLinearly() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        await withStore(k)
        func size() async -> Int {
            await k.flushExecutions()
            let t = await call(k, "memory", verb("dump")).result
            return t["tables"].serialize().count
        }
        var sizes = [await size()]
        for _ in 0..<6 {
            _ = await call(k, "kernel", verb("ledger"))
            sizes.append(await size())
        }
        let steps = zip(sizes, sizes.dropFirst()).map { $1 - $0 }
        #expect(steps.dropFirst().max()! < 2 * steps[1], "not linear: \(sizes)")
    }

    @Test func theFlushRecordsItselfAndNeverLoops() async throws {
        let k = makeKernel(MemProbe())
        await mk(k)
        await withStore(k)
        _ = await call(k, "probe", verb("work"))
        try await Task.sleep(nanoseconds: 600_000_000)  // the debounce, several times over
        let n1 = await kernelRows(k).count
        try await Task.sleep(nanoseconds: 600_000_000)
        let rows = await kernelRows(k)
        var byId: [Int64: JSON] = [:]
        for r in rows { if let id = r["id"].asInt { byId[id] = r } }
        for f in rows where f["kind"].asString == "flush" {
            let covered = (f["covers"].asArray ?? []).compactMap { $0.asInt }.compactMap { byId[$0] }
            let onlyFlushes =
                !covered.isEmpty
                && covered.allSatisfy {
                    $0["kind"].asString == "call" && $0["payload"]["type"].asString == "append"
                }
            #expect(!onlyFlushes, "a flush that only re-records flushes — the loop is back: \(f)")
        }
        #expect(rows.count >= n1)
    }

    @Test func ledgerIsAProjectionOfStoreAndRamScopedToTheCaller() async throws {
        let probe = MemProbe()
        let k = makeKernel(probe)
        await mk(k)
        await mk(
            k, "caller",
            [
                "can_call": .object([
                    "probe": .string("*"),
                    "kernel_state": .array([.string("ledger"), .string("ack_deadletter")]),
                ])
            ])
        await withStore(k)
        let mine = await call(k, "probe", verb("work"), id: "mine-1", as: "caller")
        let other = await call(k, "probe", verb("work"))
        await k.flushExecutions()
        let live = Task { await call(k, "probe", verb("slow")) }
        try await waitUntil("the slow handler") { probe.running("probe") }
        let led = await call(k, "kernel", verb("ledger")).result
        var by: [Int64: JSON] = [:]
        for e in led["executions"].asArray ?? [] { by[e["id"].asInt ?? 0] = e }
        #expect(by[mine.pid!.value]?["status"].asString == "completed", "read back from the store")
        #expect(by[mine.pid!.value]?["outcome"]["result"] == .object(["ok": .bool(true)]), "named: kept")
        #expect(by[other.pid!.value]?["outcome"].asObject?["result"] == nil, "unnamed: a digest")
        #expect(by[other.pid!.value]?["outcome"]["bytes"].asInt == 12)
        #expect(
            (led["executions"].asArray ?? []).contains { $0["status"].asString == "in_flight" },
            "RAM overlay")
        let scoped = await call(k, "kernel", verb("ledger"), as: "caller").result
        let ids = Set((scoped["executions"].asArray ?? []).compactMap { $0["id"].asInt })
        #expect(ids.contains(mine.pid!.value) && !ids.contains(other.pid!.value))
        live.cancel()
    }

    @Test func namedDedupSurvivesTheFlush() async {
        let probe = MemProbe()
        let k = makeKernel(probe)
        await mk(k)
        await withStore(k)
        let first = await call(k, "probe", verb("count"), id: "job-1")
        await k.flushExecutions()
        #expect(k.ledger.resolveName("job-1") == nil, "the name left RAM with its rows")
        let again = await call(k, "probe", verb("count"), id: "job-1")
        #expect(again.result == first.result && first.result == .object(["n": .integer(1)]))
        #expect(probe.count("probe") == 1, "the step never ran twice")
    }

    @Test func namedDedupSurvivesARestartOnTheSameStore() async throws {
        let probe = MemProbe()
        let k = makeKernel(probe)
        _ = await call(k, "kernel_state", verb("boot"))  // mints the guid
        await mk(k)
        await withStore(k)
        _ = await call(k, "probe", verb("count"), id: "nightly")
        await k.flushExecutions()
        let tables = await call(k, "memory", verb("dump")).result["tables"]
        // a NEW kernel from the same records: same guid, same counter bound, an
        // empty RAM store
        let json = try k.saveJSON()
        let k2 = makeKernel(probe)
        try k2.loadJSON(json)
        #expect(k2.root?.metaValue(forKey: "guid") == k.root?.metaValue(forKey: "guid"))
        #expect(k2.nextId >= k.nextId - 1, "ids never repeat across a restart")
        _ = await call(k2, "memory", verb("inject", ["tables": tables]))
        let out = await k2.start(bootAll: false)
        #expect(out["started"].asBool == true, "\(out.serialize())")
        let again = await call(k2, "probe", verb("count"), id: "nightly")
        #expect(again.result == .object(["n": .integer(1)]) && probe.count("probe") == 1)
    }

    // ── dead letters ──

    @Test func aDeadLetterIsAnnouncedListedAndAcked() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        await mk(
            k, "caller",
            [
                "can_call": .object([
                    "probe": .string("*"),
                    "kernel_state": .array([.string("ledger"), .string("ack_deadletter")]),
                ])
            ])
        await withStore(k)
        let inbox = k.ensureInbox(AgentId("caller"))
        let out = await call(k, "probe", verb("fail"), as: "caller")
        #expect(out.status == .failed)
        var drained: [JSON] = []
        while let item = inbox.tryTake() { drained.append(item.payload) }
        let ev = drained.first { $0["type"].asString == "on_deadletter" }
        #expect(ev?["id"].asInt == out.pid!.value, "\(String(describing: ev))")
        await k.flushExecutions()
        let led = await call(k, "kernel", verb("ledger"), as: "caller").result
        let entry = (led["executions"].asArray ?? []).first { $0["id"].asInt == out.pid!.value }
        #expect(entry?["status"].asString == "failed" && entry?["acked"] == .null)
        let acked = await call(
            k, "kernel", verb("ack_deadletter", ["id": .integer(out.pid!.value)]), as: "caller"
        ).result
        #expect(acked == .object(["acked": .bool(true), "id": .integer(out.pid!.value)]))
        let ok = await call(k, "probe", verb("work"), as: "caller")
        let refused = await call(
            k, "kernel", verb("ack_deadletter", ["id": .integer(ok.pid!.value)]), as: "caller"
        ).result
        #expect(refused["reason"].asString == "bad_arg", "only a dead letter is acked")
        await k.flushExecutions()
        let led2 = await call(k, "kernel", verb("ledger")).result
        #expect(
            (led2["executions"].asArray ?? []).first { $0["id"].asInt == out.pid!.value }?["acked"]
                == .bool(true))
        #expect(
            await kernelRows(k).contains {
                $0["kind"].asString == "ack" && $0["parent"].asInt == out.pid!.value
            })
    }

    @Test func aGracefulStopLandsInFlightWorkAsCancelledDeadLetters() async throws {
        let probe = MemProbe()
        let k = makeKernel(probe)
        await mk(k)
        await withStore(k)
        k.quietTimeout = 0.2
        let slow = Task { await call(k, "probe", verb("slow")) }
        try await waitUntil("the slow handler") { probe.running("probe") }
        let pid = (k.tasks.rows().first { $0["verb"].asString == "slow" }?["id"].asInt)!
        await k.shutdown()
        _ = await slow.value
        // the store agent survives the walk (ram_store has no teardown hook); the
        // read is the kernel's own shutdown traffic, exempt from the drain
        let rows = await KernelTaskLocals.withShutdown { await kernelRows(k) }
        let settle = rows.first { $0["kind"].asString == "settle" && $0["parent"].asInt == pid }
        #expect(settle?["outcome"]["status"].asString == "cancelled", "a dead letter, flushed")
    }

    // ── retention: the store-side window ──

    @Test func theKernelsStoreIsPrunedToItsWindow() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        await withStore(k)
        let named = await call(k, "probe", verb("work"), id: "keep-me")
        let dead = await call(k, "probe", verb("fail"))
        for _ in 0..<40 { _ = await call(k, "probe", verb("work")) }
        k.ledger.setCap(20)
        await k.flushExecutions()
        let rows = await kernelRows(k)
        let calls = rows.filter { $0["kind"].asString == "call" && !pyTruthy($0["internal"]) }
        let ids = Set(calls.compactMap { $0["id"].asInt })
        #expect(calls.count <= 22, "the store holds \(calls.count) calls against a window of 20")
        #expect(ids.contains(named.pid!.value), "a named execution is kept")
        #expect(ids.contains(dead.pid!.value), "an unacked dead letter waits for a person")
        let settles = Set(rows.filter { $0["kind"].asString == "settle" }.compactMap { $0["parent"].asInt })
        #expect(ids.isSubset(of: settles.union(ids.filter { k.ledger.has($0) })))
    }

    // ── the id counter: block reservation in the root record ──

    @Test func theIdCounterReservesBlocksInTheRootRecord() async throws {
        let k = makeKernel(MemProbe())
        #expect(k.root?.metaValue(forKey: "next_id") == nil, "load never edits a record")
        _ = await call(k, "kernel_state", verb("boot"))
        await withStore(k)
        let n = k.mintId()
        let bound = try #require(k.root?.metaValue(forKey: "next_id")?.asInt)
        #expect(bound > n, "start wrote the reserved bound")
        for _ in 0..<Kernel.ID_BLOCK { _ = k.mintId() }
        let bound2 = try #require(k.root?.metaValue(forKey: "next_id")?.asInt)
        #expect(bound2 > bound, "a started kernel persists the next block")
        // A reboot: the root record comes back through `load`, the counter with it.
        let k2 = makeKernel(MemProbe())
        try k2.load(
            KernelState(records: [
                AgentRecord(
                    id: "kernel_state", handlerModule: "kernel_state.tools",
                    meta: ["next_id": .integer(bound2)])
            ]))
        #expect(k2.mintId() >= bound2, "a reboot resumes past the reserved block")
    }

    @Test func loadRestoresTheCounterWithoutEditingTheRecord() throws {
        let k = makeKernel(MemProbe())
        let snap = KernelState(
            version: KernelState.currentVersion,
            records: [
                AgentRecord(
                    id: "kernel_state", handlerModule: "kernel_state.tools", parentId: nil,
                    meta: ["next_id": .integer(5000)])
            ])
        try k.load(snap)
        #expect(k.nextId == 5000)
        #expect(k.idReserved == 5000 + Kernel.ID_BLOCK)
        #expect(k.root?.metaValue(forKey: "next_id") == .integer(5000))
    }

    @Test func kernelStopAcks() async {
        let k = makeKernel(MemProbe())
        let out = await call(k, "kernel", verb("kernel_stop")).result
        #expect(out == .object(["type": .string("kernel_stop"), "ok": .bool(true)]))
    }

    /// C18: `cancel task_id:"+<pid>"` is NOT the pid — a leading `+` fails
    /// `_as_id`'s digit-string test, and it names no execution either, so it is
    /// the same honest `{cancelled:false}` a stale id gets.
    @Test func cancelWithALeadingPlusIsNeverThePid() async {
        let k = makeKernel(MemProbe())
        await mk(k)
        let out = await call(k, "probe", verb("work"))
        let r = await call(
            k, "kernel", verb("cancel", ["task_id": .string("+\(out.pid!.value)")])
        ).result
        #expect(r["cancelled"].asBool == false, "\(r.serialize())")
        #expect(r["killed"].asArray == [])
    }
}
