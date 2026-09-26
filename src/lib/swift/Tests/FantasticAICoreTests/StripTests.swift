// Deterministic, no-LLM tests for `strip` — the ledger at mixed resolution.
// Twins of python `ai_core/tests/test_strip.py`, case for case.
//
// The claims: reply BODIES are stubbed oldest-first in one batch (hysteresis),
// the model's own turns are never touched, a stub carries the call's title, the
// set is monotone so the rendered prefix is byte-stable between strip events,
// and the cut is only the second stage.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

private let BIG = String(repeating: "x", count: 6000)

/// One agent id per suite: the stripped set is process-global, keyed by agent,
/// and suites run in parallel.
private let AI = "ai_s"

private func ctx(_ bud: Int64, _ selfId: String = AI) -> ProjectionCtx {
    ProjectionCtx(budget: bud, recentN: 4, summarize: nil, selfId: AgentId(selfId), kernel: nil)
}

private func turn(_ role: String, _ content: String) -> JSON { plainTurn(role, content) }

private func reply(_ pid: String, _ title: String?, body: String = BIG, verb: String = "read") -> JSON {
    let t = title.map { "[reply to \(pid) · notes \(verb) · \"\($0)\"]" } ?? "[reply to \(pid) · notes \(verb)]"
    return turn("user", "\(t)\n\(body)")
}

private func run(_ n: Int = 8, live: String = "write the report") -> [JSON] {
    var body = [turn("user", "GOAL: survey then report")]
    for i in 0..<n {
        body.append(
            turn(
                "assistant",
                "Reading filler_\(i). <call name=\"send\"><target_id>notes</target_id></call>"))
        body.append(reply("ai:" + String(format: "%04x", i), "survey filler_\(i)"))
        body.append(turn("assistant", "filler_\(i) was padding."))
    }
    body.append(turn("user", live))
    return body
}

private func strip(_ body: [JSON], _ c: ProjectionCtx) async throws -> AIProjection {
    try await AIStrategies.getStrategy("strip")!(body, [], .object([:]), c)
}

private func contents(_ rows: [JSON]) -> [String] { rows.map { $0["content"].asString ?? "" } }

@Suite("strip: bodies go, titles stay", .serialized)
struct StripTests {
    init() {
        AIStripStrategy.forget(AI)
        AIStripStrategy.forget(AI + "_a")
        AIStripStrategy.forget(AI + "_b")
    }

    @Test func underBudgetNothingIsStripped() async throws {
        let body = run(2)
        let proj = try await strip(body, ctx(100_000))
        #expect(proj.stripped == 0)
        #expect(contents(proj.body) == contents(body))
    }

    @Test func overBudgetStripsOldestBodiesInOneBatchDownToLow() async throws {
        let body = run(8)
        let full = AIContext.estimateTokens(body)
        let bud = Int64(Double(full) * 0.7)  // over by ~30%
        let proj = try await strip(body, ctx(bud))
        #expect(proj.stripped > 0)
        let out = AIContext.estimateTokens(proj.body)
        // HYSTERESIS: went well below the line, not just under it.
        #expect(Double(out) <= Double(bud) * AIStripStrategy.LOW + 200, "\(out) vs \(bud)")
        // OLDEST FIRST, CONTIGUOUS: stripped pids are a prefix of the reply order.
        let stubbed = (0..<8).filter { (proj.body[2 + 3 * $0]["content"].asString ?? "").contains("(stripped:") }
        #expect(stubbed == Array(0..<stubbed.count), "\(stubbed)")
        #expect(proj.omittedMarker == false, "no cut was needed")
    }

    @Test func aStubKeepsTheTagAndTheTitleAndSaysHowBigTheBodyWas() async throws {
        let body = run(8)
        let proj = try await strip(body, ctx(Int64(Double(AIContext.estimateTokens(body)) * 0.5)))
        let firstStub = proj.body[2]["content"].asString ?? ""
        #expect(firstStub.hasPrefix("[reply to ai:0000 · notes read · \"survey filler_0\"]"))
        #expect(firstStub.contains("(stripped: 6000 bytes"))
        #expect(!firstStub.contains(BIG))
    }

    @Test func theModelsOwnTurnsAreNeverTouched() async throws {
        let body = run(8)
        let proj = try await strip(body, ctx(Int64(Double(AIContext.estimateTokens(body)) * 0.5)))
        let before = contents(body.filter { $0["role"].asString == "assistant" })
        let after = contents(proj.body.filter { $0["role"].asString == "assistant" })
        #expect(before == after)
    }

