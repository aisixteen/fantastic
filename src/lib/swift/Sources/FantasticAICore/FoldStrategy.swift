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
// summary that stays OVER ITS BUDGET (or is CUT OFF by it) after one re-summarise
// is REFUSED: nothing is folded, the span stays in view, `fold_refused` reports it.
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
        "[cleared ", "[fork of ",
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

    /// The summary budget for one span. Python `summary_budget`.
    public static func summaryBudget(_ ctx: ProjectionCtx, _ rows: [JSON], _ chain: [Int]) -> Int64 {
        let span = AIContext.estimateTokens(chain.map { rows[$0] })
        return max(
            AIConstants.SUMMARY_FLOOR,
            min(Int64(Double(span) * AIConstants.SUMMARY_SHARE), Int64(Double(ctx.budget) * AIConstants.WINDOW_SHARE)))
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

    /// (title, body, degraded), or `nil` when the summary stays OVER ITS BUDGET (or
    /// is CUT OFF by it) after one re-summarise — then nothing is folded (the span
    /// stays in view; loud in `context_status`), never a truncated summary.
    ///
    /// The budget is stated in the prompt; the call's `maxTokens` is the budget
    /// plus `AIConstants.THINK_ALLOWANCE` (a reasoning model's thought counts against it);
    /// `reserve` is what the fold row spends on its mechanical index, so summary +
    /// index fit. A provider that stopped at `maxTokens` says so (`cut`) — a
    /// cut-off summary is treated like an oversized one, never kept. Python
    /// `_summarize`.
    static func summarize(
        _ ctx: ProjectionCtx, _ rows: [JSON], _ chain: [Int], reserve: Int64 = 0
    ) async -> (String, String, Bool)? {
        let degraded = { () -> (String, String, Bool) in
            let t = titlesOf(rows, chain)
            return (cutTitle(t), t, true)
        }
        guard let summarize = ctx.summarize else { return degraded() }
        let budget = max(AIConstants.SUMMARY_FLOOR, summaryBudget(ctx, rows, chain) - reserve)
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
        var (text, cut) = await ask(foldPrompt(budget) + AIFolds.chainText(rows, chain))
        if text.isEmpty { return degraded() }
        let got = AIContext.estimateOne(.object(["role": .string("user"), "content": .string(text)]))
        if cut || got > budget {
            // ONE re-summarise, telling the model the numbers.
            let tell = cut ? resummarizeCut(budget: budget) : resummarize(got: got, budget: budget)
            (text, cut) = await ask(tell + text)
            if text.isEmpty || cut
                || AIContext.estimateOne(.object(["role": .string("user"), "content": .string(text)]))
                    > budget
            {
                return nil
            }
        }
        guard let parts = split(text) else { return degraded() }
        return (parts.0, parts.1, false)
    }

    /// Summarize `idxs` and append ONE fold row for them — or `nil` when the
    /// summary would not fit its budget (nothing folded). A fold row in `idxs` is
    /// just a member — that is how folds nest: the next fold over this stretch of
    /// the view takes the earlier fold as one of its rows, and its level is one
    /// above the deepest member. Python `write_fold`.
    static func writeFold(_ ctx: ProjectionCtx, _ rows: [JSON], _ idxs: [Int]) async throws -> JSON? {
        let ids = idxs.compactMap { rowId(rows[$0]) }
        let lvl = 1 + (idxs.filter { isFold(rows[$0]) }.map { AIFolds.level(rows, rows[$0]) }.max() ?? 0)
        // THE INDEX IS INSIDE THE BUDGET: built first, its size reserved from the
        // summary's.
        let index = mechanicalIndex(rows, idxs)
        let reserve =
            index.isEmpty
            ? 0 : AIContext.estimateOne(.object(["role": .string("user"), "content": .string(index)]))
        guard let (title, summary, degraded) = await summarize(ctx, rows, idxs, reserve: reserve)
        else { return nil }
        let text = index.isEmpty ? summary : "\(summary)\n\n\(index)"
        guard let kernel = ctx.kernel else {
            throw PyError("RuntimeError", "fold: no kernel to mint the fold's id from")
        }
        let fr = AIFolds.makeFold(
            refs: ids, title: title, body: text, degraded: degraded,
            id: kernel.mintId(), lvl: lvl,
            parent: idxs.first.flatMap { rowParent(rows[$0]) })
        if let c = ctx.clientId, !c.isEmpty {
            try await saveHistory(ctx.selfId.value, kernel, [fr])
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
    /// NON-fold rows; only when `AIConstants.FOLD_RUN` folds sit adjacent at the head do
    /// they become the span themselves. Depth then grows with the log of the
    /// history, not its length.
    static func nextSpan(
        _ rows: [JSON], _ view: [JSON], keepTail: Int = AIConstants.KEEP_TAIL, size: Int = AIConstants.SPAN
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
            take = Array(head[lead..<min(head.count, lead + size)])
            while let last = take.last, rowRole(last) == "assistant" { take.removeLast() }
        }
        return take.count >= 2 ? take.compactMap { rowId($0).flatMap { byId[$0] } } : []
    }

    /// Fold the oldest stretches of the view down to the low watermark, tidy the
    /// head, then hand what is left to `strip`. Python `fold`.
    public static func fold(
        _ body: [JSON], _ systemBlock: [JSON], _ rec: JSON, _ ctx: ProjectionCtx
    ) async throws -> AIProjection {
        var rows = body
        var view = AIFolds.render(rows)
        let budget = ctx.budget - AIConstants.NOTICE_ENVELOPE_RESERVE
        var folded = 0
        var refused = 0
        if AIContext.estimateTokens(view) > budget && rows.count > 2 {
            let target = Int64(Double(budget) * AIConstants.LOW)
            // ONE MOVE, REPEATED: fold the oldest stretch of the view — rows, folds,
            // or a mix — until under the low watermark or nothing foldable is left.
            while AIContext.estimateTokens(view) > target {
                let span = nextSpan(rows, view)
                if span.isEmpty { break }
                guard let fr = try await writeFold(ctx, rows, span) else {
                    refused += 1  // the summary would not fit: this span stays; stop here
                    break
                }
                rows.append(fr)
                folded += 1
                view = AIFolds.render(rows)
            }
        }
        // TIDY THE HEAD, budget or no budget. Coalescing used to happen only while
        // the view was OVER the watermark, so a projection that dropped under it
        // left its adjacent folds standing — and they stayed standing, because the
        // next projection folds ordinary rows first and only revisits the head
        // when it is over budget with `AIConstants.FOLD_RUN` folds already there. A session
        // therefore grew one fold per span at the head, forever.
        //
        // Terminates: each pass replaces `AIConstants.FOLD_RUN` adjacent folds with ONE, so
        // the run shrinks by `AIConstants.FOLD_RUN - 1` every time and `nextSpan` stops
        // selecting once fewer than `AIConstants.FOLD_RUN` remain.
        while refused == 0 {
            let span = nextSpan(rows, view)
            if span.isEmpty || !span.allSatisfy({ isFold(rows[$0]) }) { break }
            guard let fr = try await writeFold(ctx, rows, span) else {
                refused += 1
                break
            }
            rows.append(fr)
            folded += 1
            view = AIFolds.render(rows)
        }
        // Stage two and three: strip bodies in what is left, then cut — strip's
        // own logic.
        let out = try await AIStripStrategy.strip(view, systemBlock, rec, ctx)
        return AIProjection(
            body: out.body, omittedMarker: out.omittedMarker,
            stripped: out.stripped, strippedBytes: out.strippedBytes, folded: folded,
            foldRefused: refused)
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
