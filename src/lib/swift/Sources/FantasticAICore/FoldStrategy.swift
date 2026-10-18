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
// joined titles — and says so on its own line. Degraded, loud, never silent. A
// summary that stays OVER ITS BUDGET (or is CUT OFF by it) after `SUMMARY_RETRIES`
// re-summarises fails the turn LOUD (`FoldSummaryError`).
//
// Python `ai_core/strategies/fold.py` is the reference; rust `fold.rs` the guide.

import FantasticJSON
import FantasticKernel
import Foundation

public enum AIFoldStrategy {
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
        String(String.UnicodeScalarView(t.unicodeScalars.prefix(AIConstants.MAX_TITLE)))
    }


    /// Python `FOLD_PROMPT`; `{budget}` is filled in.
    static func foldPrompt(_ budget: Int64) -> String {
        "FOLD this finished piece of work in AT MOST ~\(budget) tokens. First line: a "
            + "one-sentence title (what it was and how it ended). Then a blank line. Then these "
            + "sections, each one line or a few, in this order, omitting none (write `none`):\n"
            + "ASKED: what was asked.\n"
            + "DONE: what was found or decided — AND WHY.\n"
            + "BINDING: rules, constraints and choices that still bind.\n"
            + "ERRORS: every error, quoted verbatim.\n"
            + "OPEN: what is unfinished.\n"
            + "If the piece contains earlier fold summaries, MERGE them — on a conflict the newer "
            + "statement wins, and unfinished work is carried forward. Output only that.\n\n"
    }

    /// Python `RESUMMARIZE`.
    static func resummarize(got: Int64, budget: Int64) -> String {
        "Your summary below is ~\(got) tokens; the budget is \(budget). Rewrite it to fit — "
            + "keep the title line, the reasons, the binding rules and every quoted error; drop "
            + "narration. Output only the summary.\n\n"
    }

    /// Python `RESUMMARIZE_CUT`.
    static func resummarizeCut(budget: Int64) -> String {
        "Your summary below was CUT OFF at the budget (\(budget) tokens) before it ended. "
            + "Write it again, shorter, so it ENDS within the budget — keep the title line, the "
            + "reasons, the binding rules and every quoted error; drop narration. Output only "
            + "the summary.\n\n"
    }

    static let pathRx = try! NSRegularExpression(
        pattern: "[\\w./-]+\\.(?:txt|md|py|json|ya?ml|js|ts|rs|swift|toml|csv|log|sh|html|css)\\b")
    static let identRx = try! NSRegularExpression(pattern: "\\b[0-9a-f]{8,}\\b")
    /// Lines the RUNTIME wrote into a user turn — never the user's own words.
    static let RUNTIME_PREFIXES = [
        "[reply to ", "[no call", "[context-notice", "[fold ", "[recall", "[late reply",
        "[cleared ",
    ]

    private static func findAll(_ rx: NSRegularExpression, _ text: String) -> [String] {
        let ns = text as NSString
        return rx.matches(in: text, range: NSRange(location: 0, length: ns.length)).map {
            ns.substring(with: $0.range)
        }
    }

    /// `sorted(set(xs))` — python's code-point order.
    private static func pySortedSet(_ xs: [String]) -> [String] {
        Array(Set(xs)).sorted { Array($0.unicodeScalars.map(\.value)).lexicographicallyPrecedes(Array($1.unicodeScalars.map(\.value))) }
    }

    /// What a fold must never lose, extracted WITHOUT the model: the files and the
    /// identifiers its members name, and what the USER said, verbatim (newest kept
    /// first when over the cap). Appended to the summary — no summariser can drop
    /// it. Python `mechanical_index`.
    public static func mechanicalIndex(_ rows: [JSON], _ chain: [Int]) -> String {
        let text = chain.map { rowContent(rows[$0]) }.joined(separator: "\n")
        let files = Array(pySortedSet(findAll(pathRx, text)).prefix(30))
        let idents = Array(pySortedSet(findAll(identRx, text)).prefix(20))
        var said: [String] = []
        var used = 0
        for i in chain.reversed() {
            let r = rows[i]
            var c = pyStrip(rowContent(r))
            if rowRole(r) != "user" || pyTruthy(r["kind"]) || c.isEmpty
                || RUNTIME_PREFIXES.contains(where: { c.hasPrefix($0) })
            {
                continue
            }
            c = pySplitWhitespace(c).joined(separator: " ")
            if used + c.unicodeScalars.count > AIConstants.INDEX_USER_CHARS {
                c = String(String.UnicodeScalarView(c.unicodeScalars.prefix(max(0, AIConstants.INDEX_USER_CHARS - used))))
                    + " …"
            }
            said.insert(c, at: 0)
            used += c.unicodeScalars.count
            if used >= AIConstants.INDEX_USER_CHARS { break }
        }
        var lines: [String] = []
        if !files.isEmpty { lines.append("files: " + files.joined(separator: ", ")) }
        if !idents.isEmpty { lines.append("identifiers: " + idents.joined(separator: ", ")) }
        lines += said.map { "user said: \($0)" }
        return lines.isEmpty ? "" : "INDEX (mechanical):\n" + lines.joined(separator: "\n")
    }

    /// (title, body) out of a summary, or `nil` when it is not one. Python
    /// `_split`.
    static func split(_ text: String) -> (String, String)? {
        // The title is the first NON-EMPTY line; a leading blank line or a stray
        // quote must not become the title. Reasoning is already stripped by the
        // summarizer; a summary that still opens a think block is not a summary.
        // `text.splitlines()` — python's boundary set, so a CRLF summary is
        // several lines and not one.
        let lines = pySplitlines(text).map(pyStrip).filter { !$0.isEmpty }
        guard let firstLine = lines.first, !firstLine.hasPrefix("<think") else { return nil }
        let title = pyStrip(pyTrim(pyTrim(firstLine, "\""), "*#"))
        let body = pyStrip(lines.dropFirst().joined(separator: "\n"))
        // `title[:AIConstants.MAX_TITLE] or "(untitled)"`, and the body falls back to the
        // UNCUT title — python's `body or title`.
        let cut = cutTitle(title)
        return (cut.isEmpty ? "(untitled)" : cut, body.isEmpty ? title : body)
    }

    /// (title, body, degraded). The model is told the budget
    /// (`AIConstants.SUMMARY_BUDGET`); the call's `maxTokens` is the budget plus
    /// `AIConstants.THINK_ALLOWANCE` (a reasoning model's thought counts against
    /// it). An answer OVER the budget, or CUT OFF by it, is asked again, telling the
    /// model the numbers — up to `AIConstants.SUMMARY_RETRIES` times; then
    /// `FoldSummaryError`, never a truncated summary and never a silent unfolded
    /// span. Python `_summarize`.
    static func summarize(
        _ ctx: ProjectionCtx, _ rows: [JSON], _ chain: [Int]
    ) async throws -> (String, String, Bool) {
        let degraded = { () -> (String, String, Bool) in
            let t = titlesOf(rows, chain)
            return (cutTitle(t), t, true)
        }
        guard let summarize = ctx.summarize else { return degraded() }
        let budget = AIConstants.SUMMARY_BUDGET
        func ask(_ content: String) async -> (String, Bool) {
            do {
                let (text, cut) = try await summarize(
                    [.object(["role": .string("user"), "content": .string(content)])],
                    Int(budget + AIConstants.THINK_ALLOWANCE))
                return (pyStrip(text), cut)
            } catch {
                return ("", false)
            }
        }
        func cost(_ t: String) -> Int64 {
            AIContext.estimateOne(.object(["role": .string("user"), "content": .string(t)]))
        }
        var (text, cut) = await ask(foldPrompt(budget) + AIFolds.chainText(rows, chain))
        if text.isEmpty { return degraded() }
        var got = cost(text)
        var retries = 0
        while cut || got > budget {
            if retries == AIConstants.SUMMARY_RETRIES {
                let ids = chain.compactMap { rowId(rows[$0]) }
                throw FoldSummaryError(
                    "fold: the summary of rows \(ids.first ?? 0)…\(ids.last ?? 0) was "
                        + (cut ? "cut off at" : "~\(got) tokens, over")
                        + " its \(budget)-token budget after \(AIConstants.SUMMARY_RETRIES) "
                        + "re-summarises — nothing folded")
            }
            retries += 1
            let tell = cut ? resummarizeCut(budget: budget) : resummarize(got: got, budget: budget)
            let (again, againCut) = await ask(tell + text)
            cut = againCut
            if !again.isEmpty { text = again }
            got = cost(text)
        }
        guard let parts = split(text) else { return degraded() }
        return (parts.0, parts.1, false)
    }

    /// Summarize `idxs` and append ONE fold row for them (a summary that will not
    /// fit throws `FoldSummaryError`). A fold row in `idxs` is
    /// just a member — that is how folds nest: the next fold over this stretch of
    /// the view takes the earlier fold as one of its rows, and its level is one
    /// above the deepest member. Python `write_fold`.
    static func writeFold(
        _ ctx: ProjectionCtx, _ rows: [JSON], _ idxs: [Int], _ anchors: [Int64: Int64]
    ) async throws -> JSON {
        let ids = idxs.compactMap { rowId(rows[$0]) }
        let anchor = try AIFolds.memberAnchor(rows, idxs, anchors)
        let lvl = 1 + (idxs.filter { isFold(rows[$0]) }.map { AIFolds.level(rows, rows[$0]) }.max() ?? 0)
        // THE INDEX RIDES BESIDE THE SUMMARY, outside its budget.
        let index = mechanicalIndex(rows, idxs)
        let (title, summary, degraded) = try await summarize(ctx, rows, idxs)
        let text = index.isEmpty ? summary : "\(summary)\n\n\(index)"
        guard let kernel = ctx.kernel else {
            throw PyError("RuntimeError", "fold: no kernel to mint the fold's id from")
        }
        let fr = AIFolds.makeFold(
            refs: ids, title: title, body: text, degraded: degraded,
            id: kernel.mintId(), anchor: anchor, lvl: lvl,
            parent: idxs.first.flatMap { rowParent(rows[$0]) })
        // A fold is a MARK that must persist — written whoever the projection is for
        // (a turn's caller, or a `context_recall` with none).
        try await saveHistory(ctx.selfId.value, kernel, [fr])
        return fr
    }

    /// The oldest foldable stretch of the VIEW, as indices into `rows`. Python
    /// `next_span`.
    ///
    /// Never the goal turn, never the last `keepTail` rows, and never ending
    /// between a call and its answer. Folds are ordinary members, but NOT the
    /// first pick: a stretch that starts on the previous fold wraps it, and doing
    /// that every time builds a linked list of summaries-of-summaries. So: skip the folds at the head and take the oldest
    /// NON-fold rows; only when `AIConstants.FOLD_RUN` folds sit adjacent at the head do
    /// they become the span themselves. Depth then grows with the log of the
    /// history, not its length.
    static func nextSpan(
        _ rows: [JSON], _ view: [JSON], need: Int64, keepTail: Int = AIConstants.KEEP_TAIL
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
        if lead >= AIConstants.FOLD_RUN {
            take = Array(head[..<lead])  // fold the folds — same operation, same summarizer
        } else {
            // the oldest rows whose tokens reach `need` — never a row count
            take = []
            var got: Int64 = 0
            for r in head[lead...] {
                if isFold(r) || got >= need { break }
                take.append(r)
                got += AIContext.estimateTokens([r])
            }
            while let last = take.last, rowRole(last) == "assistant" { take.removeLast() }
        }
        return take.count >= 2 ? take.compactMap { rowId($0).flatMap { byId[$0] } } : []
    }

    /// FOLD WHEN NEAR FULL: once the rendered view passes `FOLD_AT` of `budget`,
    /// ONE fold takes the oldest rows, by tokens, down to `LOW` of it; then tidy the
    /// head (`FOLD_RUN` adjacent folds become one parent — the fold tree), budget or
    /// no budget. Returns `(rows + the fold rows
    /// written, the view, folded, unanchored)`. The turn's `fold` projection and
    /// `context_recall` (a store longer than the window, e.g. written under a
    /// larger one) share it. Python `fold_down`.
    ///
    /// Folds are placed by `ctx.anchors` (computed over the HEAD by the caller), so
    /// a body rebuilt from the view array, which holds folds without their members,
    /// keeps every fold in place.
    static func foldDown(
        _ rowsIn: [JSON], _ ctx: ProjectionCtx, budget: Int64
    ) async throws -> (rows: [JSON], view: [JSON], folded: Int, unanchored: Int) {
        guard var anchors = ctx.anchors else {
            throw PyError("ValueError", "fold: no anchors — the caller computes them over the head")
        }
        var rows = rowsIn
        var view = AIFolds.render(rows, anchors)
        var folded = 0
        if AIContext.estimateTokens(view) > Int64(Double(budget) * AIConstants.FOLD_AT)
            && rows.count > 2
        {
            let target = Int64(Double(budget) * AIConstants.LOW)
            // ONE FOLD, SIZED BY TOKENS: the oldest rows that bring the view to LOW,
            // plus room for the fold row itself (its summary budget). Repeated only
            // if that still leaves the view over LOW, or nothing foldable is left.
            while AIContext.estimateTokens(view) > target {
                let need = AIContext.estimateTokens(view) - target + AIConstants.SUMMARY_BUDGET
                let span = nextSpan(rows, view, need: need)
                if span.isEmpty { break }
                let fr = try await writeFold(ctx, rows, span, anchors)
                if let id = rowId(fr), let a = fr["anchor"].asInt { anchors[id] = a }
                rows.append(fr)
                folded += 1
                view = AIFolds.render(rows, anchors)
            }
        }
        // TIDY THE HEAD, budget or no budget. Coalescing only while the view is OVER
        // the watermark would leave a projection's adjacent folds standing for good,
        // because the next projection folds ordinary rows first and only revisits the
        // head when it is over budget with `AIConstants.FOLD_RUN` folds already there —
        // one fold per span at the head, forever.
        //
        // Terminates: each pass replaces `AIConstants.FOLD_RUN` adjacent folds with ONE, so
        // the run shrinks by `AIConstants.FOLD_RUN - 1` every time and `nextSpan` stops
        // selecting once fewer than `AIConstants.FOLD_RUN` remain.
        while true {
            let span = nextSpan(rows, view, need: 0)  // only a FOLD_RUN of folds qualifies here
            if span.isEmpty || !span.allSatisfy({ isFold(rows[$0]) }) { break }
            let fr = try await writeFold(ctx, rows, span, anchors)
            if let id = rowId(fr), let a = fr["anchor"].asInt { anchors[id] = a }
            rows.append(fr)
            folded += 1
            view = AIFolds.render(rows, anchors)
        }
        return (rows, view, folded, AIFolds.unanchored(rows, anchors))
    }

    /// Fold the oldest stretches of the view down to the low watermark, tidy the
    /// head, then hand what is left to `strip`. Python `fold`.
    public static func fold(
        _ body: [JSON], _ systemBlock: [JSON], _ rec: JSON, _ ctx: ProjectionCtx
    ) async throws -> AIProjection {
        let down = try await foldDown(
            body, ctx, budget: ctx.budget - AIConstants.NOTICE_ENVELOPE_RESERVE)
        // Stage two and three: strip bodies in what is left, then cut — strip's
        // own logic.
        let out = try await AIStripStrategy.strip(down.view, systemBlock, rec, ctx)
        return AIProjection(
            body: out.body, omittedMarker: out.omittedMarker,
            stripped: out.stripped, strippedBytes: out.strippedBytes, folded: down.folded,
            unanchored: down.unanchored)
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

/// A span's summary stayed over `AIConstants.SUMMARY_BUDGET` (or was cut at it)
/// after `AIConstants.SUMMARY_RETRIES` re-summarises — the fold fails the turn,
/// loud. Python `FoldSummaryError(RuntimeError)`.
public struct FoldSummaryError: Error, CustomStringConvertible, Sendable {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}
