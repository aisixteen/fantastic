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

/// The same store `RamStoreTests` builds: root guid `g1` (or none), four bare
/// callers, the operator served and an admin.
private func store(guid: String? = "g1", operatorServed: Bool = true) async -> Kernel {
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
                "operator": .bool(operatorServed), "admins": .array([.string("kernel")]),
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

/// `inject` as the operator (an admin) with `tables`.
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
        for verb in ["append", "read", "share", "unshare", "grants", "prune", "inject", "dump"] {
            #expect((await call(k, p(verb), as: "alice")).serialize() == want, "\(verb)")
        }
    }

    @Test func theOperatorIsRefusedUntilServed() async {
        let k = await store(operatorServed: false)
        #expect(
            (await call(k, p("read"))).serialize()
                == #"{"error":"mem: the operator is not served here","reason":"unauthorized","#
                + #""hint":"update_agent id=mem operator=true to serve the operator (as 'kernel')","#
                + #""side":"store"}"#)
    }

    @Test func ownerRefusalsNameTheCallerAndTheOwner() async {
        let k = await store()
        #expect(
            (await call(
                k, p("append", ["owner": .string("bob"), "rows": .array([row(1)])]), as: "alice"))
                .serialize()
                == #"{"error":"append: 'alice' may act only on its own rows, not 'bob'","#
                + #""reason":"unauthorized","hint":"a caller writes and reads its OWN key; another "#
                + #"owner's rows are reached through a grant it gives (`share`), or by an agent "#
                + #"listed in mem.admins","side":"store"}"#)
        for blank: JSON in [.string("  "), .integer(5), .array([])] {
            #expect(
                (await call(k, p("prune", ["owner": blank, "before_id": .integer(9)]), as: "alice"))
                    .serialize()
                    == #"{"error":"prune: `owner` must be an agent id","reason":"bad_arg","arg":"owner"}"#,
                "\(blank.serialize())")
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

    // ── share (and its scope parser) ────────────────────────────────────

    @Test func shareArgumentRefusals() async {
        let k = await store()
        _ = await append(k, "alice", [row(5), row(6, ["kind": .string("turn")])])
        #expect(
            (await call(k, p("share", ["scope": .object(["row": .integer(5)])]), as: "alice"))
                .serialize()
                == #"{"error":"share: with required","reason":"missing_arg","arg":"with"}"#)
        #expect(
            (await call(
                k, p("share", ["with": .string("alice"), "scope": .object(["row": .integer(5)])]),
                as: "alice"))
                .serialize()
                == #"{"error":"share: you already read your own rows","reason":"bad_arg","arg":"with"}"#
        )
        #expect(
            (await call(
                k,
                p(
                    "share",
                    [
                        "with": .string("bob"), "scope": .object(["row": .integer(5)]),
                        "mode": .string("forever"),
                    ]), as: "alice")).serialize()
                == #"{"error":"share: mode must be one of ('frozen', 'live')","reason":"bad_arg","arg":"mode"}"#
        )
    }

    @Test func shareScopeRefusals() async {
        let k = await store()
        _ = await append(k, "alice", [row(5), row(6, ["kind": .string("turn")])])
        _ = await append(k, "bob", [row(50)])
        func share(_ scope: JSON?) async -> String {
            var extra: OrderedDictionary<String, JSON> = ["with": .string("bob")]
            if let scope { extra["scope"] = scope }
            return (await call(k, p("share", extra), as: "alice")).serialize()
        }
        let shape =
            #"{"error":"share: `scope` must be ONE of {head_until: <row>}, {range: {from, to}}, "#
            + #"{fold: <id>}, {row: <id>} — a descriptor, never a list of messages","#
            + #""reason":"bad_arg","arg":"scope"}"#
        #expect(await share(nil) == shape)
        #expect(await share(.array([.integer(5)])) == shape)
        #expect(await share(.object(["row": .integer(5), "fold": .integer(6)])) == shape)
        #expect(await share(.object(["rows": .integer(5)])) == shape)

        #expect(
            await share(.object(["range": .integer(5)]))
                == #"{"error":"share: range must be {from, to}","reason":"bad_arg","arg":"scope"}"#)
        let order =
            #"{"error":"share: range needs from <= to (row ids)","reason":"bad_arg","arg":"scope"}"#
        #expect(
            await share(.object(["range": .object(["from": .integer(9), "to": .integer(5)])]))
                == order)
        #expect(await share(.object(["range": .object(["from": .integer(5)])])) == order)
        #expect(
            await share(.object(["range": .object(["from": .integer(40), "to": .integer(60)])]))
                == #"{"error":"share: none of YOUR rows lie in 40..60","reason":"not_found","arg":"scope"}"#
        )

        for kind in ["row", "fold", "head_until"] {
            #expect(
                await share(.object([kind: .string("x")]))
                    == #"{"error":"share: "# + kind
                    + #" must be a row id","reason":"bad_arg","arg":"scope"}"#)
        }
        #expect(
            await share(.object(["head_until": .integer(3)]))
                == #"{"error":"share: you have no rows up to 3","reason":"not_found","arg":"scope"}"#
        )
        #expect(
            await share(.object(["row": .integer(50)]))
                == #"{"error":"share: row 50 is not one of YOUR rows — only rows you own can be shared","#
                + #""reason":"not_found","arg":"scope"}"#)
        #expect(
            await share(.object(["fold": .integer(6)]))
                == #"{"error":"share: row 6 is not a fold","reason":"bad_arg","arg":"scope"}"#)

        // A range is normalised to {from, to} and anchored on its LOWEST own row.
        let ok = await call(
            k,
            p(
                "share",
                [
                    "with": .string("bob"),
                    "scope": .object(["range": .object(["from": .string("4"), "to": .integer(6)])]),
                ]), as: "alice")
        #expect(Array(ok.asObject?.keys ?? []) == ["grant", "anchor", "as_of", "created"])
        #expect(
            ok["anchor"] == .integer(5) && ok["as_of"] == .integer(6)
                && ok["created"] == .bool(true))
        let given = await call(k, p("grants", ["side": .string("given")]), as: "alice")
        let g = given["grants"][0]
        #expect(
            Array(g.asObject?.keys ?? [])
                == [
                    "grant", "owner", "reader", "scope", "mode", "as_of", "until", "why",
                    "created_by", "ts",
                ])
        #expect(g["scope"].serialize() == #"{"range":{"from":4,"to":6}}"#)
        #expect(g["owner"] == .string("alice") && g["reader"] == .string("bob"))
    }

    // ── unshare / grants / prune ────────────────────────────────────────

    @Test func unshareGrantsAndPruneRefusals() async {
        let k = await store()
        _ = await append(k, "alice", [row(1)])
        let g = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["row": .integer(1)])]),
            as: "alice")
        let gid = g["grant"].asInt ?? -1

        #expect(
            (await call(k, p("unshare"), as: "alice")).serialize()
                == #"{"error":"unshare: grant required","reason":"missing_arg","arg":"grant"}"#)
        #expect(
            (await call(k, p("unshare", ["grant": .string("x")]), as: "alice")).serialize()
                == #"{"error":"unshare: grant required","reason":"missing_arg","arg":"grant"}"#)
        #expect(
            (await call(k, p("unshare", ["grant": .integer(999)]), as: "alice")).serialize()
                == #"{"error":"unshare: no grant 999","reason":"not_found","arg":"grant"}"#)
        #expect(
            (await call(k, p("unshare", ["grant": .integer(gid)]), as: "bob")).serialize()
                == #"{"error":"unshare: grant \#(gid) is not yours to revoke","reason":"unauthorized","side":"store"}"#
        )
        // the admin may revoke any grant
        #expect(
            (await call(k, p("unshare", ["grant": .integer(gid)]))).serialize()
                == #"{"unshared":true,"grant":\#(gid)}"#)

        for side: JSON in [.null, .string("both"), .integer(1)] {
            var extra: OrderedDictionary<String, JSON> = [:]
            if side != .null { extra["side"] = side }
            #expect(
                (await call(k, p("grants", extra), as: "alice")).serialize()
                    == #"{"error":"grants: side must be 'given' or 'received'","reason":"bad_arg","arg":"side"}"#
            )
        }

        let missing =
            #"{"error":"prune: before_id required","reason":"missing_arg","arg":"before_id"}"#
        #expect((await call(k, p("prune"), as: "alice")).serialize() == missing)
        #expect(
            (await call(
                k, p("prune", ["before_id": .string("x"), "ids": .string("1")]), as: "alice"))
                .serialize() == missing)
        // `ids` alone is enough
        #expect(
            (await call(k, p("prune", ["ids": .array([.string("1")])]), as: "alice")).serialize()
                == #"{"pruned":1,"grants_removed":0}"#)
    }

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
        #expect(out.serialize() == #"{"pruned":2,"grants_removed":0}"#)
        _ = await append(k, "alice", [row(4, ["kind": .array([.string("x")])])])
        let raised = await KernelTaskLocals.$currentSender.withValue(AgentId("alice")) {
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

    // ── read under a grant ──────────────────────────────────────────────

    @Test func readUnderAGrantRefusals() async {
        let k = await store()
        _ = await append(k, "alice", [row(1)])
        let g = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["row": .integer(1)])]),
            as: "alice")
        let gid = g["grant"].asInt ?? -1
        #expect(
            (await call(k, p("read", ["grant": .integer(777)]), as: "bob")).serialize()
                == #"{"error":"read: grant 777 does not exist — revoked (unshare) or its rows were pruned","#
                + #""reason":"not_found","arg":"grant"}"#)
        #expect(
            (await call(k, p("read", ["grant": .string("abc")]), as: "bob")).serialize()
                == #"{"error":"read: grant 'abc' does not exist — revoked (unshare) or its rows were pruned","#
                + #""reason":"not_found","arg":"grant"}"#)
        #expect(
            (await call(
                k, p("search", ["query": .string("x"), "grant": .integer(gid)]), as: "carol"))
                .serialize()
                == #"{"error":"search: grant \#(gid) was given to 'bob', not 'carol'","reason":"unauthorized","side":"store"}"#
        )
    }

    // ── inject / dump (admins only) ─────────────────────────────────────

    @Test func injectAndDumpAreAdminOnly() async {
        let k = await store()
        #expect(
            (await call(k, p("inject", ["tables": .object([:])]), as: "alice")).serialize()
                == #"{"error":"inject: 'alice' is not an admin of mem","reason":"unauthorized","side":"store"}"#
        )
        #expect(
            (await call(k, p("dump"), as: "bob")).serialize()
                == #"{"error":"dump: 'bob' is not an admin of mem","reason":"unauthorized","side":"store"}"#
        )
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

        func grants(_ g: JSON) -> JSON { .object(["rows": rows, "grants": .array([g])]) }
        #expect(
            await inject(
                k,
                grants(
                    .object([
                        "owner": .string("g1|alice"), "anchor": .integer(9), "grant": .integer(1),
                    ])))
                == bad("inject: a grant's anchor names no row"))
        #expect(
            await inject(k, grants(.integer(1))) == bad("inject: a grant's anchor names no row"))
        #expect(
            await inject(
                k,
                grants(
                    .object([
                        "owner": .string("g1|alice"), "anchor": .integer(1), "grant": .string("g"),
                    ])))
                == bad("inject: a grant needs an int `grant`"))

        #expect((await call(k, p("dump")))["tables"] == before, "a refused inject changed nothing")

        // A falsy `rows` is no rows: the store is replaced by an empty one.
        #expect(await inject(k, .object(["rows": .integer(0)])) == #"{"rows":0,"grants":0}"#)
        #expect(
            (await call(k, p("dump")))["tables"].serialize()
                == #"{"rows":[],"fold_refs":[],"grants":[]}"#)
    }

    /// The `loadTables` seam the verb uses, called directly: a dump of a store
    /// with a fold and a grant loads back to the same dump.
    @Test func loadTablesRoundTripsADump() async {
        let k = await store()
        _ = await append(
            k, "alice",
            [
                row(1), row(2),
                row(3, ["kind": .string("fold"), "refs": .array([.integer(1), .integer(2)])]),
            ])
        _ = await call(
            k, p("share", ["with": .string("bob"), "scope": .object(["fold": .integer(3)])]),
            as: "alice")
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
