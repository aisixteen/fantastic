// Context-overflow strategies (the projection logic). A strategy maps the
// conversation BODY (history + live user turn, WITHOUT the system block) to a
// `Projection`: a shortened body that fits the budget + a structured artifact
// (a summary, or an omitted-span flag). It does NOT fabricate a notice turn —
// the ONE canonical [context-notice] is composed at the seam. The durable
// store is NEVER touched by `compact`/`truncate`/`strip`; `fold` is the one
// strategy that WRITES (a mark that does not persist is a prefix that rewrites
// every turn). Mirrors Python `ai_core/strategies` / Rust `strategies/`.
//
// Selection is STATIC per agent (`context_strategy`, default `compact`); no
// runtime try-X-else-Y (NO-FALLBACKS). The persist
// nudge is universal in the seam notice.

import FantasticJSON
import FantasticKernel
import Foundation

/// What a strategy returns: the projected body + the artifact the seam needs
/// to compose the canonical context-notice. NEVER a fabricated user turn.
/// Python `strategies/base.Projection`.
public struct AIProjection: Sendable {
    /// Projected conversation body — NO notice turn, NO system block.
    public var body: [JSON]
    /// compact: the LLM summary text; truncate/none: nil.
    public var summary: String?
    /// truncate: an earlier span was elided in place.
    public var omittedMarker: Bool
    /// The agent's live FACT BLOCK, rendered. Carried as an ARTIFACT for the
    /// same reason `summary` is — a strategy states what it retained, the seam
    /// is the one place that turns it into a turn. Empty for every strategy
    /// shipped today (python's `halve` was the one that set it).
    public var pinned: [String]
    /// `strip`: reply blocks reduced to their stub this projection, and the
    /// bytes that left the view with them. Reported in the artifact so a probe
    /// can prove the mechanism ran, separately from whether the run succeeded.
    public var stripped: Int
    public var strippedBytes: Int
    /// `fold`: fold rows written this projection.
    public var folded: Int
    public init(
        body: [JSON], summary: String? = nil, omittedMarker: Bool = false,
        pinned: [String] = [], stripped: Int = 0, strippedBytes: Int = 0, folded: Int = 0
    ) {
        self.body = body
        self.summary = summary
        self.omittedMarker = omittedMarker
        self.pinned = pinned
        self.stripped = stripped
        self.strippedBytes = strippedBytes
        self.folded = folded
    }
}

/// The side call that writes a summary: a plain text completion over the backend
/// provider (no tools — providers stream raw text). `maxTokens` bounds the output
/// (the fold's summary budget); `cut` says whether the provider stopped AT that
/// bound, so a cut-off summary is never kept as if it were whole. Python
/// `_Summarizer` (`__call__(msgs, max_tokens)` + `last_cut`).
public typealias AISummarizer =
    @Sendable ([JSON], Int?) async throws -> (text: String, cut: Bool)

/// Everything a strategy needs, WITHOUT importing the seam. Python
/// `context.ProjectionCtx`.
///
/// `budget` here is the budget for the conversation BODY (the window budget
/// minus the already-measured system block) — strategies fit the body to it.
/// `summarize` is a closure over the backend provider (nil for a caller that
/// cannot summarize); `compact` and `fold` use it. `clientId` is the
/// conversation this projection is for — `fold` is the one strategy that
/// WRITES and it needs to know which store.
public struct ProjectionCtx: Sendable {
    public var budget: Int64
    public var recentN: Int
    public var summarize: AISummarizer?
    public var selfId: AgentId
    /// `nil` for a strategy exercised with no kernel behind it (python's
    /// `kernel=None`): `strip` then starts unseeded and `fold` refuses to write.
    public var kernel: Kernel?
    public var clientId: String?
    public init(
        budget: Int64, recentN: Int, summarize: AISummarizer?,
        selfId: AgentId, kernel: Kernel?, clientId: String? = nil
    ) {
        self.budget = budget
        self.recentN = recentN
        self.summarize = summarize
        self.selfId = selfId
        self.kernel = kernel
        self.clientId = clientId
    }
}

/// `(body, system_block, rec, ctx) -> Projection` (body excludes the system
/// block). Python `STRATEGY`. THROWS: `fold` writes its fold rows to the agent's
/// memory store, and a store that refuses fails the turn fast
/// (`MemoryStoreError`).
public typealias AIStrategy =
    @Sendable ([JSON], [JSON], JSON, ProjectionCtx) async throws -> AIProjection

public enum AIStrategies {
    /// The stub used when a summarizer is unavailable / fails — a degraded
    /// artifact (the full transcript is whole in the durable store), NOT a
    /// fallback chain. Python `STUB_SUMMARY`.
    public static let stubSummary = "[Earlier conversation omitted — summary unavailable]"

    /// Python `DEFAULT_STRATEGY`.
    public static let defaultStrategy = "compact"

    /// Keep the largest SUFFIX of `turns` that fits `budget`, always including
    /// the last turn (the live request).
    public static func fitTail(_ turns: [JSON], _ budget: Int64) -> [JSON] {
        guard let last = turns.last else { return [] }
        var kept = [last]
        var used = AIContext.estimateOne(last)
        for t in turns.dropLast().reversed() {
            let c = AIContext.estimateOne(t)
            if used + c > budget { break }
            kept.insert(t, at: 0)
            used += c
        }
        return kept
    }

    /// Split `body` into (overflow, recent) at the last `recentN` turns.
    public static func recentSplit(_ body: [JSON], _ recentN: Int) -> ([JSON], [JSON]) {
        let start = max(0, body.count - recentN)
        return (Array(body[..<start]), Array(body[start...]))
    }

