// ram_store: EVERY refusal body, byte for byte — key order included.
//
// The sibling `RamStoreTests` asserts behaviour and the refusal CODE; this file
// pins the whole wire body each refusal answers (error text, reason, the machine
// field after it, and their order), so a control-flow rewrite of the verb bodies
// (Result plumbing -> typed throws) cannot change a reply without failing here.
// Texts are python `ram_store/tools.py`'s.

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

/// The same store `RamStoreTests` builds: root guid `g1` (or none), three bare
/// callers.
private func store(guid: String? = "g1") async -> Kernel {
    let r = BundleRegistry()
    r.register("ram_store.tools", RamStoreBundle())
    let k = Kernel(storage: .inMemory, bundles: r)
    var meta: OrderedDictionary<String, JSON> = [:]
    if let guid { meta["guid"] = .string(guid) }
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil, meta: meta)
    k.register(root)
    k.setRoot(root)
    for aid in ["alice", "bob", "carol"] {
        let a = Agent(
            id: AgentId(aid), handlerModule: nil, parentId: root.id,
            meta: ["can_call": .object(["mem": .string("*")])])
        k.register(a)
        root.insertChild(a)
    }
    _ = await k.send([
        Item(
            "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "id": .string("mem"), "handler_module": .string("ram_store.tools"),
                "callable_by": .object([
                    "alice": .string("*"), "bob": .string("*"), "carol": .string("*"),
                ]),
            ]))
    ])[0].result
    return k
}

private func append(_ k: Kernel, _ who: String, _ rows: [JSON]) async -> JSON {
    await call(k, p("append", ["rows": .array(rows)]), as: who)
}

/// `inject` as the operator with `tables`.
private func inject(_ k: Kernel, _ tables: JSON) async -> String {
    await call(k, p("inject", ["tables": tables])).serialize()
}

private func dumpRow(_ owner: String, _ row: JSON) -> JSON {
    .object(["owner": .string(owner), "row": row])
}

@Suite("ram_store — refusal bodies, byte for byte")
struct RamStoreRefusalPinTests {

    // ── the caller ──────────────────────────────────────────────────────

    @Test func aKernelWithNoGuidRefusesEveryCaller() async {
        let k = await store(guid: nil)
        let want =
            #"{"error":"ram_store: this kernel has no guid yet — the store keys every row by it","#
            + #""reason":"not_found","hint":"boot the kernel root first (it mints the guid)"}"#
        for verb in ["append", "read", "prune"] {
            #expect((await call(k, p(verb), as: "alice")).serialize() == want, "\(verb)")
        }
    }