    @Test func goalAndLiveTurnAreNeverStripped() async throws {
        var body = run(8)
        body[0] = reply("goal:0", "the goal itself")  // a reply-shaped goal turn
        body[body.count - 1] = reply("live:0", "a reply-shaped live turn")
        let proj = try await strip(body, ctx(Int64(Double(AIContext.estimateTokens(body)) * 0.4)))
        #expect((proj.body[0]["content"].asString ?? "").contains(BIG))
        #expect((proj.body[proj.body.count - 1]["content"].asString ?? "").contains(BIG))
    }

    /// The KV-cache property: between strip events the same body renders the same
    /// bytes, and a later projection never UN-strips (which would be an edit
    /// inside the prefix).
    @Test func strippingIsMonotoneAndTheRenderIsByteStableBetweenEvents() async throws {
        let body = run(8)
        let bud = Int64(Double(AIContext.estimateTokens(body)) * 0.6)
        let a = try await strip(body, ctx(bud))
        let b = try await strip(body, ctx(bud))  // same inputs, no new event
        #expect(JSON.array(a.body).serialize() == JSON.array(b.body).serialize())
        // Now the budget RELAXES; nothing may come back.
        let c = try await strip(body, ctx(bud * 4))
        #expect(JSON.array(c.body).serialize() == JSON.array(a.body).serialize())
    }

    @Test func aSecondEventOnlyExtendsTheStrippedRun() async throws {
        let body = run(8)
        let bud1 = Int64(Double(AIContext.estimateTokens(body)) * 0.8)
        let a = try await strip(body, ctx(bud1))
        let n1 = a.stripped
        let bud2 = Int64(Double(AIContext.estimateTokens(body)) * 0.4)
        let b = try await strip(body, ctx(bud2))
        #expect(b.stripped > n1)
        // the first n1 stubs are byte-identical to before — the prefix extended, no holes
        #expect(contents(Array(b.body.prefix(2 + 3 * n1))) == contents(Array(a.body.prefix(2 + 3 * n1))))
    }

    @Test func whenEveryBodyIsStrippedAndProseStillOverflowsTheCutIsStageTwo() async throws {
        var body = [turn("user", "GOAL")]
        for i in 0..<40 {
            body.append(turn("assistant", "long deliberation \(i) " + String(repeating: "y", count: 800)))
            body.append(reply("ai:" + String(format: "%04x", i), "t\(i)", body: String(repeating: "z", count: 50)))
        }
        body.append(turn("user", "now?"))
        let proj = try await strip(body, ctx(3000))
        #expect(proj.omittedMarker == true, "prose alone overflowed; the cut must have fired")
        #expect(proj.body[0]["content"].asString == "GOAL")
        #expect(proj.body[proj.body.count - 1]["content"].asString == "now?")
        #expect(AIContext.estimateTokens(proj.body) <= 3000)
    }

    @Test func perAgentStateDoesNotLeakAcrossAgents() async throws {
        let body = run(8)
        _ = try await strip(body, ctx(Int64(Double(AIContext.estimateTokens(body)) * 0.5), AI + "_a"))
        let fresh = try await strip(body, ctx(100_000, AI + "_b"))
        #expect(fresh.stripped == 0)
    }

    @Test func aRecallPageIsTrimmedToHalfTheWindowAndSaysSo() async throws {
        // python `_K(window=12000)`: a 12000 window with no output reserve.
        let (kernel, ids, _) = await bootMockKernel(
            config: baseConfig { .provider(MockProvider()) }, backendIds: [AI],
            contextWindow: 12000, meta: ["output_reserve": .integer(0)])
        let agentId = ids[AI]!
        appendRows(agentId, kernel: kernel, (0..<20).map { turn("user", "t\($0) " + String(repeating: "w", count: 8000)) })
        let r = await kernel.send([Item(agentId, .object([
                "type": .string("recall"), "client_id": .string("c"), "limit": .integer(20),
            ]))])[0].result
        let msgs = r["messages"].asArray ?? []
        #expect(0 < msgs.count && msgs.count < 20)
        #expect(AIContext.estimateTokens(msgs) <= 6000)
        #expect(r["truncated"].asBool == true)
        let note = r["note"].asString ?? ""
        #expect(note.contains("before=") && note.contains(String(msgs[0]["index"].asInt ?? -1)), Comment(rawValue: note))
        // the NEWEST of what was asked for is what survives
        #expect(msgs.last?["index"].asInt == 19)
    }

    /// python `_clean_title` — the title a reply tag carries.
    @Test func aTitleCannotCloseTheTagOrEndTheLine() {
        #expect(cleanTitle(.string("read filler_2] then\nrun")) == "read filler_2  then run")
        #expect(cleanTitle(.string("   ")) == nil)
        #expect(cleanTitle(.integer(42)) == nil)
        #expect(cleanTitle(.string(String(repeating: "t", count: 500)))?.count == 140)
    }
}