    /// Closure over the backend provider: summarize a span of turns to a
    /// string. A plain text completion (no tools — providers stream raw text).
    /// Python `_make_summarizer`.
    ///
    /// The input is NOT char-capped: python hands the span over whole. A
    /// REASONING MODEL THINKS OUT LOUD HERE TOO, burying the summary under the
    /// reasoning. Side calls strip like the main turn.
    public static func makeSummarizer(_ provider: any AIProvider) -> AISummarizer {
        { msgs, maxTokens in
            let rendered = msgs.map { m -> String in
                let role = m["role"].asString ?? "?"
                let content = m["content"].asString ?? m["content"].serialize()
                return "\(role): \(content)"
            }.joined(separator: "\n")
            let prompt: [JSON] = [
                .object([
                    "role": .string("system"),
                    "content": .string(
                        "Summarize the conversation excerpt below concisely, PRESERVING "
                            + "names, decisions, facts, preferences, and unresolved tasks. "
                            + "Output ONLY the summary."),
                ]),
                .object(["role": .string("user"), "content": .string(rendered)]),
            ]
            // A FRESH reading: a provider that reports nothing this time must not
            // leave last call's stop reason standing.
            provider.usage?.lastStopReason = nil
            var parts = ""
            for try await chunk in provider.chat(messages: prompt, maxTokens: maxTokens) {
                if case .token(let t) = chunk { parts += t }
            }
            let cut = provider.usage?.lastStopReason == "max_tokens"
            return (withoutThinking(parts), cut)
        }
    }

    /// Summarize the overflow via the backend provider; degrade to a stub on
    /// ANY failure (a degraded artifact — the full transcript is whole in the
    /// chat store). Python `safe_summary`.
    public static func safeSummary(_ ctx: ProjectionCtx, _ overflow: [JSON]) async -> String {
        guard let summarize = ctx.summarize else { return stubSummary }
        let s: String
        do { s = try await summarize(overflow, nil).text } catch { return stubSummary }
        let t = s.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? stubSummary : t
    }

    /// `compact` — the DEFAULT strategy. Keep the recent turns verbatim + an
    /// LLM summary of the overflow. The summary rides the artifact (NOT a
    /// fabricated turn); the seam wraps it into the one canonical
    /// context-notice.
    public static func compact(
        _ body: [JSON], _ systemBlock: [JSON], _ rec: JSON, _ ctx: ProjectionCtx
    ) async -> AIProjection {
        let (overflow, recent) = recentSplit(body, ctx.recentN)
        if overflow.isEmpty {
            return AIProjection(body: fitTail(body, ctx.budget))
        }
        let summary = await safeSummary(ctx, overflow)
        // Reserve room for the notice the seam will prepend: the wrapper envelope
        // + the summary text itself (the seam embeds this exact summary). One
        // budget authority.
        let summaryCost = AIContext.estimateOne(
            .object(["role": .string("user"), "content": .string(summary)]))
        let avail = max(0, ctx.budget - AIConstants.NOTICE_ENVELOPE_RESERVE - summaryCost)
        // PIN THE GOAL. The first turn is the task framing, and it is the OLDEST — so it
        // is the first thing the overflow split takes. A summary is a lossy retelling;
        // the one turn that must survive verbatim is the instruction the whole run
        // serves: without it the agent no longer knows what it is doing. `truncate`
        // pins this turn too. Cost is one turn of budget, charged before the tail.
        let first = body[0]
        let headCost = AIContext.estimateOne(first)
        let kept = fitTail(recent, max(0, avail - headCost))
        // Only re-add it if the recent window does not already carry it (short bodies).
        if let k = kept.first, k == first {
            return AIProjection(body: kept, summary: summary)
        }
        return AIProjection(body: [first] + kept, summary: summary)
    }

    /// `truncate` — keep the first (task-framing) turn + the recent turns,
    /// drop the middle. Cheap: NO summarizer call. The elision is reported via
    /// `omittedMarker` (NOT a fabricated turn).
    public static func truncate(
        _ body: [JSON], _ systemBlock: [JSON], _ rec: JSON, _ ctx: ProjectionCtx
    ) async -> AIProjection {
        if body.count <= 1 {
            return AIProjection(body: fitTail(body, ctx.budget))
        }
        let first = body[0]
        // Reserve room for the notice the seam prepends (no summary in truncate).
        let headCost = AIContext.estimateOne(first) + AIConstants.NOTICE_ENVELOPE_RESERVE
        let tail = fitTail(Array(body[1...]), max(0, ctx.budget - headCost))
        return AIProjection(body: [first] + tail, omittedMarker: true)
    }

    /// The registry, in python's order. `strip` keeps every turn and drops
    /// reply BODIES instead — the ledger at mixed resolution. `fold` leaves
    /// MARKS: closed stretches collapse to an appended fold row (summary +
    /// refs) the model can descend into; then strip, then cut.
    public static let registry: [(String, AIStrategy)] = [
        ("compact", compact),
        ("truncate", truncate),
        ("strip", AIStripStrategy.strip),
        ("fold", AIFoldStrategy.fold),
    ]

    /// Return the strategy by name, or nil for an unknown name — the caller
    /// surfaces a config error, NOT a silent fall-through to the default.
    /// Python `get_strategy`.
    public static func getStrategy(_ name: String) -> AIStrategy? {
        registry.first { $0.0 == name }?.1
    }

    /// Python `strategy_names()`.
    public static func strategyNames() -> [String] {
        registry.map { $0.0 }
    }
}
