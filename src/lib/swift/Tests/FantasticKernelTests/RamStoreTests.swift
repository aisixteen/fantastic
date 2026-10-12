// ram_store: tenancy by caller, fold refs + FK cascade, search, inject/dump.
//
// Callers act through `currentSender` (a real agent send, gated both ways); the
// operator is a bare `kernel.send`. The root's guid is set by hand — the root is
// not booted in these tests, and the store keys every row by it.
//
// Twin of python `bundled_agents/memory/ram_store/tests/test_ram_store.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel
@testable import FantasticRamStore

private func call(_ k: Kernel, _ payload: JSON, as sender: String? = nil) async -> JSON {
    let item = Item(AgentId("mem"), payload)
    if let sender {
        return await KernelTaskLocals.$sender.withValue(AgentId(sender)) {
            await k.send([item])[0].result
        }
    }
    return await k.send([item])[0].result
}

private func p(_ t: String, _ extra: OrderedDictionary<String, JSON> = [:]) -> JSON {
    var o: OrderedDictionary<String, JSON> = ["type": .string(t)]
    for (k, v) in extra { o[k] = v }
    return .object(o)
}

private func row(_ id: Int64, _ extra: OrderedDictionary<String, JSON> = [:]) -> JSON {
    var o: OrderedDictionary<String, JSON> = ["id": .integer(id)]
    for (k, v) in extra { o[k] = v }
    return .object(o)
}

private func store() async -> Kernel {
    let r = BundleRegistry()
    r.register("ram_store.tools", RamStoreBundle())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
        meta: ["guid": .string("g1")])
    k.register(root)
    k.setRoot(root)
    // Bare agents (python `kernel.create(None, id=…)`): no bundle, a grant to
    // reach the store.
    for aid in ["alice", "bob", "carol", "leg"] {
        let a = Agent(
            id: AgentId(aid), handlerModule: nil, parentId: root.id,
            meta: ["can_call": .object(["mem": .string("*")])])
        k.register(a)
        root.insertChild(a)
    }
    let out = await k.send([
        Item(
            "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "id": .string("mem"), "handler_module": .string("ram_store.tools"),
                "callable_by": .object([
                    "alice": .string("*"), "bob": .string("*"), "carol": .string("*"),
                    "leg": .string("*"),
                ]),
            ]))
    ])[0].result
    #expect(out["id"].asString == "mem", "\(out.serialize())")
    return k
}

private func append(_ k: Kernel, _ who: String, _ rows: [JSON]) async -> JSON {
    await call(k, p("append", ["rows": .array(rows)]), as: who)
}

private func ids(_ r: JSON) -> [Int64] { (r["rows"].asArray ?? []).compactMap { $0["id"].asInt } }

@Suite("ram_store — the memory store in RAM")
struct RamStoreTests {
    @Test func rowsAreKeyedByCaller() async {
        let k = await store()
        let out = await append(k, "alice", [row(101, ["role": .string("user"), "content": .string("hi")])])
        #expect(out == .object(["ids": .array([.integer(101)]), "appended": .integer(1)]))
        // the same id is free under another owner — keys are (owner, id)
        let bob = await append(k, "bob", [row(101, ["content": .string("bob")])])
        #expect(bob["ids"] == .array([.integer(101)]))
        let a = await call(k, p("read"), as: "alice")
        let b = await call(k, p("read"), as: "bob")
        #expect((a["rows"].asArray ?? []).map { $0["content"] } == [.string("hi")])
        #expect((b["rows"].asArray ?? []).map { $0["content"] } == [.string("bob")])
        // a duplicate id under one owner is refused, all or none
        let dup = await append(k, "alice", [row(102), row(101)])
        #expect(dup["reason"].asString == "exists")
        #expect(ids(await call(k, p("read"), as: "alice")).count == 1)
    }

    @Test func ownerIsNeverAWritableField() async {
        let k = await store()
        // `owner` is no argument: the rows land under the CALLER's key
        _ = await call(
            k, p("append", ["owner": .string("bob"), "rows": .array([row(5)])]), as: "alice")
        #expect(ids(await call(k, p("read"), as: "alice")) == [5])
        #expect(ids(await call(k, p("read", ["owner": .string("alice")]), as: "bob")) == [])
    }

