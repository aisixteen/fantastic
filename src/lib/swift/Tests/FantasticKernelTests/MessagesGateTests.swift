// The `messages` verb's in-handler gate, and hydration's PRIVILEGED pair
// `rows_read`/`rows_write`. Twin of python `tests/kernel/test_rows.py` (the
// verb half); rust `tests/messages_gate.rs`.

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

private func send(_ k: Kernel, _ target: String, _ payload: JSON) async -> JSON {
    await k.send([Item(AgentId(target), payload)])[0].result
}

private func mk(_ k: Kernel, _ id: String, _ meta: [String: JSON] = [:]) async {
    var p: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"), "parent_id": .string("core"),
        "id": .string(id), "handler_module": .string("noop.tools"),
    ]
    for (kk, v) in meta { p[kk] = v }
    let r = await send(k, "core", .object(p))
    #expect(r["error"].isNull, "\(r.serialize())")
}

private func asSender<T>(_ id: String, _ body: () async -> T) async -> T {
    await KernelTaskLocals.$currentSender.withValue(AgentId(id)) { await body() }
}

private func put(_ k: Kernel, _ agent: String, _ pairs: KeyValuePairs<String, JSON>) -> Int64 {
    var r: Row = [:]
    for (kk, v) in pairs { r[kk] = v }
    return try! k.rows.put(AgentId(agent), r)
}

private func ids(_ r: JSON) -> [Int64] { (r["rows"].asArray ?? []).compactMap { $0["id"].asInt } }
private func refused(_ r: JSON) -> [Int64] { (r["refused"].asArray ?? []).compactMap { $0.asInt } }

@Suite("messages gate")
struct MessagesGateTests {
    @Test func operatorSeesAllAndAnswersDigestsNotBodies() async {
        let k = bootKernel()
        let a = put(k, "ai", ["role": .string("user"), "content": .string(String(repeating: "x", count: 5000)), "kind": .string("reply")])
        let f = put(k, "ai", [
            "role": .string("user"), "kind": .string("fold"), "title": .string("done"),
            "refs": .array([.integer(a)]), "level": .integer(1), "content": .string("[fold]"),
        ])
        let r = await send(k, "kernel", .object(["type": .string("messages"), "ids": .array([.integer(a), .integer(f), .integer(999_999)])]))
        #expect(refused(r) == [999_999], "\(r.serialize())")
        #expect(ids(r) == [a, f])
        let rows = r["rows"].asArray ?? []
        #expect(rows[0]["bytes"].asInt == 5000 && (rows[0]["head"].asString ?? "").count <= 120)
        #expect(rows[1]["title"].asString == "done" && rows[1]["refs"].asInt == 1 && rows[1]["level"].asInt == 1)
    }

    @Test func ownRowsAlwaysAPeersOnlyByGrant() async {
        let k = bootKernel()
        await mk(k, "ai", ["can_call": .object(["core": .array([.string("messages")])])])
        await mk(k, "peer", ["can_call": .object(["core": .array([.string("messages")])])])
        let mine = put(k, "ai", ["role": .string("user"), "content": .string("private")])
        let theirs = put(k, "peer", ["role": .string("user"), "content": .string("peer's")])
        // the AI reads its own, is refused the peer's
        var r = await asSender("ai") {
            await send(k, "kernel", .object(["type": .string("messages"), "ids": .array([.integer(mine), .integer(theirs)])]))
        }
        #expect(ids(r) == [mine] && refused(r) == [theirs], "\(r.serialize())")
        // the peer is refused the AI's — until the AI's callable_by names it for `messages`
        r = await asSender("peer") {
            await send(k, "kernel", .object(["type": .string("messages"), "ids": .array([.integer(mine)])]))
        }
        #expect(ids(r) == [] && refused(r) == [mine], "\(r.serialize())")
        _ = await send(k, "core", .object([
            "type": .string("update_agent"), "id": .string("ai"),
            "callable_by": .object(["peer": .array([.string("messages")])]),
        ]))
        r = await asSender("peer") {
            await send(k, "kernel", .object(["type": .string("messages"), "ids": .array([.integer(mine)])]))
        }
        #expect(ids(r) == [mine] && refused(r) == [], "\(r.serialize())")
    }

    @Test func pagesAndCaps() async {
        let k = bootKernel()
        let all = (0..<400).map { _ in put(k, "ai", ["role": .string("user"), "content": .string(String(repeating: "y", count: 200))]) }
        let idsJ: JSON = .array(all.map { .integer($0) })
        let r = await send(k, "kernel", .object(["type": .string("messages"), "ids": idsJ, "limit": .integer(5)]))
        #expect(ids(r).count == 5 && r["next"].asInt == all[4], "\(r["next"])")
        let r2 = await send(k, "kernel", .object(["type": .string("messages"), "ids": idsJ, "after": r["next"], "limit": .integer(5)]))
        #expect(ids(r2).first == all[5])
        let big = await send(k, "kernel", .object(["type": .string("messages"), "ids": idsJ]))
        #expect(ids(big).count < 400 && big["next"].asInt != nil, "the 32KB cap paged it")
    }

