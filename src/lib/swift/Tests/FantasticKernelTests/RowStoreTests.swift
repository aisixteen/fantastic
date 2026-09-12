// The kernel row store: rows, bodies, views, the tail. Twin of python
// `tests/kernel/test_rows.py` (the store half) and
// `test_message_persistence.py::test_rows_stream_*`; rust `rows/tests.rs`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct Noop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}

private func bootKernel() -> Kernel {
    let registry = BundleRegistry()
    registry.register("noop.tools", Noop())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func row(_ pairs: KeyValuePairs<String, JSON>) -> Row {
    var r: Row = [:]
    for (k, v) in pairs { r[k] = v }
    return r
}

@Suite("row store")
struct RowStoreTests {
    @Test func rowsPutMintsInTheExecutionIdSpaceAndKeepsOrder() throws {
        let k = bootKernel()
        let a = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("hello")]))
        let b = try k.rows.put(
            AgentId("ai"), row(["role": .string("assistant"), "content": .string("hi"), "parent": .integer(a)]))
        #expect(b == a + 1, "the one counter, in order")
        #expect(k.rows.forAgent(AgentId("ai")).map { $0["id"]?.asInt } == [a, b])
        #expect(k.rows.forAgent(AgentId("nobody")).isEmpty)
        #expect(k.rows.get([b, 999_999, a]).map { $0["id"]?.asInt } == [b, a])
        let ra = try #require(k.rows.getOne(a))
        #expect(ra["agent"]?.asString == "ai" && ra["ts"] != nil)
    }

    @Test func aRowIdNeverCollidesWithALiveTask() async throws {
        let k = bootKernel()
        _ = await k.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"), "parent_id": .string("core"),
            "id": .string("a"), "handler_module": .string("noop.tools"),
        ]))])
        var ids = Set<Int64>()
        for i in 0..<50 {
            ids.insert(try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("\(i)")])))
        }
        #expect(ids.count == 50)
        let live = Set(k.tasks.rows().compactMap { $0["id"].asInt })
        #expect(ids.isDisjoint(with: live))
    }

    @Test func bodiesPageAndViewsRoundTrip() throws {
        let k = bootKernel()
        let pid = k.rows.mintId()
        try k.rows.bodyPut(AgentId("ai"), pid: pid, text: "0123456789")
        let p1 = try #require(k.rows.bodyGet(pid, offset: 0, length: 4))
        #expect(p1 == ("0123", 4, false, 10))
        let p2 = try #require(k.rows.bodyGet(pid, offset: 8, length: 4))
        #expect(p2 == ("89", 10, true, 10))
        #expect(k.rows.bodyGet(999, offset: 0, length: 4) == nil)
        try k.rows.viewPut(AgentId("ai"), row(["v": .integer(1), "ids": .array([.object(["id": .integer(7), "res": .string("full")])])]))
        try k.rows.viewPut(AgentId("ai"), row(["v": .integer(1), "ids": .array([.object(["id": .integer(8), "res": .string("stub")])])]))
        #expect(k.rows.viewGet(AgentId("ai"))?["ids"]?.asArray?.first?["id"].asInt == 8)
        #expect(k.rows.viewGet(AgentId("zz")) == nil)
        let kinds = k.rows.forAgent(AgentId("ai")).map { $0["kind"]?.asString }
        #expect(kinds == ["body", "view", "view"], "bodies and views are rows of the one table")
    }

    @Test func bodiesSliceCharactersNotBytes() throws {
        let k = bootKernel()
        let pid = k.rows.mintId()
        try k.rows.bodyPut(AgentId("ai"), pid: pid, text: "héllo wörld")
        let p = try #require(k.rows.bodyGet(pid, offset: 0, length: 5))
        #expect(p.0 == "héllo" && p.1 == 5 && p.3 == 11)
    }

    @Test func putAcceptsAPreMintedUnusedIdAndRefusesTheRest() throws {
        let k = bootKernel()
        let reserved = k.rows.mintId()
        let later = try k.rows.put(AgentId("n"), row(["kind": .string("call")]))
        #expect(later == reserved + 1)
        let landed = try k.rows.put(AgentId("ai"), row(["id": .integer(reserved), "content": .string("late")]))
        #expect(landed == reserved)
        #expect(throws: RowStoreError.self) {
            try k.rows.put(AgentId("ai"), row(["id": .integer(reserved), "content": .string("dup")]))
        }
        #expect(throws: RowStoreError.self) {
            try k.rows.put(AgentId("ai"), row(["id": .integer(reserved + 50), "content": .string("unminted")]))
        }
        #expect(throws: RowStoreError.self) {
            try k.rows.put(AgentId("ai"), row(["id": .string("7"), "content": .string("string id")]))
        }
    }

    @Test func setNextIdNeverLowers() {
        let k = bootKernel()
        k.rows.setNextId(100)
        #expect(k.rows.nextId == 100)
        k.rows.setNextId(5)
        #expect(k.rows.nextId == 100)
        #expect(k.rows.mintId() == 100)
    }

    @Test func stampMergesInPlaceWithoutRenotify() throws {
        let k = bootKernel()
        let id = try k.rows.put(AgentId("t"), row(["kind": .string("call"), "verb": .string("x")]))
        k.rows.stamp(id, row(["outcome": .object(["status": .string("completed")])]))
        #expect(k.rows.getOne(id)?["outcome"]?["status"].asString == "completed")
        #expect(k.rows.getOne(id)?["verb"]?.asString == "x", "merge, not replace")
        k.rows.stamp(999_999, row(["outcome": .null]))  // unknown: a no-op
    }

    @Test func rowsOutliveDeleteAgent() async throws {
        let k = bootKernel()
        let rid = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("kept")]))
        _ = await k.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"), "parent_id": .string("core"),
            "id": .string("ai"), "handler_module": .string("noop.tools"),
        ]))])
        let d = await k.send([Item(AgentId("core"), .object(["type": .string("delete_agent"), "id": .string("ai")]))])[0].result
        #expect(d["error"].isNull, "\(d.serialize())")
        #expect(!k.rows.get([rid]).isEmpty && !k.rows.forAgent(AgentId("ai")).isEmpty)
    }

    // ── the tail ──────────────────────────────────────────────────────────

    @Test func rowsStreamReplaysAfterThenTailsWithoutDuplicates() async throws {
        let k = bootKernel()
        let a = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("one")]))
        let b = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("two")]))
        let tail = k.rows.subscribe(after: a)
        let consumer = Task { () -> [Int64] in
            var got: [Int64] = []
            for await id in tail.ids {
                got.append(id)
                if got.count == 3 { break }
            }
            return got
        }
        try await Task.sleep(nanoseconds: 10_000_000)
        let c = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("three")]))
        let d = try k.rows.put(AgentId("other"), row(["role": .string("user"), "content": .string("four")]))
        let got = await consumer.value
        #expect(got == [b, c, d], "replay (> after) then live, once each")
    }

    @Test func rowsStreamKeepsAPreMintedRowThatLandsLate() async throws {
        let k = bootKernel()
        let base = try k.rows.put(AgentId("ai"), row(["role": .string("user"), "content": .string("root")]))
        let tail = k.rows.subscribe(after: base)
        let consumer = Task { () -> [Int64] in
            var got: [Int64] = []
            for await id in tail.ids {
                got.append(id)
                if got.count == 3 { break }
            }
            return got
        }
        try await Task.sleep(nanoseconds: 10_000_000)
        let turn = k.rows.mintId()  // reserved now, written later
        let later = try k.rows.put(AgentId("notes"), row(["kind": .string("call"), "verb": .string("read")]))
        let later2 = try k.rows.put(AgentId("notes"), row(["kind": .string("call"), "verb": .string("read")]))
        try k.rows.put(AgentId("ai"), row(["id": .integer(turn), "role": .string("assistant"), "content": .string("the turn")]))
        let got = await consumer.value
        #expect(got == [later, later2, turn], "arrival order, the late pre-minted row kept")
    }
}
