// `fold` — self-inflicted, mechanical compaction that leaves marks the model can walk.
//
// At the watermark, oldest first, one batch down to LOW:
//   1. FOLD closed chains — one summarizing call per chain (title + body), one
//      appended fold row each with `refs`; the members stay one descent away
//   2. still over → STRIP bodies in open chains (the `strip` strategy, unchanged)
//   3. still over → CUT (strip's own second stage)
//
// THE ONE STRATEGY THAT WRITES. Every other strategy is a pure projection; this
// one appends fold rows to the durable store, because a fold is a mark that must
// persist — the next turn has to see the same collapse, or the prefix rewrites
// every turn. Bookkeeping from above, same path as the chat store itself.
// Nothing is ever edited.
//
// If the summarizer fails, the fold is still written — refs plus the members'
// joined titles — and says so on its own line. Degraded, loud, never silent.
//
// Python `ai_core/strategies/fold.py` is the reference; rust `fold.rs` the guide.

import FantasticJSON
import FantasticKernel
import Foundation

public enum AIFoldStrategy {
    /// The longest a fold title may be, in chars. Python `MAX_TITLE`.
    public static let MAX_TITLE = 140
    /// Rows at the end of the view that never fold — the live tail. Python `KEEP_TAIL`.
    public static let KEEP_TAIL = 6
    /// Rows per fold at the watermark — small enough to summarize, large enough
    /// to matter. Python `SPAN`.
    public static let SPAN = 8
    /// Adjacent folds at the head of the view before they fold as a group.
    ///
    /// THREE, not four. A real run folds at most once per turn — each projection
    /// drops the view under the watermark and stops — so the head accumulates
    /// folds one at a time and this threshold is the whole story. At four it
    /// never fired: measured 2026-09-11 (python), `nested_folds` and `flat_roots`
    /// both ended `levels [1]`, a head that grows one fold per span forever,
    /// which is the growth this constant exists to stop.
    ///
    /// Three still skips-then-batches, so depth grows as log₃ of the history
    /// rather than one level per cut (the 2026-09-06 measurement: twelve levels
    /// in 26 cuts, when every span began on the previous fold). Python `FOLD_RUN`.
    public static let FOLD_RUN = 3

    /// The joined `· "title"` marks of a span — what a degraded fold carries
    /// instead of a summary. Python `_titles_of`.
    static func titlesOf(_ rows: [JSON], _ chain: [Int]) -> String {
        var out: [String] = []
        for i in chain {
            if let t = AIFolds.quotedTitle(rowContent(rows[i])), !out.contains(t) {
                out.append(t)
            }
        }
        return out.isEmpty ? "(untitled)" : out.joined(separator: "; ")
    }

    private static func cutTitle(_ t: String) -> String {
        String(String.UnicodeScalarView(t.unicodeScalars.prefix(MAX_TITLE)))
    }

    /// (title, body, degraded). One summarizer call for one closed chain. Python
    /// `_summarize`.
    static func summarize(_ ctx: ProjectionCtx, _ rows: [JSON], _ chain: [Int]) async -> (String, String, Bool) {
        let degraded = { () -> (String, String, Bool) in
            let t = titlesOf(rows, chain)
            return (cutTitle(t), t, true)
        }
        guard let summarize = ctx.summarize else { return degraded() }
        let ask: JSON = .object([
            "role": .string("user"),
            "content": .string(
                "FOLD this finished piece of work. First line: a one-sentence title (what it "
                    + "was and how it ended). Then a blank line. Then 2-5 sentences: what was asked, "
                    + "what was found or decided, anything that still binds. Output only that.\n\n"
                    + AIFolds.chainText(rows, chain)),
        ])
        var text = ""
        do { text = try await summarize([ask]) } catch { text = "" }
        text = pyStrip(text)
        if text.isEmpty { return degraded() }
        // The title is the first NON-EMPTY line; a leading blank line or a stray
        // quote must not become the title. Reasoning is already stripped by the
        // summarizer; a summary that still opens a think block is not a summary —
        // degrade to titles.
        // `text.splitlines()` — python's boundary set, so a CRLF summary is
        // several lines and not one. Splitting on `"\n"` as a Character read the
        // WHOLE CRLF text as a single line, which became the title.
        let lines = pySplitlines(text).map(pyStrip).filter { !$0.isEmpty }
        guard let firstLine = lines.first, !firstLine.hasPrefix("<think") else { return degraded() }
        let title = pyStrip(pyTrim(pyTrim(firstLine, "\""), "*#"))
        let body = pyStrip(lines.dropFirst().joined(separator: "\n"))
        // `title[:MAX_TITLE] or "(untitled)"`, and the body falls back to the
        // UNCUT title — python's `body or title`, not `body or title[:140]`.
        let cut = cutTitle(title)
        return (cut.isEmpty ? "(untitled)" : cut, body.isEmpty ? title : body, false)
    }

