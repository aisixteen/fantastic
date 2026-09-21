// FULL ASYNC: a reply is a row, not a turn.
//
// A settled call's body is inlined only if it is small; otherwise it is stored
// under its pid and the model is handed the digest. Then a 240KB ledger dump is
// a row to page, never a turn that has to fit — which is the difference between
// "the model can look" and "the chain is dead". These tests are the store, the
// cap, and the paging. Twins of python `ai_core/tests/test_async_replies.py`,
// case for case.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// One agent id per suite: suites run in parallel and other suites boot their
/// own backends.
private let AI = "ai_ar"

/// python `_Leg()`: a store under an agent whose window is 12000 with no output
/// reserve.
private func boot() async -> (Kernel, Agent, FoldFixture) {
    let (kernel, ids, _) = await bootMockKernel(
        config: baseConfig { .provider(MockProvider()) }, backendIds: [AI],
        contextWindow: 12000, meta: ["output_reserve": .integer(0)])
    let agent = kernel.agent(ids[AI]!)!
    return (kernel, agent, FoldFixture(kernel: kernel, agent: AI))
}

/// python `_store_reply`.
@discardableResult
private func storeReply(_ kernel: Kernel, _ agent: Agent, _ pid: Int64, _ body: String) -> Bool {
    (try? kernel.rows.bodyPut(agent.id, pid: pid, text: body)) != nil
}

private func verb(
    _ kernel: Kernel, _ agent: Agent, _ type: String, _ args: OrderedDictionary<String, JSON> = [:]
)
    async -> JSON
{
    var payload: OrderedDictionary<String, JSON> = ["type": .string(type)]
    for (k, v) in args { payload[k] = v }
    return await kernel.send([Item(agent.id, .object(payload))])[0].result
}

private func asUserTurn(_ r: JSON) -> JSON {
    .object(["role": .string("user"), "content": .string(r.serialize())])
}