    @Test func refusesABadIdsArg() async {
        let k = bootKernel()
        let r = await send(k, "kernel", .object(["type": .string("messages")]))
        #expect(r["error"].asString != nil && r["reason"].asString == "bad_arg" && r["arg"].asString == "ids")
        let r2 = await send(k, "kernel", .object(["type": .string("messages"), "ids": .array([.string("a:b")])]))
        #expect(r2["reason"].asString == "bad_arg")
        let r3 = await send(k, "kernel", .object(["type": .string("messages"), "ids": .array([.string("12")])]))
        #expect(r3["reason"].isNull, "a decimal string IS an id: \(r3.serialize())")
    }

    // ── rows_read / rows_write: hydration's pair, operator-only ──────────

    @Test func rowsReadReturnsWholeRowsOldestFirstAndPages() async {
        let k = bootKernel()
        let all = (0..<4).map { i in put(k, "ai", ["role": .string("user"), "content": .string("turn \(i)")]) }
        let got = await send(k, "core", .object(["type": .string("rows_read"), "agent": .string("ai")]))
        #expect(ids(got) == all)
        #expect(got["rows"].asArray?[0]["content"].asString == "turn 0", "WHOLE, not a digest")
        #expect(got["next"].isNull)
        let page = await send(k, "core", .object(["type": .string("rows_read"), "agent": .string("ai"), "limit": .integer(2)]))
        #expect(ids(page) == Array(all[..<2]) && page["next"].asInt == all[1])
        let rest = await send(k, "core", .object(["type": .string("rows_read"), "agent": .string("ai"), "after": page["next"]]))
        #expect(ids(rest) == Array(all[2...]))
        let none = await send(k, "core", .object(["type": .string("rows_read"), "agent": .string("nobody")]))
        #expect(none["rows"].asArray?.isEmpty == true, "empty, not an error")
    }

    @Test func rowsWriteAppendsUnderFreshIds() async {
        let k = bootKernel()
        let src = put(k, "ai", ["role": .string("user"), "content": .string("the original")])
        let rows: [JSON] = k.rows.forAgent(AgentId("ai")).map { r in
            var c = r
            for key in ["id", "agent", "ts"] { c.removeValue(forKey: key) }
            return .object(c)
        }
        let out = await send(k, "core", .object(["type": .string("rows_write"), "agent": .string("ai_fork"), "rows": .array(rows)]))
        let outIds = (out["ids"].asArray ?? []).compactMap { $0.asInt }
        #expect(outIds.count == 1 && outIds[0] != src, "a copy is a NEW row: \(out.serialize())")
        let copied = k.rows.forAgent(AgentId("ai_fork"))
        #expect(copied[0]["content"]?.asString == "the original" && copied[0]["agent"]?.asString == "ai_fork")
        #expect(k.rows.forAgent(AgentId("ai")).map { $0["id"]?.asInt } == [src], "the original is untouched")
    }

    @Test func rowsWriteRefusesARowThatCarriesItsOwnId() async {
        let k = bootKernel()
        let r = await send(k, "core", .object([
            "type": .string("rows_write"), "agent": .string("ai"),
            "rows": .array([.object(["id": .integer(7), "content": .string("x")])]),
        ]))
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "rows")
        #expect((r["error"].asString ?? "").contains("may not carry an `id`"))
    }

    @Test func badArgsAreNamed() async {
        let k = bootKernel()
        let cases: [(JSON, String)] = [
            (.object(["type": .string("rows_read")]), "agent"),
            (.object(["type": .string("rows_write"), "agent": .string("ai")]), "rows"),
            (.object(["type": .string("rows_write"), "agent": .string("ai"), "rows": .array([])]), "rows"),
            (.object(["type": .string("rows_write"), "agent": .string("ai"), "rows": .array([.string("nope")])]), "rows"),
        ]
        for (payload, arg) in cases {
            let r = await send(k, "core", payload)
            #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == arg, "\(payload.serialize()) → \(r.serialize())")
        }
    }

    @Test func aWildcardNeverCarriesThePairButANamedGrantDoes() async {
        let k = bootKernel()
        _ = put(k, "ai", ["role": .string("user"), "content": .string("private")])
        // Wide open BOTH ways, and it still refuses: `*` is not a yes to these.
        await mk(k, "peer", ["can_call": .object(["core": .string("*")])])
        _ = await send(k, "core", .object([
            "type": .string("update_agent"), "id": .string("core"),
            "callable_by": .object(["peer": .string("*")]),
        ]))
        for payload: JSON in [
            .object(["type": .string("rows_read"), "agent": .string("ai")]),
            .object(["type": .string("rows_write"), "agent": .string("ai"), "rows": .array([.object(["content": .string("x")])])]),
        ] {
            let r = await asSender("peer") { await send(k, "kernel", payload) }
            #expect(r["reason"].asString == "unauthorized", "\(payload.serialize()) → \(r.serialize())")
        }
        #expect(k.rows.forAgent(AgentId("ai")).count == 1, "nothing was written")
        // NAMED, and it opens — on both sides of the gate.
        _ = await send(k, "core", .object([
            "type": .string("update_agent"), "id": .string("peer"),
            "can_call": .object(["core": .array([.string("rows_read")])]),
        ]))
        _ = await send(k, "core", .object([
            "type": .string("update_agent"), "id": .string("core"),
            "callable_by": .object(["peer": .array([.string("rows_read")])]),
        ]))
        let r = await asSender("peer") {
            await send(k, "kernel", .object(["type": .string("rows_read"), "agent": .string("ai")]))
        }
        #expect((r["rows"].asArray ?? []).map { $0["content"].asString } == ["private"], "\(r.serialize())")
    }
}