    /// Summarize `idxs` and append ONE fold row for them. A fold row in `idxs` is
    /// just a member — that is how folds nest: the next fold over this stretch
    /// of the view takes the earlier fold as one of its rows, and its level is
    /// one above the deepest member. Python `write_fold`.
    static func writeFold(_ ctx: ProjectionCtx, _ rows: [JSON], _ idxs: [Int]) async -> JSON {
        let ids = idxs.compactMap { rowId(rows[$0]) }
        let lvl = 1 + (idxs.filter { isFold(rows[$0]) }.map { AIFolds.level(rows, rows[$0]) }.max() ?? 0)
        let (title, text, degraded) = await summarize(ctx, rows, idxs)
        let fr = AIFolds.makeFold(
            refs: ids, title: title, body: text, degraded: degraded,
            id: ctx.kernel.rows.mintId(), lvl: lvl,
            parent: idxs.first.flatMap { rowParent(rows[$0]) })
        if let c = ctx.clientId, !c.isEmpty {
            appendRows(ctx.selfId, kernel: ctx.kernel, [fr])
        }
        return fr
    }

    /// The oldest foldable stretch of the VIEW, as indices into `rows`. Python
    /// `next_span`.
    ///
    /// Never the goal turn, never the last `keepTail` rows, and never ending
    /// between a call and its answer. Folds are ordinary members, but NOT the
    /// first pick: a stretch that starts on the previous fold wraps it, and doing
    /// that every time builds a linked list of summaries-of-summaries (measured
    /// 2026-09-06, python: twelve levels deep in 26 cuts, the model lost in it
    /// for 50 turns). So: skip the folds at the head and take the oldest
    /// NON-fold rows; only when `FOLD_RUN` folds sit adjacent at the head do
    /// they become the span themselves. Depth then grows with the log of the
    /// history, not its length.
    static func nextSpan(
        _ rows: [JSON], _ view: [JSON], keepTail: Int = KEEP_TAIL, size: Int = SPAN
    ) -> [Int] {
        var byId: [Int64: Int] = [:]
        for (i, r) in rows.enumerated() {
            if let id = rowId(r), id != 0 { byId[id] = i }
        }
        let head: [JSON] =
            view.count > keepTail + 1
            ? view[1..<(view.count - keepTail)].filter { rowId($0).map { byId[$0] != nil } ?? false }
            : []
        var lead = 0
        while lead < head.count && isFold(head[lead]) { lead += 1 }
        var take: [JSON]
        if lead >= FOLD_RUN {
            take = Array(head[..<lead])  // fold the folds — same operation, same summarizer
        } else {
            take = Array(head[lead..<min(head.count, lead + size)])
            while let last = take.last, rowRole(last) == "assistant" { take.removeLast() }
        }
        return take.count >= 2 ? take.compactMap { rowId($0).flatMap { byId[$0] } } : []
    }

    /// Fold the oldest stretches of the view down to the low watermark, tidy the
    /// head, then hand what is left to `strip`. Python `fold`.
    public static func fold(
        _ body: [JSON], _ systemBlock: [JSON], _ rec: JSON, _ ctx: ProjectionCtx
    ) async -> AIProjection {
        var rows = body
        var view = AIFolds.render(rows)
        let budget = ctx.budget - AIContext.noticeEnvelopeReserve
        var folded = 0
        if AIContext.estimateTokens(view) > budget && rows.count > 2 {
            let target = Int64(Double(budget) * AIStripStrategy.LOW)
            // ONE MOVE, REPEATED: fold the oldest stretch of the view — rows, folds,
            // or a mix — until under the low watermark or nothing foldable is left.
            while AIContext.estimateTokens(view) > target {
                let span = nextSpan(rows, view)
                if span.isEmpty { break }
                rows.append(await writeFold(ctx, rows, span))
                folded += 1
                view = AIFolds.render(rows)
            }
        }
        // TIDY THE HEAD, budget or no budget. Coalescing used to happen only while
        // the view was OVER the watermark, so a projection that dropped under it
        // left its adjacent folds standing — and they stayed standing, because the
        // next projection folds ordinary rows first and only revisits the head
        // when it is over budget with `FOLD_RUN` folds already there. A session
        // therefore grew one fold per span at the head, forever.
        //
        // Terminates: each pass replaces `FOLD_RUN` adjacent folds with ONE, so
        // the run shrinks by `FOLD_RUN - 1` every time and `nextSpan` stops
        // selecting once fewer than `FOLD_RUN` remain.
        while true {
            let span = nextSpan(rows, view)
            if span.isEmpty || !span.allSatisfy({ isFold(rows[$0]) }) { break }
            rows.append(await writeFold(ctx, rows, span))
            folded += 1
            view = AIFolds.render(rows)
        }
        // Stage two and three: strip bodies in what is left, then cut — strip's
        // own logic.
        let out = await AIStripStrategy.strip(view, systemBlock, rec, ctx)
        return AIProjection(
            body: out.body, omittedMarker: out.omittedMarker,
            stripped: out.stripped, strippedBytes: out.strippedBytes, folded: folded)
    }
}

/// Python's `str.strip(chars)`: every LEADING and TRAILING scalar that is one of
/// `chars` is removed. `CharacterSet` trimming compares Characters, which is a
/// different set the moment a combining mark is involved.
private func pyTrim(_ s: String, _ chars: String) -> String {
    let set = Set(chars.unicodeScalars)
    var view = Array(s.unicodeScalars)
    var lo = 0
    var hi = view.count
    while lo < hi, set.contains(view[lo]) { lo += 1 }
    while hi > lo, set.contains(view[hi - 1]) { hi -= 1 }
    view = Array(view[lo..<hi])
    return String(String.UnicodeScalarView(view))
}