@Suite("async replies: a reply is a row", .serialized)
struct AsyncRepliesTests {
    /// THE INVARIANT that closes the recursion. If a page could be larger than
    /// what inlines, the page itself would be stored and digested, and the model
    /// would pull the pull forever (measured in review before it reached a live
    /// run).
    @Test func aPullPageCanNeverExceedTheInlineCap() {
        for window: Int64 in [8000, 12000, 32000, 131072] {
            let rec = Agent(
                id: AgentId("x"), handlerModule: nil, parentId: nil,
                meta: ["context_window": .integer(window), "output_reserve": .integer(0)])
            #expect(AIContext.inlineCap(rec) == max(256, AIContext.budget(rec) / 8))
            #expect(
                Int64(AIContext.pullCapChars(rec) / AIContext.charsPerToken)
                    <= AIContext.inlineCap(rec))
        }
    }

    @Test func storeThenPullPagesThroughTheBody() async throws {
        let (kernel, agent, _) = await boot()
        let body = (0..<8000).map { "line \($0)\n" }.joined()  // ~70KB, cap is 6KB chars
        let pid = kernel.rows.mintId()
        #expect(storeReply(kernel, agent, pid, body))
        let cap = AIContext.pullCapChars(agent)
        var got = ""
        var off: Int64 = 0
        var pages = 0
        while true {
            let r = await verb(
                kernel, agent, "pull", ["pid": .integer(pid), "offset": .integer(off)])
            #expect(r["error"] == .null, Comment(rawValue: r.serialize()))
            let text = r["text"].asString ?? ""
            #expect(text.count <= cap)
            got += text
            pages += 1
            if r["eof"].asBool == true { break }
            off = r["next_offset"].asInt ?? 0
            if pages > 100 {
                Issue.record("no eof after 100 pages")
                break
            }
        }
        #expect(got == body)
        #expect(pages > 1, "the body should have needed more than one page")
    }

    @Test func aPullCanNeverReturnMoreThanTheCap() async throws {
        let (kernel, agent, _) = await boot()
        let pid = kernel.rows.mintId()
        storeReply(kernel, agent, pid, String(repeating: "z", count: 100_000))
        let cap = AIContext.pullCapChars(agent)
        var r = await verb(
            kernel, agent, "pull", ["pid": .integer(pid), "length": .integer(Int64(cap + 1))])
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "length")
        r = await verb(kernel, agent, "pull", ["pid": .integer(pid)])
        // a page is sized so the SERIALIZED reply fits inline — never more than
        // the cap in chars, and shorter when escaping would push the reply over
        let n = r["text"].asString?.count ?? 0
        #expect(64 < n && n <= cap && r["eof"].asBool == false)
        #expect(AIContext.estimateOne(asUserTurn(r)) <= AIContext.inlineCap(agent))
    }

    @Test func pullingAnUnknownPidIsAReadableRefusal() async throws {
        let (kernel, agent, _) = await boot()
        var r = await verb(kernel, agent, "pull", ["pid": .integer(999_999)])
        #expect(r["reason"].asString == "not_found")
        r = await verb(kernel, agent, "pull")
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "pid")
    }

    @Test func noFileLegMeansTheBodyCannotBeStored() {
        #expect(Kernel().rows.bodyGet(1, offset: 0, length: 10) == nil)
    }

    // ── grep: find the line, don't page for it ───────────────────────────

    @Test func grepFindsALineInsideAStoredBodyWithoutPullingIt() async throws {
        let (kernel, agent, f) = await boot()
        let pid = kernel.rows.mintId()
        let fox = String(repeating: "the quick brown fox ", count: 40)
        let body =
            (["chunk 5: " + fox, "chunk 5: " + fox, "chunk 5: " + fox] + [
                ">> audit marker: a1b2c3d4"
            ] + [fox, fox, fox])
            .joined(separator: "\n")
        storeReply(kernel, agent, pid, body)
        f.seed([
            f.row("user", "survey the corpus", "user:0001"),
            f.row("assistant", "Reading filler_5.", "ai:0002", "user:0001"),
            f.row(
                "user",
                "[reply to \(pid) · notes read · \"survey filler_5\"]\n(settled, 9000 bytes — NOT inlined. Pull a page: …)",
                "ai:0003"),
        ])
        let r = await verb(kernel, agent, "grep", ["query": .string("AUDIT MARKER")])
        #expect(r["total"].asInt == 1 && r["truncated"].asBool == false)
        let m = r["matches"][0]
        #expect(
            m["id"].asInt == pid && m["line"].asInt == 4
                && (m["text"].asString ?? "").contains("a1b2c3d4"))
    }

    @Test func grepScopesToIdsAndCapsMatches() async throws {
        let (kernel, agent, f) = await boot()
        let p1 = kernel.rows.mintId()
        let p2 = kernel.rows.mintId()
        storeReply(kernel, agent, p1, (0..<50).map { "hit \($0)" }.joined(separator: "\n"))
        storeReply(kernel, agent, p2, "hit elsewhere")
        f.seed([f.row("user", "[reply to \(p1) · n r]\nx\n\n[reply to \(p2) · n r]\ny", "u:0")])
        let r = await verb(
            kernel, agent, "grep",
            ["query": .string("hit"), "ids": .array([.integer(p1)]), "limit": .integer(5)])
        let matches = r["matches"].asArray ?? []
        #expect(r["total"].asInt == 50 && matches.count == 5 && r["truncated"].asBool == true)
        #expect(matches.allSatisfy { $0["id"].asInt == p1 })
    }

    @Test func grepRefusesWithoutAQuery() async throws {
        let (kernel, agent, _) = await boot()
        let r = await verb(kernel, agent, "grep")
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "query")
    }

    /// Measured 2026-09-06 (python): the task line repeated per delivery matched
    /// 28 times and pushed the one body hit past the cap. Bodies come first;
    /// identical lines once.
    @Test func grepPutsTheStoredBodyHitFirstAndCollapsesRepeatedLines() async throws {
        let (kernel, agent, f) = await boot()
        for _ in 0..<6 {
            f.seed([plainTurn("user", "find the audit marker in the files")])
        }
        let pid = kernel.rows.mintId()
        f.seed([
            plainTurn("user", "[reply to \(pid) · notes read]\n(settled, 9 bytes — NOT inlined)")
        ])
        storeReply(kernel, agent, pid, "chunk 1\n>> audit marker: deadbeef\nchunk 1")
        let r = await verb(kernel, agent, "grep", ["query": .string("audit marker")])
        let first = r["matches"][0]
        #expect(first["id"].asInt == pid && (first["text"].asString ?? "").contains("deadbeef"))
        #expect(r["total"].asInt == 2, Comment(rawValue: r.serialize()))
    }

    /// A descent: `ids:[fold]` must reach the bodies behind the member replies.
    @Test func grepScopedToAFoldSearchesItsMembersBodies() async throws {
        let (kernel, agent, f) = await boot()
        let call = kernel.rows.mintId()
        let pid = kernel.rows.mintId()
        let reply = try kernel.rows.put(
            agent.id,
            [
                "role": .string("user"), "parent": .integer(call),
                "content": .string(
                    "[reply to \(pid) · notes read · \"read filler_1\" · ↑\(call)]\n(settled, 9 bytes — NOT inlined)"
                ),
            ])
        storeReply(kernel, agent, pid, "chunk 1\n>> audit marker: cafebabe\nchunk 1")
        let fold = f.fold([call, reply], "read filler_1", "plain filler", false)
        f.seed([fold])
        let r = await verb(
            kernel, agent, "grep", ["query": .string("audit marker"), "ids": .array([fold["id"]])])
        #expect(
            r["total"].asInt == 1 && r["matches"][0]["id"].asInt == pid
                && (r["matches"][0]["text"].asString ?? "").contains("cafebabe"),
            Comment(rawValue: r.serialize()))
        let r2 = await verb(
            kernel, agent, "grep",
            ["query": .string("audit marker"), "ids": .array([.integer(pid)])])
        #expect(r2["total"].asInt == 1, "an id that is itself a pid still reaches its body")
    }

    @Test func grepWidthIsTheModelsToRaise() async throws {
        let (kernel, agent, f) = await boot()
        let pid = kernel.rows.mintId()
        f.seed([
            plainTurn("user", "[reply to \(pid) · notes read]\n(settled, 9 bytes — NOT inlined)")
        ])
        storeReply(
            kernel, agent, pid, "rule: " + String(repeating: "x", count: 300) + " VALUE=cafebabe")
        var r = await verb(kernel, agent, "grep", ["query": .string("rule:")])
        var text = r["matches"][0]["text"].asString ?? ""
        #expect(text.count == 200 && !text.contains("cafebabe"))
        r = await verb(kernel, agent, "grep", ["query": .string("rule:"), "width": .integer(400)])
        text = r["matches"][0]["text"].asString ?? ""
        #expect(text.contains("cafebabe"))
    }

    // ── code points, not graphemes (B5.1) ────────────────────────────────

    // A CRLF is ONE Character and TWO code points, so a grapheme-counted page
    // and a grapheme-counted width both return more than python returns.
    //
    // The STORED-BODY branch's page (`kernel.rows.bodyGet`) counts code points
    // too as of C70 — pinned separately in
    // `ViewRecallTests.pullPagesACrlfBodyByCodePoints`. What is pinned here is
    // the FOLD branch and the grep width.
    @Test func pullAndGrepWidthCountCodePoints() async throws {
        let (kernel, agent, f) = await boot()
        let fold = f.fold([], "t", String(repeating: "a\r\nb", count: 3), false)
        f.seed([fold])
        let pid = try #require(rowId(fold))
        let r = await verb(kernel, agent, "pull", ["pid": .integer(pid), "length": .integer(5)])
        #expect(r["text"].asString == "a\r\nba", "\(r.serialize())")
        #expect(r["next_offset"].asInt == 5)

        // `line.strip()[:width]` counts code points too.
        let (k2, a2, f2) = await boot()
        f2.seed([f2.row("user", String(repeating: "a\r\n", count: 8) + "needle", "u:1")])
        let g = await verb(k2, a2, "grep", ["query": .string("needle"), "width": .integer(20)])
        #expect(g["total"].asInt == 1, "\(g.serialize())")
    }

    // `Character` `contains` cannot find `e` inside `é` and DOES find text
    // python calls different (NFC vs NFD). A grep answers on code points.
    @Test func grepMatchesCodePointsNotGraphemes() async throws {
        let (kernel, agent, f) = await boot()
        // `é` written NFD is `e` + U+0301: python's substring test finds `e`.
        f.seed([f.row("user", "caf\u{65}\u{301} time", "u:1")])
        let hit = await verb(kernel, agent, "grep", ["query": .string("cafe")])
        #expect(hit["total"].asInt == 1, "\(hit.serialize())")
        // And the NFC form is NOT the NFD form.
        let (k2, a2, f2) = await boot()
        f2.seed([f2.row("user", "caf\u{e9} time", "u:1")])
        let miss = await verb(k2, a2, "grep", ["query": .string("caf\u{65}\u{301}")])
        #expect(miss["total"].asInt == 0, "\(miss.serialize())")
    }
}
