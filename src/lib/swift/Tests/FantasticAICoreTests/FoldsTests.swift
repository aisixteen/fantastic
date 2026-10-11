// Folds are marks: chains derived from `parent`, closed ones collapse to an
// appended row with refs, the view hides the members, and every one stays one
// descent away. Twins of python `ai_core/tests/test_folds.py`, case for case.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticKernel

/// One agent id per suite: the stripped set `fold` hands to `strip` is
/// process-global, keyed by agent, and suites run in parallel.
private let AI = "ai_f"

/// The body a projection folds is the agent's conversation, so it lives in the
/// agent's memory store — seeded there first (the store refuses a fold whose
/// refs name rows it does not hold, as sqlite's FK would).
private func fold(_ body: [JSON], _ c: ProjectionCtx) async throws -> AIProjection {
    if let k = c.kernel { appendRows(c.selfId, kernel: k, body.filter { rowId($0) != nil }) }
    // the projection runs in the agent's own turn: its store calls are the agent's
    return try await KernelTaskLocals.$sender.withValue(c.selfId) {
        try await AIStrategies.getStrategy("fold")!(body, [], .object([:]), c)
    }
}

private func viewIds(_ view: [JSON]) -> [Int64?] { view.map(rowId) }

private func serialized(_ rows: [JSON]) -> String { JSON.array(rows).serialize() }

@Suite("folds: a mark, not a deletion", .serialized)
struct FoldsTests {
    init() { AIStripStrategy.forget(nil, AI) }

    // ── chains and closure ──────────────────────────────────────────────

    @Test func chainsAreDerivedFromParentLinksAcrossAnInterleave() {
        let f = FoldFixture(agent: AI)
        let (a, b, c) = (
            f.chain("a", 2, big: false), f.chain("b", 3, big: false),
            f.chain("c", 1, big: false, openEnd: true)
        )
        let rows = interleave([a, b, c])
        let got = AIFolds.chains(rows)
        #expect(got.count == 3)
        let roots = Set(got.compactMap { rowId(rows[$0[0]]) })
        #expect(roots == [f.id("user:a"), f.id("user:b"), f.id("user:c")])
        for ch in got {  // a chain is a parent path: every member hangs off the previous one
            for j in 1..<ch.count {
                #expect(rowParent(rows[ch[j]]) == rowId(rows[ch[j - 1]]))
            }
        }
    }

    @Test func aChainEndingOnAnUnansweredSendIsOpen() {
        let f = FoldFixture(agent: AI)
        let rows = f.chain("z", 1, big: false, openEnd: true)
        let ch = AIFolds.chains(rows)[0]
        #expect(AIFolds.isOpen(rows, ch))
        let closed = f.chain("y", 1, big: false)
        #expect(!AIFolds.isOpen(closed, AIFolds.chains(closed)[0]))
    }

    @Test func rowsWithoutIdsAreTheirOwnRootsAndNeverFold() {
        let rows = [plainTurn("user", "old"), plainTurn("assistant", "older")]
        #expect(AIFolds.chains(rows).count == 2)
    }

    // ── render ──────────────────────────────────────────────────────────

    @Test func aFoldHidesItsRefsAndSitsWhereItsFirstRefWas() {
        let f = FoldFixture(agent: AI)
        let rows = f.chain("a", 2, big: false) + [f.row("user", "later", "user:l")]
        let fd = f.fold(f.ids(Array(rows.prefix(5))), "done a", "body a", false)
        let view = AIFolds.render(rows + [fd])
        let ids = viewIds(view)
        #expect(ids[0] == rowId(fd), "the fold renders at the first ref's position")
        let refs = Set(refsOf(fd))
        #expect(!view.contains { rowId($0).map { refs.contains($0) } ?? false })
        #expect(ids[ids.count - 1] == f.id("user:l"))
        let head = view[0]["content"].asString ?? ""
        #expect(head.hasPrefix("[fold ") && head.contains("\"done a\""))
    }

    @Test func aFoldOfFoldsHidesTheInnerFold() {
        let f = FoldFixture(agent: AI)
        let rows = f.chain("a", 1, big: false)
        let inner = f.fold(f.ids(rows), "inner", "b", false)
        let outer = f.fold([rowId(inner)!], "outer", "b", false)
        let view = AIFolds.render(rows + [inner, outer])
        #expect(viewIds(view) == [rowId(outer)])
    }

