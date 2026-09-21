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
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
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
        let a = try k.rows.put(
            AgentId("ai"), row(["role": .string("user"), "content": .string("hello")]))
        let b = try k.rows.put(
            AgentId("ai"),
            row(["role": .string("assistant"), "content": .string("hi"), "parent": .integer(a)]))
        #expect(b == a + 1, "the one counter, in order")
        #expect(k.rows.forAgent(AgentId("ai")).map { $0["id"]?.asInt } == [a, b])
        #expect(k.rows.forAgent(AgentId("nobody")).isEmpty)
        #expect(k.rows.get([b, 999_999, a]).map { $0["id"]?.asInt } == [b, a])
        let ra = try #require(k.rows.getOne(a))
        #expect(ra["agent"]?.asString == "ai" && ra["ts"] != nil)
    }

    @Test func aRowIdNeverCollidesWithALiveTask() async throws {
        let k = bootKernel()
        _ = await k.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string("a"), "handler_module": .string("noop.tools"),
                ]))
        ])
        var ids = Set<Int64>()
        for i in 0..<50 {
            ids.insert(
                try k.rows.put(
                    AgentId("ai"), row(["role": .string("user"), "content": .string("\(i)")])))
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
        try k.rows.viewPut(
            AgentId("ai"),
            row([
                "v": .integer(1),
                "ids": .array([.object(["id": .integer(7), "res": .string("full")])]),
            ]))
        try k.rows.viewPut(
            AgentId("ai"),
            row([
                "v": .integer(1),
                "ids": .array([.object(["id": .integer(8), "res": .string("stub")])]),
            ]))
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
        // "a\r\nb" is 4 CODE POINTS to python (CR and LF each count), not 3 —
        // a `Character`/grapheme slice would read CRLF as one cluster (size 3).
        let crlfPid = k.rows.mintId()
        try k.rows.bodyPut(AgentId("ai"), pid: crlfPid, text: "a\r\nb")
        #expect(k.rows.bodyGet(crlfPid, offset: 0, length: 4)?.3 == 4)
    }

    /// C70: `body_get(pid,50,10)` on the 4-code-point body `a\r\nb` answers
    /// `("", 50, true, 4)` — `nextOffset` echoes the CALLER's offset past the
    /// end, never walked back down to `size`, exactly like python's
    /// `nxt = offset + len(chunk)` with an empty chunk.
    @Test func bodiesPagePastTheEndKeepTheOffset() throws {
        let k = bootKernel()
        let pid = k.rows.mintId()
        try k.rows.bodyPut(AgentId("ai"), pid: pid, text: "a\r\nb")
        let p = try #require(k.rows.bodyGet(pid, offset: 50, length: 10))
        #expect(p == ("", 50, true, 4))
        // A 12-code-point body, pulled at a wildly out-of-range offset.
        let bigPid = k.rows.mintId()
        try k.rows.bodyPut(AgentId("ai"), pid: bigPid, text: "0123456789ab")
        let big = try #require(k.rows.bodyGet(bigPid, offset: 9999, length: 1))
        #expect(big == ("", 9999, true, 12))
    }

    /// C70: `body_get` pages by CODE POINTS with a python slice, exactly
    /// pinning the plan's measured python outputs on `a\r\nb`.
    @Test func bodyGetSlicesCodePoints() throws {
        let k = bootKernel()
        let pid = k.rows.mintId()
        try k.rows.bodyPut(AgentId("ai"), pid: pid, text: "a\r\nb")
        #expect(k.rows.bodyGet(pid, offset: 50, length: 10)! == ("", 50, true, 4))
        #expect(k.rows.bodyGet(pid, offset: 0, length: 2)! == ("a\r", 2, false, 4))
        // A non-string body content is read through `py_str`, like python's
        // `str(row.get("content", ""))` — not `.asString`, which would answer
        // an empty page for a number.
        let numPid = k.rows.mintId()
        _ = try k.rows.put(
            AgentId("ai"),
            row(["kind": .string("body"), "parent": .integer(numPid), "content": .integer(5)]))
        #expect(k.rows.bodyGet(numPid, offset: 0, length: 1)! == ("5", 1, true, 1))
    }

    @Test func putAcceptsAPreMintedUnusedIdAndRefusesTheRest() throws {
        let k = bootKernel()
        let reserved = k.rows.mintId()
        let later = try k.rows.put(AgentId("n"), row(["kind": .string("call")]))
        #expect(later == reserved + 1)
        let landed = try k.rows.put(
            AgentId("ai"), row(["id": .integer(reserved), "content": .string("late")]))
        #expect(landed == reserved)
        #expect(throws: RowStoreError.self) {
            try k.rows.put(
                AgentId("ai"), row(["id": .integer(reserved), "content": .string("dup")]))
        }
        #expect(throws: RowStoreError.self) {
            try k.rows.put(
                AgentId("ai"), row(["id": .integer(reserved + 50), "content": .string("unminted")]))
        }
        #expect(throws: RowStoreError.self) {
            try k.rows.put(
                AgentId("ai"), row(["id": .string("7"), "content": .string("string id")]))
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
        let rid = try k.rows.put(
            AgentId("ai"), row(["role": .string("user"), "content": .string("kept")]))
        _ = await k.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string("ai"), "handler_module": .string("noop.tools"),
                ]))
        ])
        let d = await k.send([
            Item(
                AgentId("kernel_state"),
                .object(["type": .string("delete_agent"), "id": .string("ai")]))
        ])[0].result
        #expect(d["error"].isNull, "\(d.serialize())")
        #expect(!k.rows.get([rid]).isEmpty && !k.rows.forAgent(AgentId("ai")).isEmpty)
    }

    // ── the tail ──────────────────────────────────────────────────────────

    @Test func rowsStreamReplaysAfterThenTailsWithoutDuplicates() async throws {
        let k = bootKernel()
        let a = try k.rows.put(
            AgentId("ai"), row(["role": .string("user"), "content": .string("one")]))
        let b = try k.rows.put(
            AgentId("ai"), row(["role": .string("user"), "content": .string("two")]))
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
        let c = try k.rows.put(
            AgentId("ai"), row(["role": .string("user"), "content": .string("three")]))
        let d = try k.rows.put(
            AgentId("other"), row(["role": .string("user"), "content": .string("four")]))
        let got = await consumer.value
        #expect(got == [b, c, d], "replay (> after) then live, once each")
    }

    @Test func rowsStreamKeepsAPreMintedRowThatLandsLate() async throws {
        let k = bootKernel()
        let base = try k.rows.put(
            AgentId("ai"), row(["role": .string("user"), "content": .string("root")]))
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
        let msg = k.rows.mintId()  // reserved now, written later
        let later = try k.rows.put(
            AgentId("notes"), row(["kind": .string("call"), "verb": .string("read")]))
        let later2 = try k.rows.put(
            AgentId("notes"), row(["kind": .string("call"), "verb": .string("read")]))
        try k.rows.put(
            AgentId("ai"),
            row([
                "id": .integer(msg), "role": .string("assistant"), "content": .string("the message"),
            ]))
        let got = await consumer.value
        #expect(got == [later, later2, msg], "arrival order, the late pre-minted row kept")
    }
}