    @Test func missingIdTakesTheKernelsNext() async {
        let k = await store()
        let before = k.nextId
        let out = await append(k, "alice", [.object(["content": .string("x")])])
        #expect((out["ids"][0].asInt ?? 0) >= before)
    }

    @Test func theOperatorIsServedAsKernel() async {
        let k = await store()
        _ = await call(k, p("append", ["rows": .array([row(7, ["kind": .string("call")])])]))
        let got = await call(k, p("read", ["kinds": .array([.string("call")])]))
        #expect(ids(got) == [7])
        // the operator's rows are its own key — alice sees none of them
        #expect((await call(k, p("read"), as: "alice"))["rows"] == .array([]))
    }

    @Test func refsMustNameOwnRows() async {
        let k = await store()
        _ = await append(k, "bob", [row(10)])
        var out = await append(
            k, "alice",
            [row(20, ["kind": .string("fold"), "refs": .array([.integer(10)])])])
        #expect(out["reason"].asString == "not_found")
        // members in the same batch are fine
        out = await append(
            k, "alice",
            [row(1), row(2), row(3, ["kind": .string("fold"), "refs": .array([.integer(1), .integer(2)])])])
        #expect(out["ids"] == .array([.integer(1), .integer(2), .integer(3)]))
    }

    @Test func pruneCascadesToFoldRefs() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [row(1), row(2), row(3, ["kind": .string("fold"), "refs": .array([.integer(1), .integer(2)])])])
        let out = await call(k, p("prune", ["before_id": .integer(2)]), as: "alice")
        #expect(out == .object(["pruned": .integer(1)]))
        let fold = await call(k, p("read", ["ids": .array([.integer(3)])]), as: "alice")
        #expect(fold["rows"][0]["refs"] == .array([.integer(2)]))
    }

    @Test func searchLinesAndFoldDescent() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [
                row(1, ["content": .string("alpha\nthe audit marker is 7\nomega")]),
                row(2, ["content": .string("nothing")]),
                row(3, [
                    "kind": .string("fold"), "refs": .array([.integer(1), .integer(2)]),
                    "content": .string("[fold] survey"),
                ]),
            ])
        let got = await call(k, p("search", ["query": .string("AUDIT")]), as: "alice")
        #expect(
            got["matches"]
                == .array([
                    .object([
                        "id": .integer(1), "line": .integer(2),
                        "text": .string("the audit marker is 7"),
                    ])
                ]), "\(got.serialize())")
        let scoped = await call(
            k, p("search", ["query": .string("marker"), "ids": .array([.integer(3)])]), as: "alice")
        #expect(scoped["total"].asInt == 1)
    }

    @Test func readPagingAndSelectors() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [
                row(1, ["kind": .string("call"), "name": .string("job")]),
                row(2, ["kind": .string("clear")]),
                row(3),
                row(4, ["kind": .string("view")]),
            ])
        let head = await call(k, p("read", ["from_last_kind": .string("clear")]), as: "alice")
        #expect(ids(head) == [2, 3, 4])
        let named = await call(k, p("read", ["match": .object(["name": .string("job")])]), as: "alice")
        #expect(ids(named) == [1])
        let lastView = await call(
            k,
            p("read", [
                "kinds": .array([.string("view")]), "order": .string("desc"), "limit": .integer(1),
            ]), as: "alice")
        #expect(ids(lastView) == [4])
        let page = await call(k, p("read", ["limit": .integer(2)]), as: "alice")
        #expect(ids(page) == [1, 2] && page["next"].asInt == 2)
        let rest = await call(k, p("read", ["after": .integer(2)]), as: "alice")
        #expect(ids(rest) == [3, 4])
    }

    @Test func dumpInjectRoundtrip() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [row(1), row(2), row(3, ["kind": .string("fold"), "refs": .array([.integer(1), .integer(2)])])])
        // a plain verb — the gate alone decides who may call it
        #expect((await call(k, p("dump"), as: "alice"))["tables"] != .null)
        let tables = (await call(k, p("dump")))["tables"]
        _ = await call(k, p("prune", ["before_id": .integer(99)]), as: "alice")
        #expect((await call(k, p("read"), as: "alice"))["rows"] == .array([]))
        let out = await call(k, p("inject", ["tables": tables]))
        #expect(out == .object(["rows": .integer(3)]))
        #expect((await call(k, p("dump")))["tables"] == tables)
        // a broken FK is refused and nothing changes
        var bad = tables.asObject ?? [:]
        var f0 = tables["fold_refs"][0].asObject ?? [:]
        f0["members"] = .array([.integer(999)])
        bad["fold_refs"] = .array([.object(f0)])
        #expect((await call(k, p("inject", ["tables": .object(bad)])))["reason"].asString == "bad_arg")
        #expect((await call(k, p("dump")))["tables"] == tables)
    }

    @Test func theReadmeIsPythonVerbatim() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent(
                "python/bundled_agents/memory/ram_store/src/ram_store/readme.md")
        #expect(RamStoreBundle().readme == (try String(contentsOf: url, encoding: .utf8)))
    }

    /// python `_as_id`: an ASCII digit string only — `'²'` and `'٣'` name no
    /// row, like any non-id. Twin of
    /// `test_a_superscript_digit_is_no_id_and_raises_nothing`.
    @Test func aNonAsciiDigitIsNoId() async {
        let k = await store()
        _ = await append(k, "alice", [row(101, ["role": "user", "content": "hi"])])
        let out = await call(k, p("read", ["ids": ["²", "٣", "101"]]), as: "alice")
        #expect(out["error"].isNull, "\(out.serialize())")
        #expect(ids(out) == [101], "\(out.serialize())")
        #expect(asId(.string("٣")) == nil && asId(.string(" 7 ")) == 7 && asId(.string("0")) == 0)
    }

    /// python `limit = _as_id('0')` is 0 — FALSY: `ids[:limit] if limit else
    /// ids`, so every row and no cursor. Swift crashed the process here.
    @Test func aZeroStringLimitReadsEveryRow() async {
        let k = await store()
        _ = await append(k, "alice", [row(1), row(2), row(3)])
        let out = await call(k, p("read", ["limit": "0"]), as: "alice")
        #expect(ids(out) == [1, 2, 3], "\(out.serialize())")
        #expect(out["next"].isNull && out["total"] == 3, "\(out.serialize())")
    }

    /// python `_as_id(...) or 100` / `or 200`: a `'0'` limit or width is falsy
    /// and takes the default.
    @Test func searchZeroStringLimitAndWidthTakeTheDefaults() async {
        let k = await store()
        _ = await append(k, "alice", [row(1, ["role": "user", "content": "needle here"])])
        let out = await call(
            k, p("search", ["query": "needle", "limit": "0", "width": "0"]), as: "alice")
        let m = out["matches"].asArray ?? []
        #expect(m.count == 1 && out["truncated"] == false, "\(out.serialize())")
        #expect(m.first?["text"].asString?.contains("needle here") == true, "\(out.serialize())")
    }

    /// python `row.get("kind") in keep`: a STRING keep is a substring test, a
    /// row with no kind against it raises (nothing deleted), and a keep `in`
    /// cannot search raises too.
    @Test func pruneKeepIsPythonsIn() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [row(1, ["kind": "fold"]), row(2, ["kind": "turn"]), row(3, ["kind": "fold"])])
        let out = await call(k, p("prune", ["before_id": 10, "keep": "fold"]), as: "alice")
        #expect(out["pruned"] == 1, "\(out.serialize())")
        let left = await call(k, p("read"), as: "alice")
        #expect(ids(left) == [1, 3], "\(left.serialize())")

        _ = await append(k, "alice", [row(4)])  // no kind
        let raised = await KernelTaskLocals.$sender.withValue(AgentId("alice")) {
            await k.send([Item(AgentId("mem"), p("prune", ["before_id": 10, "keep": "fold"]))])[0]
        }
        #expect(raised.status == .failed, "\(raised.result.serialize())")
        #expect(
            raised.result["error"].asString
                == "TypeError: 'in <string>' requires string as left operand, not NoneType",
            "\(raised.result.serialize())")
        #expect(ids(await call(k, p("read"), as: "alice")) == [1, 3, 4], "nothing was deleted")
        let notIterable = await KernelTaskLocals.$sender.withValue(AgentId("alice")) {
            await k.send([Item(AgentId("mem"), p("prune", ["before_id": 10, "keep": 5]))])[0]
        }
        #expect(
            notIterable.result["error"].asString
                == "TypeError: argument of type 'int' is not iterable",
            "\(notIterable.result.serialize())")
    }

}