    @Test func expandIdsDescendsThroughFolds() {
        let f = FoldFixture(agent: AI)
        let rows = f.chain("a", 1, big: false)
        let inner = f.fold(f.ids(rows), "i", "b", false)
        let outer = f.fold([rowId(inner)!, f.id("other:1")], "o", "b", false)
        let got = AIFolds.expandIds(rows + [inner, outer], [rowId(outer)!])
        #expect(Set(got) == Set(f.ids(rows)).union([f.id("other:1")]))
    }

    // ── the strategy ────────────────────────────────────────────────────

    @Test func foldCollapsesTheOldestStretchFirstAndPersistsIt() async throws {
        let f = FoldFixture(agent: AI)
        let (a, b, c) = (f.chain("a", 3), f.chain("b", 3), f.chain("c", 2, openEnd: true))
        let mid = interleave([a, b, c])
        let body = [f.row("user", "GOAL", "user:goal")] + mid + [f.row("user", "now?", nil)]
        let full = AIContext.estimateTokens(body)
        let bud = Int64(Double(full) * 0.45)
        let proj = try await fold(body, f.ctx(bud))
        #expect(proj.folded >= 1)
        #expect(f.foldsOf().count == proj.folded, "every fold row was persisted")
        let blob = serialized(proj.body)
        #expect(blob.contains("GOAL") && blob.contains("now?"), "goal and live turn never fold")
        let tail = Set(mid.suffix(6).compactMap(rowId))
        let shown = Set(proj.body.compactMap(rowId))
        #expect(tail.isSubset(of: shown), "the live tail never folds")
        #expect(AIContext.estimateTokens(proj.body) <= bud)
    }

