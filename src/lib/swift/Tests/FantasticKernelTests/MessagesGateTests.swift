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
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func send(_ k: Kernel, _ target: String, _ payload: JSON) async -> JSON {
    await k.send([Item(AgentId(target), payload)])[0].result
}

private func mk(_ k: Kernel, _ id: String, _ meta: [String: JSON] = [:]) async {
    var p: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"), "parent_id": .string("kernel_state"),
        "id": .string(id), "handler_module": .string("noop.tools"),
    ]
    for (kk, v) in meta { p[kk] = v }
    let r = await send(k, "kernel_state", .object(p))
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

private func keys(_ d: JSON) -> [String] {
    guard let o = d.asObject else { return [] }
    return Array(o.keys)
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
        await mk(k, "ai", ["can_call": .object(["kernel_state": .array([.string("messages")])])])
        await mk(k, "peer", ["can_call": .object(["kernel_state": .array([.string("messages")])])])
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
        _ = await send(k, "kernel_state", .object([
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

    /// C18: python `_as_id` — `0`, a negative int, a whitespace-padded digit
    /// string and a leading `+` all fail and refuse `messages` as a whole
    /// (any non-id in the list refuses the batch); a real minted id (even a
    /// Unicode-decimal-digit spelling) still resolves.
    @Test func messagesRefusesZeroNegativeAndPaddedIds() async {
        let k = bootKernel()
        await mk(k, "src")
        let real = put(k, "src", ["kind": "message", "content": "hi"])
        for bad: JSON in [
            .integer(0), .integer(-5), .string(" \(real)"), .string("\(real) "), .string("+\(real)"),
        ] {
            let r = await send(
                k, "kernel", .object(["type": .string("messages"), "ids": .array([bad])]))
            #expect(r["reason"].asString == "bad_arg", "\(bad): \(r.serialize())")
            #expect(r["arg"].asString == "ids")
        }
        // A Unicode decimal digit spelling of the SAME id resolves (`_as_id`
        // uses `str.isdigit()`, not an ASCII-only check).
        var fullwidthScalars = String.UnicodeScalarView()
        for u in String(real).unicodeScalars { fullwidthScalars.append(Unicode.Scalar(u.value + 0xFEE0)!) }
        let fullwidth = String(fullwidthScalars)
        let r = await send(
            k, "kernel", .object(["type": .string("messages"), "ids": .array([.string(fullwidth)])]))
        #expect(r["rows"].asArray?.first?["id"].asInt == real, "\(r.serialize())")
    }

    /// C18: `cancel task_id:"+<pid>"` is NOT the pid — a leading `+` fails
    /// `_as_id`'s digit-string test, and it names no execution either, so it
    /// is the same honest `{cancelled:false}` a stale id gets, never
    /// `Int64.init?`'s "+" acceptance sneaking it through.
    @Test func cancelWithALeadingPlusIsNeverThePid() async {
        let k = bootKernel()
        await mk(k, "w")
        let out = await k.send([Item(AgentId("w"), .object(["type": .string("ping")]))])[0]
        let pid = out.pid!.value
        let r = await send(
            k, "kernel", .object(["type": .string("cancel"), "task_id": .string("+\(pid)")]))
        #expect(r["cancelled"].asBool == false, "\(r.serialize())")
        #expect(r["killed"].asArray == [])
    }

    /// C63 — `head` is the first 120 CODE POINTS with `\n` → ` `, `bytes` is
    /// `len(str(content))` in code points. Not UTF-8 bytes and not graphemes:
    /// `\r\n` is one swift Character and two python characters, so a
    /// grapheme-counting head returned 240 scalars where python returns 120.
    @Test func digestHeadAndBytesCountUnicodeScalars() async {
        let k = bootKernel()
        let crlf = put(
            k, "ai",
            ["role": .string("user"), "content": .string(String(repeating: "\r\n", count: 100))])
        let cjk = put(k, "ai", ["role": .string("user"), "content": .string("日本語")])
        let flags = put(
            k, "ai",
            ["role": .string("user"), "content": .string(String(repeating: "🇺🇦", count: 10))])
        let r = await send(
            k, "kernel",
            .object([
                "type": .string("messages"),
                "ids": .array([.integer(crlf), .integer(cjk), .integer(flags)]),
            ]))
        let rows = r["rows"].asArray ?? []
        #expect(rows[0]["bytes"].asInt == 200, "\(rows[0].serialize())")
        #expect(
            rows[0]["head"].asString == String(repeating: "\r ", count: 60),
            "\(rows[0].serialize())")
        #expect(rows[0]["head"].asString?.unicodeScalars.count == 120)
        #expect(rows[1]["bytes"].asInt == 3, "code points, not the 9 UTF-8 bytes")
        #expect(rows[2]["bytes"].asInt == 20, "a flag is two scalars, one grapheme, eight bytes")
    }

    /// C64 — the digest's content is `str(content)`: an absent key is `""`, a
    /// number its digits, `None` the word, a container python's repr.
    @Test func nonStringContentDigestsLikePythonStr() async {
        let k = bootKernel()
        let cases: [(JSON, String, Int)] = [
            (.integer(5), "5", 1),
            (.null, "None", 4),
            (.array([.string("a"), .integer(1), .bool(true)]), "['a', 1, True]", 14),
            (.double(1.5), "1.5", 3),
            (.object(["k": .string("v"), "n": .null]), "{'k': 'v', 'n': None}", 21),
        ]
        var ids: [JSON] = []
        for (content, _, _) in cases {
            ids.append(.integer(put(k, "ai", ["role": .string("user"), "content": content])))
        }
        let absent = put(k, "ai", ["role": .string("user")])
        ids.append(.integer(absent))
        let r = await send(k, "kernel", .object(["type": .string("messages"), "ids": .array(ids)]))
        let rows = r["rows"].asArray ?? []
        for (i, (_, head, bytes)) in cases.enumerated() {
            #expect(
                rows[i]["head"].asString == head && rows[i]["bytes"].asInt == Int64(bytes),
                "\(rows[i].serialize())")
        }
        #expect(rows[5]["head"].asString == "" && rows[5]["bytes"].asInt == 0)
    }

    /// C65 — every optional key is decided by PYTHON TRUTHINESS and copied
    /// RAW; `refs` and a view's `ids` are `len()` over a list, a string or a
    /// dict; a view's `title` is `why`, assigned last so a truthy title keeps
    /// its earlier position with the new value.
    @Test func digestOptionalKeysFollowPythonTruthiness() async {
        let k = bootKernel()
        let strRefs = put(
            k, "ai",
            [
                "role": .string("user"), "content": .string("x"), "title": .integer(0),
                "level": .integer(0), "refs": .string("abc"),
            ])
        let rawTitle = put(
            k, "ai",
            [
                "role": .string("user"), "content": .string("y"), "title": .integer(7),
                "kind": .string(""),
            ])
        let falsies = put(
            k, "ai",
            [
                "role": .string("user"), "content": .string("q"), "title": .string(""),
                "level": .null, "refs": .array([]),
            ])
        let boolLevel = put(
            k, "ai",
            [
                "role": .string("user"), "content": .string("b"), "level": .bool(false),
                "title": .array([.integer(1)]),
            ])
        let dictRefs = put(
            k, "ai",
            [
                "role": .string("user"), "content": .string("d"),
                "refs": .object(["a": .integer(1), "b": .integer(2)]),
            ])
        let titledView = put(
            k, "ai",
            [
                "kind": .string("view"), "title": .string("t"), "why": .string("w"),
                "ids": .string("abc"), "budget": .integer(1),
            ])
        let plainView = put(
            k, "ai",
            [
                "kind": .string("view"), "title": .string(""), "why": .string("w"),
                "ids": .array([.integer(1), .integer(2)]), "budget": .integer(5),
            ])
        let r = await send(
            k, "kernel",
            .object([
                "type": .string("messages"),
                "ids": .array([
                    .integer(strRefs), .integer(rawTitle), .integer(falsies), .integer(boolLevel),
                    .integer(dictRefs), .integer(titledView), .integer(plainView),
                ]),
            ]))
        let rows = r["rows"].asArray ?? []
        #expect(
            rows[0]["refs"].asInt == 3 && rows[0]["title"].isNull && rows[0]["level"].isNull,
            "\(rows[0].serialize())")
        #expect(
            rows[1]["title"].asInt == 7 && rows[1]["kind"].asString == "user",
            "a raw int title; '' kind falls back to role")
        #expect(
            rows[2]["title"].isNull && rows[2]["level"].isNull && rows[2]["refs"].isNull,
            "\(rows[2].serialize())")
        #expect(
            rows[3]["level"].isNull && rows[3]["title"] == .array([.integer(1)]),
            "\(rows[3].serialize())")
        #expect(rows[4]["refs"].asInt == 2, "a dict's len is its key count")
        // KEY ORDER: a truthy title sits before `ids`, and takes `why` as its value.
        #expect(
            keys(rows[5]) == [
                "id", "agent", "parent", "kind", "role", "head", "bytes", "title", "ids", "budget",
            ], "\(rows[5].serialize())")
        #expect(rows[5]["title"].asString == "w" && rows[5]["ids"].asInt == 3)
        #expect(
            keys(rows[6]) == [
                "id", "agent", "parent", "kind", "role", "head", "bytes", "ids", "budget", "title",
            ], "\(rows[6].serialize())")
        #expect(
            rows[6]["title"].asString == "w" && rows[6]["ids"].asInt == 2
                && rows[6]["budget"].asInt == 5)
    }

    /// C66 — a call/task digest's `status` is the SETTLE's, read off the nested
    /// `outcome` stamp: no stamp is no key, an empty one is no key, a stamp
    /// without a status is a null one, and a top-level `status` is not a status.
    @Test func emptyOutcomeAddsNoStatus() async {
        let k = bootKernel()
        let none = put(k, "ai", ["kind": .string("call"), "verb": .string("v")])
        let empty = put(
            k, "ai", ["kind": .string("call"), "verb": .string("v"), "outcome": .object([:])])
        let noStatus = put(
            k, "ai",
            ["kind": .string("call"), "verb": .string("v"), "outcome": .object(["x": .integer(1)])])
        let shadowed = put(
            k, "ai",
            [
                "kind": .string("call"), "verb": .string("x"), "status": .string("completed"),
                "outcome": .object(["status": .string("failed")]), "name": .string(""),
            ])
        let named = put(
            k, "ai",
            [
                "kind": .string("task"), "verb": .string("v"), "name": .integer(5),
                "outcome": .object(["status": .string("completed")]),
            ])
        let r = await send(
            k, "kernel",
            .object([
                "type": .string("messages"),
                "ids": .array([
                    .integer(none), .integer(empty), .integer(noStatus), .integer(shadowed),
                    .integer(named),
                ]),
            ]))
        let rows = r["rows"].asArray ?? []
        #expect(
            rows[0].asObject?["status"] == nil && rows[1].asObject?["status"] == nil,
            "\(rows[1].serialize())")
        #expect(rows[2].asObject?["status"] == JSON.null, "a stamp with no status IS a null status")
        #expect(
            rows[3]["status"].asString == "failed" && rows[3].asObject?["name"] == nil,
            "\(rows[3].serialize())")
        #expect(rows[4]["name"].asInt == 5 && rows[4]["status"].asString == "completed")
    }

    /// C67 — `limit` is `int(x or 0)` with python's own `except (TypeError,
    /// ValueError): 0`. A digit string of any script, a padded or signed one and
    /// a float all coerce; a falsy value, an unparseable string and a container
    /// are 0 (no cap). A NEGATIVE limit is truthy: `messages` keeps nothing,
    /// `rows_read` drops that many from the END.
    @Test func limitCoercesLikePythonInt() async {
        let k = bootKernel()
        let all = (0..<8).map { i in
            put(k, "ai", ["role": .string("user"), "content": .string("message \(i)")])
        }
        let idsJ: JSON = .array(all.map { .integer($0) })
        let twos: [JSON] = [
            .double(2.5), .string(" 2"), .string("2"), .double(2.0), .string("+2"),
            .string("\u{0662}"),
        ]
        for lim in twos {
            let r = await send(
                k, "kernel", .object(["type": .string("messages"), "ids": idsJ, "limit": lim]))
            #expect(
                ids(r) == Array(all[..<2]) && r["next"].asInt == all[1],
                "limit \(lim.serialize()) → \(r.serialize())")
        }
        let one = await send(
            k, "kernel", .object(["type": .string("messages"), "ids": idsJ, "limit": .bool(true)]))
        #expect(ids(one) == [all[0]], "True is 1")
        for lim: JSON in [
            .string("2.5"), .string("0"), .bool(false), .array([.integer(1)]), .object([:]), .null,
        ] {
            let r = await send(
                k, "kernel", .object(["type": .string("messages"), "ids": idsJ, "limit": lim]))
            #expect(
                ids(r).count == 8 && r["next"].isNull, "limit \(lim.serialize()) → \(r.serialize())"
            )
        }
        let ten = await send(
            k, "kernel",
            .object(["type": .string("messages"), "ids": idsJ, "limit": .string("1_0")]))
        #expect(ids(ten).count == 8, "python reads `1_0` as 10")
        let neg = await send(
            k, "kernel", .object(["type": .string("messages"), "ids": idsJ, "limit": .integer(-1)]))
        #expect(ids(neg).isEmpty && neg["next"].isNull, "\(neg.serialize())")
        // rows_read: a python slice, so a negative limit truncates from the end.
        let back = await send(
            k, "kernel_state",
            .object(["type": .string("rows_read"), "agent": .string("ai"), "limit": .integer(-3)]))
        #expect(
            ids(back) == Array(all[..<5]) && back["next"].asInt == all[4], "\(back.serialize())")
        let str2 = await send(
            k, "kernel_state",
            .object(["type": .string("rows_read"), "agent": .string("ai"), "limit": .string("2")]))
        #expect(
            ids(str2) == Array(all[..<2]) && str2["next"].asInt == all[1], "\(str2.serialize())")
        let wipe = await send(
            k, "kernel_state",
            .object(["type": .string("rows_read"), "agent": .string("ai"), "limit": .integer(-20)]))
        #expect(
            ids(wipe).isEmpty && wipe["next"].isNull, "a slice longer than the list keeps nothing")
        // A magnitude python holds and `Int` cannot keeps its MEANING: a huge
        // positive limit caps nothing, a huge negative one keeps nothing.
        let huge = await send(
            k, "kernel",
            .object(["type": .string("messages"), "ids": idsJ, "limit": .double(1e30)]))
        #expect(ids(huge).count == 8 && huge["next"].isNull, "\(huge.serialize())")
        let hugeNeg = await send(
            k, "kernel_state",
            .object([
                "type": .string("rows_read"), "agent": .string("ai"), "limit": .double(-1e30),
            ]))
        #expect(ids(hugeNeg).isEmpty && hugeNeg["next"].isNull, "\(hugeNeg.serialize())")
    }

    /// C68 — the ~32 KB cap measures `len(json.dumps(d))`: python separators,
    /// `ensure_ascii` escapes and CODE POINTS. A compact UTF-8 byte count pages
    /// somewhere else entirely — 158/73/57 rows instead of 149/40/21.
    @Test func digestCapCountsPythonJsonDumpsLength() async {
        for (content, expected) in [
            (String(repeating: "x", count: 300), 149),
            (String(repeating: "日本語", count: 100), 40),
            (String(repeating: "😀", count: 150), 21),
        ] {
            let k = bootKernel()
            let all = (0..<300).map { _ in
                put(k, "ai", ["role": .string("user"), "content": .string(content)])
            }
            #expect(all.first == 1, "the ids this pin is computed against")
            let r = await send(
                k, "kernel",
                .object(["type": .string("messages"), "ids": .array(all.map { .integer($0) })]))
            #expect(ids(r).count == expected, "kept \(ids(r).count), expected \(expected)")
            #expect(r["next"].asInt == all[expected - 1], "\(r["next"].serialize())")
        }
    }

    /// C69 (python 2026-09-20) — a digest COUNTS: an unsized `refs`/`ids` is
    /// `1`, a non-dict `outcome` is no status. These used to raise off the wire.
    @Test func digestCountsUnsizedRefsAndNonObjectOutcomeLikePython() async {
        let k = bootKernel()
        let refsInt = put(
            k, "ai", ["role": .string("user"), "content": .string("c"), "refs": .integer(5)])
        let viewIds = put(
            k, "ai",
            [
                "kind": .string("view"), "why": .string("w"), "ids": .integer(5),
                "budget": .integer(1),
            ])
        let strOutcome = put(
            k, "ai", ["kind": .string("call"), "verb": .string("v"), "outcome": .string("x")])
        let ok = put(k, "ai", ["role": .string("user"), "content": .string("a")])
        let cases: [(Int64, String, JSON)] = [
            (refsInt, "refs", .integer(1)),
            (viewIds, "ids", .integer(1)),
            (strOutcome, "status", .null),
        ]
        for (rid, key, want) in cases {
            let r = await send(
                k, "kernel", .object(["type": .string("messages"), "ids": .array([.integer(rid)])]))
            #expect(r["error"] == nil, "\(r.serialize())")
            #expect(r["rows"].asArray?.first?[key] == want, "\(r.serialize())")
        }
        // `'²'.isdecimal()` is false (python `_as_id`, 2026-09-20): not an id,
        // so the list is refused as a whole — where the wider `isdigit` guard
        // let it through to raise inside `int()`.
        let sup = await send(
            k, "kernel",
            .object(["type": .string("messages"), "ids": .array([.integer(ok), .string("²")])]))
        #expect(
            sup["error"].asString == "messages: `ids` must be a non-empty list of message ids (ints)"
                && sup["reason"].asString == "bad_arg", "\(sup.serialize())")
        // …and the digit strings python DOES take still read as ids.
        let padded = await send(
            k, "kernel",
            .object(["type": .string("messages"), "ids": .array([.string("000\(ok)")])]))
        #expect(ids(padded) == [ok], "\(padded.serialize())")
        let zero = await send(
            k, "kernel", .object(["type": .string("messages"), "ids": .array([.string("0")])]))
        #expect(ids(zero).isEmpty && refused(zero) == [0], "`'0'` is an id, and no row holds it")
    }

    // ── rows_read / rows_write: hydration's pair, operator-only ──────────

    @Test func rowsReadReturnsWholeRowsOldestFirstAndPages() async {
        let k = bootKernel()
        let all = (0..<4).map { i in put(k, "ai", ["role": .string("user"), "content": .string("message \(i)")]) }
        let got = await send(k, "kernel_state", .object(["type": .string("rows_read"), "agent": .string("ai")]))
        #expect(ids(got) == all)
        #expect(got["rows"].asArray?[0]["content"].asString == "message 0", "WHOLE, not a digest")
        #expect(got["next"].isNull)
        let page = await send(k, "kernel_state", .object(["type": .string("rows_read"), "agent": .string("ai"), "limit": .integer(2)]))
        #expect(ids(page) == Array(all[..<2]) && page["next"].asInt == all[1])
        let rest = await send(k, "kernel_state", .object(["type": .string("rows_read"), "agent": .string("ai"), "after": page["next"]]))
        #expect(ids(rest) == Array(all[2...]))
        let none = await send(k, "kernel_state", .object(["type": .string("rows_read"), "agent": .string("nobody")]))
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
        let out = await send(k, "kernel_state", .object(["type": .string("rows_write"), "agent": .string("ai_fork"), "rows": .array(rows)]))
        let outIds = (out["ids"].asArray ?? []).compactMap { $0.asInt }
        #expect(outIds.count == 1 && outIds[0] != src, "a copy is a NEW row: \(out.serialize())")
        let copied = k.rows.forAgent(AgentId("ai_fork"))
        #expect(copied[0]["content"]?.asString == "the original" && copied[0]["agent"]?.asString == "ai_fork")
        #expect(k.rows.forAgent(AgentId("ai")).map { $0["id"]?.asInt } == [src], "the original is untouched")
    }

    @Test func rowsWriteRefusesARowThatCarriesItsOwnId() async {
        let k = bootKernel()
        let r = await send(k, "kernel_state", .object([
            "type": .string("rows_write"), "agent": .string("ai"),
            "rows": .array([.object(["id": .integer(7), "content": .string("x")])]),
        ]))
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "rows")
        #expect((r["error"].asString ?? "").contains("may not carry an `id`"))
    }

    /// C90 — a refused put is the ANSWER, never a minted id. `rowsWriteReply`
    /// swallowed the throw with `?? 0`, so a caller was handed row `0` — a
    /// number that names no row — and could not tell the write had failed. Two
    /// halves: the verb refuses before it writes anything, and the store itself
    /// RAISES rather than answering an id (python's `rows_put`).
    @Test func aRefusedRowWriteNeverReportsIdZero() async {
        let k = bootKernel()
        let r = await send(
            k, "kernel_state",
            .object([
                "type": .string("rows_write"), "agent": .string("ai"),
                "rows": .array([.object(["id": .integer(7), "content": .string("x")])]),
            ]))
        #expect(r["ids"].isNull, "a refusal carries no ids: \(r.serialize())")
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        var raised = false
        do {
            _ = try k.rows.put(AgentId("ai"), ["id": .integer(99), "content": .string("x")])
        } catch {
            raised = true
        }
        #expect(raised, "rows.put answered an id for a bad one instead of refusing")
        #expect(k.rows.forAgent(AgentId("ai")).isEmpty, "nothing was written")
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
            let r = await send(k, "kernel_state", payload)
            #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == arg, "\(payload.serialize()) → \(r.serialize())")
        }
    }

    @Test func aWildcardNeverCarriesThePairButANamedGrantDoes() async {
        let k = bootKernel()
        _ = put(k, "ai", ["role": .string("user"), "content": .string("private")])
        // Wide open BOTH ways, and it still refuses: `*` is not a yes to these.
        await mk(k, "peer", ["can_call": .object(["kernel_state": .string("*")])])
        _ = await send(k, "kernel_state", .object([
            "type": .string("update_agent"), "id": .string("kernel_state"),
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
        _ = await send(k, "kernel_state", .object([
            "type": .string("update_agent"), "id": .string("peer"),
            "can_call": .object(["kernel_state": .array([.string("rows_read")])]),
        ]))
        _ = await send(k, "kernel_state", .object([
            "type": .string("update_agent"), "id": .string("kernel_state"),
            "callable_by": .object(["peer": .array([.string("rows_read")])]),
        ]))
        let r = await asSender("peer") {
            await send(k, "kernel", .object(["type": .string("rows_read"), "agent": .string("ai")]))
        }
        #expect((r["rows"].asArray ?? []).map { $0["content"].asString } == ["private"], "\(r.serialize())")
    }
}