    @Test func anUnknownVerbIsUnhandledNamingIt() async {
        let k = await store()
        #expect(
            (await call(k, p("nope"), as: "alice")).serialize()
                == #"{"error":"ram_store: unknown type 'nope'","reason":"unhandled","verb":"nope"}"#
        )
    }

    // ── append ──────────────────────────────────────────────────────────

    @Test func appendRefusals() async {
        let k = await store()
        let notRows =
            #"{"error":"append: `rows` must be a non-empty list of rows","reason":"bad_arg","arg":"rows"}"#
        #expect((await call(k, p("append"), as: "alice")).serialize() == notRows)
        #expect((await append(k, "alice", [])).serialize() == notRows)
        #expect((await append(k, "alice", [.integer(1)])).serialize() == notRows)
        #expect(
            (await call(k, p("append", ["rows": .string("x")]), as: "alice")).serialize() == notRows
        )

        for (bad, shown) in [
            (JSON.string("5"), "'5'"), (.bool(true), "True"), (.integer(0), "0"),
            (.integer(-3), "-3"), (.double(1.5), "1.5"), (.array([]), "[]"),
        ] {
            #expect(
                (await append(k, "alice", [.object(["id": bad])])).serialize()
                    == #"{"error":"append: a row id is a kernel int, got "# + shown
                    + #"","reason":"bad_arg","arg":"rows"}"#)
        }

        #expect((await append(k, "alice", [row(1)]))["ids"] == .array([.integer(1)]))
        let exists =
            #"{"error":"append: row 1 already exists for this owner","reason":"exists","id":1}"#
        #expect((await append(k, "alice", [row(1)])).serialize() == exists)
        #expect(
            (await append(k, "alice", [row(2), row(2)])).serialize()
                == #"{"error":"append: row 2 already exists for this owner","reason":"exists","id":2}"#
        )

        for refs: JSON in [
            .string("x"), .array([.string("a")]), .array([.integer(0)]), .integer(1),
        ] {
            #expect(
                (await append(k, "alice", [row(7, ["refs": refs])])).serialize()
                    == #"{"error":"append: row 7 refs must be row ids","reason":"bad_arg","arg":"rows"}"#,
                "\(refs.serialize())")
        }
        #expect(
            (await append(k, "alice", [row(7, ["refs": .array([.integer(1), .integer(99)])])]))
                .serialize()
                == #"{"error":"append: row 7 refs rows that do not exist for this owner: [99]","#
                + #""reason":"not_found","arg":"rows"}"#)
        // A digit STRING passes the id check but names no int row: python's
        // `t.has(owner, "1")` misses too.
        #expect(
            (await append(k, "alice", [row(7, ["refs": .array([.string("1")])])])).serialize()
                == #"{"error":"append: row 7 refs rows that do not exist for this owner: ['1']","#
                + #""reason":"not_found","arg":"rows"}"#)
        // all or none: nothing above landed
        let read = await call(k, p("read"), as: "alice")
        #expect((read["rows"].asArray ?? []).map { $0["id"] } == [.integer(1)])
    }

    // ── prune ───────────────────────────────────────────────────────────

    /// python `row.get("kind") in keep` against a DICT keep: a key test for a
    /// string kind, never a match for None, and a TypeError for an unhashable
    /// kind — raised, nothing deleted.
    @Test func pruneKeepAgainstADict() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [row(1, ["kind": .string("fold")]), row(2, ["kind": .string("turn")]), row(3)])
        let out = await call(
            k, p("prune", ["before_id": .integer(10), "keep": .object(["fold": .integer(1)])]),
            as: "alice")
        #expect(out.serialize() == #"{"pruned":2}"#)
        _ = await append(k, "alice", [row(4, ["kind": .array([.string("x")])])])
        let raised = await KernelTaskLocals.$sender.withValue(AgentId("alice")) {
            await k.send([
                Item(
                    AgentId("mem"),
                    p("prune", ["before_id": .integer(10), "keep": .object(["fold": .integer(1)])]))
            ])[0]
        }
        #expect(raised.status == .failed)
        #expect(
            raised.result["error"].asString == "TypeError: unhashable type: 'list'",
            "\(raised.result.serialize())")
        let left = await call(k, p("read"), as: "alice")
        #expect((left["rows"].asArray ?? []).map { $0["id"] } == [.integer(1), .integer(4)])
    }

    @Test func injectRefusesEveryBrokenKeyAndChangesNothing() async {
        let k = await store()
        _ = await append(k, "alice", [row(1), row(2)])
        let before = (await call(k, p("dump")))["tables"]
        func bad(_ msg: String) -> String {
            #"{"error":""# + msg + #"","reason":"bad_arg","arg":"tables"}"#
        }

        #expect(await inject(k, .null) == bad("inject: `tables` must be a dump dict"))
        #expect(await inject(k, .array([])) == bad("inject: `tables` must be a dump dict"))
        #expect(
            await inject(k, .object(["rows": .string("x")]))
                == bad("inject: tables.rows must be a list"))
        #expect(
            await inject(k, .object(["rows": .object(["a": .integer(1)])]))
                == bad("inject: tables.rows must be a list"))
        #expect(
            await inject(k, .object(["rows": .array([.integer(5)])]))
                == bad("inject: every row needs {owner, row}"))
        #expect(
            await inject(
                k, .object(["rows": .array([.object(["owner": .integer(5), "row": row(1)])])]))
                == bad("inject: every row needs {owner, row}"))
        #expect(
            await inject(k, .object(["rows": .array([dumpRow("g1|alice", .object([:]))])]))
                == bad("inject: every row needs an int id"))
        #expect(
            await inject(
                k, .object(["rows": .array([dumpRow("g1|alice", .object(["id": .string("x")]))])]))
                == bad("inject: every row needs an int id"))
        #expect(
            await inject(k, .object(["rows": .array([dumpRow("g1|alice", .string("r"))])]))
                == bad("inject: every row needs an int id"))
        #expect(
            await inject(
                k,
                .object([
                    "rows": .array([dumpRow("g1|alice", row(3)), dumpRow("g1|alice", row(3))])
                ]))
                == #"{"error":"inject: duplicate row (g1|alice, 3)","reason":"exists","id":3}"#)

        let rows: JSON = .array([dumpRow("g1|alice", row(1)), dumpRow("g1|alice", row(2))])
        func refs(_ fold: JSON, _ members: [JSON]) -> JSON {
            .object([
                "rows": rows,
                "fold_refs": .array([
                    .object([
                        "owner": .string("g1|alice"), "fold": fold, "members": .array(members),
                    ])
                ]),
            ])
        }
        #expect(await inject(k, refs(.integer(99), [])) == bad("inject: fold_refs names no row 99"))
        #expect(
            await inject(k, refs(.string("x"), [])) == bad("inject: fold_refs names no row None"))
        #expect(
            await inject(k, refs(.integer(2), [.integer(1), .integer(42)]))
                == bad("inject: fold 2 refs a missing row"))
        #expect(
            await inject(k, refs(.integer(2), [.string("z")]))
                == bad("inject: fold 2 refs a missing row"))

        #expect((await call(k, p("dump")))["tables"] == before, "a refused inject changed nothing")

        // A falsy `rows` is no rows: the store is replaced by an empty one.
        #expect(await inject(k, .object(["rows": .integer(0)])) == #"{"rows":0}"#)
        #expect(
            (await call(k, p("dump")))["tables"].serialize()
                == #"{"rows":[],"fold_refs":[]}"#)
    }

    /// The `loadTables` seam the verb uses, called directly: a dump of a store
    /// with a fold loads back to the same dump.
    @Test func loadTablesRoundTripsADump() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [
                row(1), row(2),
                row(3, ["kind": .string("fold"), "refs": .array([.integer(1), .integer(2)])]),
            ])
        let tables = (await call(k, p("dump")))["tables"]
        guard case .success(let t) = loadTables(tables) else {
            Issue.record("a dump must load")
            return
        }
        #expect(t.dump() == tables)
        #expect(
            t.dump()["fold_refs"].serialize() == #"[{"owner":"g1|alice","fold":3,"members":[1,2]}]"#
        )
    }
}