    @Test func aFoldIsMonotoneAcrossProjections() async throws {
        let f = FoldFixture(agent: AI)
        let (a, b) = (f.chain("a", 2), f.chain("b", 2))
        let body = [f.row("user", "GOAL", "user:goal")] + a + b + [f.row("user", "now?", nil)]
        let bud = Int64(Double(AIContext.estimateTokens(body)) * 0.5)
        let p1 = try await fold(body, f.ctx(bud))
        // next turn: the store now carries the fold rows; the view must not unfold them
        let persisted = f.rowsOf()
        let body2 = Array(body.dropLast()) + persisted.filter(isFold) + [body[body.count - 1]]
        let p2 = try await fold(body2, f.ctx(bud * 10))
        #expect(p2.folded == 0)
        // the SAME collapse renders: the folds visible now are exactly the ones visible then
        #expect(
            Set(p2.body.filter(isFold).compactMap(rowId)) == Set(p1.body.filter(isFold).compactMap(rowId)))
    }

    @Test func aFailedSummarizerStillWritesAFoldAndSaysSo() async throws {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 8)
        let body = [f.row("user", "GOAL", "user:goal")] + a + [f.row("user", "now?", nil)]
        let proj = try await fold(
            body,
            f.ctx(
                Int64(Double(AIContext.estimateTokens(body)) * 0.6),
                summarize: { _ in throw MockError.boom }))
        #expect(proj.folded >= 1)
        // Asserted on the STORE: with a snug budget the cut may take the fold out of
        // this turn's view, but the mark itself must have been written, degraded and
        // saying so.
        let stored = f.foldsOf()
        let fd = try #require(stored.first)
        #expect((fd["content"].asString ?? "").contains("summary unavailable"))
        #expect((fd["title"].asString ?? "").contains("read a0"), "degraded title is the members' titles")
    }

    // ── the prerequisite: views keep row keys ───────────────────────────

    /// Descent without `pull`: grep scoped to a fold id searches its members
    /// (the store expands the fold); `history {fold}` lists them with the fold's
    /// summary. python `test_grep_and_history_descend_into_a_fold`.
    @Test func grepAndHistoryDescendIntoAFold() async throws {
        let (kernel, ids, _) = await bootMockKernel(
            config: baseConfig { MockProvider() }, backendIds: [AI],
            contextWindow: 12000, meta: ["output_reserve": .integer(0)])
        let agentId = ids[AI]!
        let f = FoldFixture(kernel: kernel, agent: AI)
        var rows = f.chain("a", 1, big: false)
        rows[2] = withField(
            rows[2], "content",
            .string((rows[2]["content"].asString ?? "").replacingOccurrences(of: "ok", with: "the secret is 9f3a")))
        let fo = f.fold(f.ids(rows), "done a", "summary of a", false)
        f.seed(rows + [fo])
        let r = await kernel.send([Item(agentId, .object([
                "type": .string("grep"), "query": .string("secret"), "ids": .array([fo["id"]]),
            ]))])[0].result
        #expect(r["total"].asInt == 1 && r["matches"][0]["id"] == rows[2]["id"], Comment(rawValue: r.serialize()))
        let h = await kernel.send([Item(agentId, .object(["type": .string("history"), "fold": fo["id"]]))])[0].result
        #expect(h["fold"]["title"].asString == "done a" && h["fold"]["summary"].asString == "summary of a",
            Comment(rawValue: h.serialize()))
        #expect((h["rows"].asArray ?? []).map { $0["id"] } == rows.map { $0["id"] })
    }

    @Test func theViewTrimmersKeepRowKeys() {
        let turn: JSON = .object([
            "role": .string("user"), "id": .string("u:1"), "parent": .string("a:0"),
            "content": .string("[reply to n:1 · notes reflect]\nTREE\n\n[reply to n:2 · notes read]\nx"),
        ])
        let out2 = dedupeStaleReads([
            turn, plainTurn("assistant", "k"), withField(turn, "id", .string("u:2")),
        ])
        #expect(out2.filter { $0["role"].asString == "user" }.allSatisfy { $0.asObject?["id"] != nil })
    }

    // ── edge cases ──────────────────────────────────────────────────────

    @Test func anOpenSuperlongChainFoldsItsOldestStretchAndKeepsTheTail() async throws {
        let f = FoldFixture(agent: AI)
        let ch = f.chain("a", 12, openEnd: true)  // 25 rows, still awaiting a reply
        let body = [f.row("user", "GOAL", "user:goal")] + ch + [f.row("user", "now?", nil)]
        let span = AIFoldStrategy.nextSpan(body, AIFolds.render(body))
        #expect(span.count >= 2 && !span.contains(0))
        let tailIds = Set(ch.suffix(5).compactMap(rowId))
        #expect(!span.contains { rowId(body[$0]).map { tailIds.contains($0) } ?? false }, "the live tail never folds")
        #expect(rowRole(body[span[span.count - 1]]) != "assistant", "a span never ends between a call and its answer")
        let proj = try await fold(body, f.ctx(Int64(Double(AIContext.estimateTokens(body)) * 0.4)))
        #expect(proj.folded >= 1)
        #expect(serialized(proj.body).contains("one more"), "the open call at the tail is still in view")
    }

    @Test func foldsAreOrdinaryMembersSoNestingNeedsNoSecondStage() async throws {
        let f = FoldFixture(agent: AI)
        let chs = "abcdefghijklmnop".map { f.chain(String($0), 1, big: false) }  // 16: the 6-row tail is protected
        var rows = [f.row("user", "GOAL", "user:goal")] + interleave(chs)
        for c in chs {  // pre-folded with LARGE bodies, so the view is mostly folds
            rows.append(f.fold(f.ids(c), "done \(rowId(c[0])!)", String(repeating: "S", count: 1500), false))
        }
        rows.append(f.row("user", "now?", nil))
        let proj = try await fold(rows, f.ctx(1200))
        let top = proj.body.filter { isFold($0) && ($0["level"].asInt ?? 1) >= 2 }
        #expect(!top.isEmpty, "a fold whose members were folds was written by the SAME path")
        #expect(
            top.allSatisfy { !($0["body"].asString ?? "").isEmpty && !($0["title"].asString ?? "").isEmpty },
            "every level carries a summary")
    }

    @Test func foldOfFoldsTerminatesWhenEvenFoldsDoNotFit() async throws {
        let f = FoldFixture(agent: AI)
        var rows = [f.row("user", "GOAL", "user:goal")]
        for t in ["a", "b"] {
            let c = f.chain(t, 1, big: false)
            rows += c
            rows.append(f.fold(f.ids(c), "done \(t)", String(repeating: "S", count: 5000), false))
        }
        rows.append(f.row("user", "now?", nil))
        let proj = try await fold(rows, f.ctx(300))  // absurdly small
        #expect(!proj.body.isEmpty)  // it returned — no infinite loop
    }

    @Test func aTitleCannotCloseTheTagOrQuoteItself() {
        let f = FoldFixture(agent: AI)
        let fd = f.fold([f.id("x:1"), f.id("x:2")], "weird \"title] here", "b", false)
        let head = (fd["content"].asString ?? "").split(separator: "\n", omittingEmptySubsequences: false)[0]
        #expect(head.filter { $0 == "]" }.count == 1 && head.hasSuffix("\"weird 'title) here\""), "\(head)")
    }

    @Test func theSummarizerInputIsCapped() {
        let f = FoldFixture(agent: AI)
        let rows = (0..<6).map { f.row("user", String(repeating: "z", count: 50_000), "u:\($0)") }
        let txt = AIFolds.chainText(rows, Array(0..<6))
        #expect(txt.count <= AIConstants.SUMMARY_INPUT_CAP)
    }

    @Test func aFoldWrittenTwiceOverTheSameStretchIsNotWrittenTwice() async throws {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 8)
        let body = [f.row("user", "GOAL", "user:goal")] + a + [f.row("user", "now?", nil)]
        let bud = Int64(Double(AIContext.estimateTokens(body)) * 0.5)
        let p1 = try await fold(body, f.ctx(bud))
        let persisted = f.rowsOf()
        let body2 = Array(body.dropLast()) + persisted.filter(isFold) + [body[body.count - 1]]
        let p2 = try await fold(body2, f.ctx(bud))
        #expect(f.foldsOf().count == p1.folded + p2.folded, "no duplicate fold over an already-folded stretch")
    }

    @Test func overlappingFoldsNeverShowAMemberTwice() {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 2, big: false)
        let f1 = f.fold(f.ids(Array(a.prefix(3))), "part", "b", false)
        let f2 = f.fold(f.ids(a), "whole", "b", false)
        let view = AIFolds.render(a + [f1, f2])
        let ids = view.compactMap(rowId)
        #expect(ids.count == Set(ids).count)
        let members = Set(f.ids(a))
        #expect(!ids.contains { members.contains($0) })
    }

    @Test func aFoldWhoseMemberWasPrunedStillRendersAndExpands() {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 1, big: false)
        let fd = f.fold(f.ids(a) + [f.id("gone:0000")], "done", "b", false)
        let view = AIFolds.render(a + [fd])
        #expect(viewIds(view) == [rowId(fd)])
        #expect(AIFolds.expandIds(a + [fd], [rowId(fd)!]).contains(f.id("gone:0000")))
    }

    @Test func aLateReplyWhoseParentIsFoldedStaysVisibleAndAttached() {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 1, big: false)
        let fd = f.fold(f.ids(a), "done", "b", false)
        let late = f.row("user", "[reply to notes:late · notes read · ↑ai:a0]\nlate reply", "ai:late", "ai:a0")
        let view = AIFolds.render(a + [fd, late])
        #expect(viewIds(view) == [rowId(fd), f.id("ai:late")])
        // attached by parent, not orphaned
        #expect(AIFolds.chains(a + [fd, late])[0].contains(a.count + 1))
    }

    /// SPAN = 1024 (owner, 2026-10-11): one overflow folds the whole foldable
    /// stretch into ONE fold — not 8 rows at a time (python E39: 47 folds, 4
    /// levels). Twelve levels in 26 cuts was the older failure; one fold per
    /// overflow cannot build that list.
    @Test func oneOverflowFoldsTheWholeStretchOnce() async throws {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 30)
        let body = [f.row("user", "GOAL", "user:goal")] + a + [f.row("user", "now?", nil)]
        let bud = Int64(Double(AIContext.estimateTokens(body)) * 0.35)
        _ = try await fold(body, f.ctx(bud))
        let levels = f.foldsOf().map { $0["level"].asInt ?? 1 }
        #expect(levels == [1], "\(levels)")
    }

    @Test func aFoldInheritsItsFirstMembersParent() {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 1, big: false)
        let fd = f.fold([rowId(a[1])!, rowId(a[2])!], "t", "b", false, parent: rowParent(a[1]))
        #expect(rowParent(fd) == f.id("user:a"))
        #expect(AIFolds.chains(a + [fd])[0] == [0, 1, 2, 3], "the fold sits in the chain it collapsed")
    }

    /// THE OTHER DIRECTION OF THE SAME KNOB. The test above pins the CEILING (no
    /// linked list of summaries); this pins the FLOOR — folds must actually
    /// coalesce.
    ///
    /// A real run folds ONCE per turn at most: each projection drops the view
    /// under the watermark and stops. So the head accumulates folds one at a
    /// time, and the threshold is the whole story. At FOLD_RUN=4 a run that
    /// produced three folds never nested at all — measured 2026-09-11 (python),
    /// `nested_folds` and `flat_roots` both ended `levels [1]`, a head that grows
    /// one fold per span forever. The synthetic budget fixtures cannot show this:
    /// they fold many times inside ONE call and sail past four.
    ///
    /// So the rule is exercised directly, at exactly the count that failed.
    @Test func threeAdjacentFoldsAtTheHeadFoldAsAGroup() {
        let f = FoldFixture(agent: AI)
        let goal = f.row("user", "GOAL", "user:goal")
        let heads = (0..<3).map { f.fold([], "fold \($0)", "body", false, parent: nil, id: 1000 + Int64($0)) }
        let tail = (0..<(AIConstants.KEEP_TAIL + 1)).map { f.row("user", "t\($0)", "user:t\($0)") }
        let rows = [goal] + heads + tail
        let span = AIFoldStrategy.nextSpan(rows, rows)
        // The three folds are at indices 1-3; anything else means the rule skipped
        // them and took ordinary rows, so they would never coalesce.
        #expect(span == [1, 2, 3], "three adjacent folds at the head must fold as a group, got \(span)")
    }

    /// Folds at the head used to coalesce ONLY while the view was over the
    /// watermark. A projection that dropped under it left its adjacent folds
    /// standing, and they stayed standing — the next projection folds ordinary
    /// rows first and only revisits the head when it is over budget with FOLD_RUN
    /// folds already there. So a session grew one fold per span at the head,
    /// forever, which is the growth FOLD_RUN exists to stop. Measured 2026-09-11
    /// (python) on `flat_roots`: three level-1 folds and no nesting across 51 turns.
    ///
    /// Here the body ALREADY FITS the budget, so the fold loop never runs; only
    /// the tidy pass can produce the level-2 fold.
    @Test func theHeadIsCoalescedEvenWhenTheViewAlreadyFits() async throws {
        let f = FoldFixture(agent: AI)
        let goal = f.row("user", "GOAL", "user:goal")
        // Each fold must COVER real rows, or `render` has nowhere to place it and
        // the view never shows it at all.
        let src = (0..<6).map { f.row("user", "s\($0)", "user:s\($0)") }
        let heads = (0..<3).map { i in
            f.fold(
                [rowId(src[2 * i])!, rowId(src[2 * i + 1])!], "f\(i)", "b", false,
                parent: rowParent(src[2 * i]), id: 2000 + Int64(i))
        }
        let tail = (0..<7).map { f.row("user", "t\($0)", "user:t\($0)") }
        let body = [goal] + src + heads + tail
        let huge = AIContext.estimateTokens(body) * 100  // comfortably under budget
        _ = try await fold(body, f.ctx(huge))
        let levels = f.foldsOf().map { $0["level"].asInt ?? 1 }
        #expect(!levels.isEmpty && (levels.max() ?? 0) >= 2, "the head was left uncoalesced while under budget: \(levels)")
    }

    // B5.22: `_summarize` splits the model's text with python's `splitlines()`.
    // Splitting on `"\n"` as a CHARACTER read a CRLF summary as ONE line, so the
    // whole summary became the title and the body fell back to it.
    @Test func crlfSummarySplitsLikePython() async throws {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 8)
        let body = [f.row("user", "GOAL", "user:goal")] + a + [f.row("user", "now?", nil)]
        _ = try await fold(
            body,
            f.ctx(
                Int64(Double(AIContext.estimateTokens(body)) * 0.6),
                summarize: { _ in "\"TITLE line\"\r\n\r\nBody line one.\r\nBody line two." }))
        let fd = try #require(f.foldsOf().first)
        #expect(fd["title"].asString == "TITLE line", "\(fd["title"].serialize())")
        let content = fd["content"].asString ?? ""
        #expect(content.contains("Body line one."), "\(content)")
        #expect(!content.contains("TITLE line\r\n"), "the title is not the whole text")
    }

    // B5.22: `body or title` falls back to the UNCUT title, not the cut one.
    @Test func summaryBodyFallsBackToTheUncutTitle() async throws {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 8)
        let body = [f.row("user", "GOAL", "user:goal")] + a + [f.row("user", "now?", nil)]
        let long = String(repeating: "t", count: 200)
        _ = try await fold(
            body,
            f.ctx(
                Int64(Double(AIContext.estimateTokens(body)) * 0.6),
                summarize: { _ in long }))
        let fd = try #require(f.foldsOf().first)
        #expect(fd["title"].asString?.count == 140, "the title is cut at 140")
        #expect((fd["content"].asString ?? "").contains(long), "the BODY keeps all 200")
    }
}
