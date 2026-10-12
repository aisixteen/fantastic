// A reply is a row, not a turn.
//
// A settled call's body is inlined only if it is small; otherwise it is held in
// RAM under its pid (never persisted) and the model is handed the digest + its
// origin line; `grep` finds the line. These tests are the RAM body, the cap, and
// grep. Twins of python `ai_core/tests/test_async_replies.py`, case for case.

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
        config: baseConfig { MockProvider() }, backendIds: [AI],
        contextWindow: 12000, meta: ["output_reserve": .integer(0)])
    let agent = kernel.agent(ids[AI]!)!
    return (kernel, agent, FoldFixture(kernel: kernel, agent: AI))
}

/// python `_store_reply`.
@discardableResult
private func storeReply(_ kernel: Kernel, _ agent: Agent, _ pid: Int64, _ body: String) -> Bool {
    FantasticAICore.storeReply(agent.id.value, kernel, pid: pid, body: body)
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
    @Test func theInlineCapIsAnEighthOfTheBudget() {
        for window: Int64 in [8000, 12000, 32000, 131072] {
            let rec = Agent(
                id: AgentId("x"), handlerModule: nil, parentId: nil,
                meta: ["context_window": .integer(window), "output_reserve": .integer(0)])
            #expect(AIContext.inlineCap(rec, modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == max(256, AIContext.budget(rec, modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) / 8))
        }
    }

    /// BODIES ARE NEVER PERSISTED: a large reply is kept in RAM under its pid
    /// for this incarnation, and after a restart only its origin line remains.
    @Test func aBodyIsHeldInRamForThisRunOnly() async throws {
        let (kernel, agent, _) = await boot()
        let body = (0..<800).map { "line \($0)\n" }.joined()
        #expect(storeReply(kernel, agent, 1001, body))
        #expect(heldBodies(agent.id.value, kernel)[1001] == body)
        #expect(
            !storeRows(kernel: kernel, id: agent.id).contains { ($0["content"].asString ?? "").contains("line 799") },
            "nothing of the body reached the store")
        AIMemory.forget(kernel)  // the process restarts
        #expect(heldBodies(agent.id.value, kernel)[1001] == nil)
    }

    // ── grep: find the line, don't page for it ───────────────────────────

    @Test func grepFindsALineInsideAStoredBodyWithoutPullingIt() async throws {
        let (kernel, agent, f) = await boot()
        let pid = kernel.mintId()
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
        let p1 = kernel.mintId()
        let p2 = kernel.mintId()
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
        let pid = kernel.mintId()
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
        let call = kernel.mintId()
        let pid = kernel.mintId()
        let reply = kernel.mintId()
        f.seed([
            .object([
                "role": .string("user"), "parent": .integer(call), "id": .integer(reply),
                "content": .string(
                    "[reply to \(pid) · notes read · \"read filler_1\" · ↑\(call)]\n(settled, 9 bytes — NOT inlined)"
                ),
            ])
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
        let pid = kernel.mintId()
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

    // `line.strip()[:width]` counts code points: a CRLF is ONE Character and TWO
    // code points, so a grapheme-counted width returns more than python returns.
    @Test func grepWidthCountsCodePoints() async throws {
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
