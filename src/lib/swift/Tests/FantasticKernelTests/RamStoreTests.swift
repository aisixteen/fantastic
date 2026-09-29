// ram_store: tenancy by caller, grants + FK cascade, search, inject/dump.
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
        return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
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
                "operator": .bool(true), "admins": .array([.string("kernel")]),
                "callable_by": .object([
                    "alice": .string("*"), "bob": .string("*"), "carol": .string("*"),
                    "leg": .string("*"),
                    // a relayed call is gated AS the foreign caller: named
                    "g2|scout": .string("*"),
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
        var out = await call(
            k, p("append", ["owner": .string("bob"), "rows": .array([row(5)])]), as: "alice")
        #expect(out["reason"].asString == "unauthorized")
        out = await call(k, p("read", ["owner": .string("bob")]), as: "alice")
        #expect(out["reason"].asString == "unauthorized")
    }

    @Test func missingIdTakesTheKernelsNext() async {
        let k = await store()
        let before = k.nextId
        let out = await append(k, "alice", [.object(["content": .string("x")])])
        #expect((out["ids"][0].asInt ?? 0) >= before)
    }

    @Test func operatorNeedsExplicitService() async {
        let k = await store()
        k.update(AgentId("mem"), ["operator": .bool(false)])
        let out = await call(k, p("read"))
        #expect(out["reason"].asString == "unauthorized")
        k.update(AgentId("mem"), ["operator": .bool(true)])
        _ = await call(k, p("append", ["rows": .array([row(7, ["kind": .string("call")])])]))
        let got = await call(k, p("read", ["kinds": .array([.string("call")])]))
        #expect(ids(got) == [7])
        // the operator's rows are its own key — alice sees none of them
        #expect((await call(k, p("read"), as: "alice"))["rows"] == .array([]))
    }

    @Test func adminMayNameAnOwner() async {
        let k = await store()
        _ = await append(k, "alice", [row(1, ["content": .string("a")])])
        let got = await call(k, p("read", ["owner": .string("alice")]))
        #expect(ids(got) == [1])
        let out = await call(
            k, p("append", ["owner": .string("alice"), "rows": .array([row(2)])]))
        #expect(out["ids"] == .array([.integer(2)]))
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

    @Test func shareReadAndZeroTolerance() async {
        let k = await store()
        _ = await append(k, "alice", [1, 2, 3].map { row($0, ["content": .string("a\($0)")]) })
        _ = await append(k, "bob", [row(50, ["content": .string("b")])])
        let g = await call(
            k,
            p("share", [
                "with": .string("bob"), "scope": .object(["head_until": .integer(2)]),
                "why": .string("context"),
            ]), as: "alice")
        #expect(g["created"].asBool == true && g["anchor"].asInt == 1, "\(g.serialize())")
        let got = await call(k, p("read", ["grant": g["grant"]]), as: "bob")
        #expect(ids(got) == [1, 2])
        // carol holds no grant
        var out = await call(k, p("read", ["grant": g["grant"]]), as: "carol")
        #expect(out["reason"].asString == "unauthorized")
        // bob cannot share alice's rows — not under his key
        out = await call(
            k, p("share", ["with": .string("carol"), "scope": .object(["row": .integer(1)])]),
            as: "bob")
        #expect(out["reason"].asString == "not_found")
        // an identical share is the same grant
        let again = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["head_until": .integer(2)])]),
            as: "alice")
        #expect(again["grant"] == g["grant"] && again["created"].asBool == false)
    }

    @Test func frozenStopsLiveFollows() async {
        let k = await store()
        _ = await append(k, "alice", [row(1), row(2)])
        let frozen = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["head_until": .integer(2)])]),
            as: "alice")
        let live = await call(
            k,
            p("share", [
                "with": .string("bob"), "scope": .object(["head_until": .integer(2)]),
                "mode": .string("live"),
            ]), as: "alice")
        _ = await append(k, "alice", [row(3)])
        let f = await call(k, p("read", ["grant": frozen["grant"]]), as: "bob")
        let lv = await call(k, p("read", ["grant": live["grant"]]), as: "bob")
        #expect(ids(f) == [1, 2])
        #expect(ids(lv) == [1, 2, 3])
    }

    @Test func foldScopeReachesMembers() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [
                row(1), row(2),
                row(3, ["kind": .string("fold"), "refs": .array([.integer(1), .integer(2)])]),
                row(4),
            ])
        let g = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["fold": .integer(3)])]),
            as: "alice")
        let got = await call(k, p("read", ["grant": g["grant"]]), as: "bob")
        #expect(ids(got) == [1, 2, 3])
        #expect(got["rows"][2]["refs"] == .array([.integer(1), .integer(2)]))
    }

    @Test func pruneCascadesToGrantsAndFoldRefs() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [row(1), row(2), row(3, ["kind": .string("fold"), "refs": .array([.integer(1), .integer(2)])])])
        let g = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["head_until": .integer(3)])]),
            as: "alice")
        let out = await call(k, p("prune", ["before_id": .integer(2)]), as: "alice")
        #expect(out == .object(["pruned": .integer(1), "grants_removed": .integer(1)]))
        let gone = await call(k, p("read", ["grant": g["grant"]]), as: "bob")
        #expect(gone["reason"].asString == "not_found")
        let fold = await call(k, p("read", ["ids": .array([.integer(3)])]), as: "alice")
        #expect(fold["rows"][0]["refs"] == .array([.integer(2)]))
        let listed = await call(k, p("grants", ["side": .string("given")]), as: "alice")
        #expect(listed["grants"] == .array([]))
    }

    @Test func unshareAndGrantsListing() async {
        let k = await store()
        _ = await append(k, "alice", [row(1)])
        let g = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["row": .integer(1)])]),
            as: "alice")
        let rec = await call(k, p("grants", ["side": .string("received")]), as: "bob")
        #expect((rec["grants"].asArray ?? []).map { $0["owner"] } == [.string("alice")])
        var out = await call(k, p("unshare", ["grant": g["grant"]]), as: "bob")
        #expect(out["reason"].asString == "unauthorized")
        out = await call(k, p("unshare", ["grant": g["grant"]]), as: "alice")
        #expect(out["unshared"].asBool == true)
        out = await call(k, p("read", ["grant": g["grant"]]), as: "bob")
        #expect(out["reason"].asString == "not_found")
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
        _ = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["fold": .integer(3)])]),
            as: "alice")
        // only admins
        #expect((await call(k, p("dump"), as: "alice"))["reason"].asString == "unauthorized")
        let tables = (await call(k, p("dump")))["tables"]
        _ = await call(k, p("prune", ["before_id": .integer(99)]), as: "alice")
        #expect((await call(k, p("read"), as: "alice"))["rows"] == .array([]))
        let out = await call(k, p("inject", ["tables": tables]))
        #expect(out == .object(["rows": .integer(3), "grants": .integer(1)]))
        #expect((await call(k, p("dump")))["tables"] == tables)
        // a broken FK is refused and nothing changes
        var bad = tables.asObject ?? [:]
        var g0 = tables["grants"][0].asObject ?? [:]
        g0["anchor"] = .integer(999)
        bad["grants"] = .array([.object(g0)])
        #expect((await call(k, p("inject", ["tables": .object(bad)])))["reason"].asString == "bad_arg")
        #expect((await call(k, p("dump")))["tables"] == tables)
    }

    @Test func remoteCallerIsKeyedByItsQualifiedAddress() async {
        let k = await store()
        let out = await KernelTaskLocals.withRemoteCaller(leg: AgentId("leg"), address: "g2|scout") {
            await call(k, p("append", ["rows": .array([row(1)])]), as: "leg")
        }
        #expect(out["ids"] == .array([.integer(1)]), "\(out.serialize())")
        var tables = (await call(k, p("dump")))["tables"]
        #expect((tables["rows"].asArray ?? []).map { $0["owner"] } == [.string("g2|scout")])
        // the leg's own writes stay the leg's
        _ = await call(k, p("append", ["rows": .array([row(2)])]), as: "leg")
        tables = (await call(k, p("dump")))["tables"]
        let owners = (tables["rows"].asArray ?? []).compactMap { $0["owner"].asString }.sorted()
        #expect(owners == ["g1|leg", "g2|scout"])
    }

    @Test func theReadmeIsPythonVerbatim() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent(
                "python/bundled_agents/memory/ram_store/src/ram_store/readme.md")
        #expect(RamStoreBundle().readme == (try String(contentsOf: url, encoding: .utf8)))
    }

    @Test func metaIsValidated() {
        let b = RamStoreBundle()
        #expect(b.validateMeta(["operator": .string("yes")], record: [:]) != nil)
        #expect(b.validateMeta(["admins": .array([.integer(1)])], record: [:]) != nil)
        #expect(b.validateMeta(["admins": .string("kernel")], record: [:]) == nil)
        #expect(b.validateMeta(["operator": .bool(true), "admins": .array([.string("kernel")])], record: [:]) == nil)
    }
}
