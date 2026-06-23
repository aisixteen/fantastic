// Context-overflow strategies (the projection logic). A strategy maps the
// conversation BODY (history + live user turn, WITHOUT the system block) to a
// `Projection`: a shortened body that fits the budget + a structured artifact
// (a summary, or an omitted-span flag). It does NOT fabricate a notice turn —
// the ONE canonical [context-notice] is composed at the seam. The durable
// store is NEVER touched. Mirrors Python `ai_core/strategies` / Rust
// `strategies.rs`.
//
// Tool-pairing is load-bearing: the OpenAI/NIM wire rejects a `role:tool` that
// isn't preceded by its `assistant.tool_calls` turn. Every cut drops orphaned
// leading `role:tool` messages so the body stays wire-valid.
//
// Selection is STATIC per agent (`context_strategy`, default `compact`); no
// runtime try-X-else-Y (NO-FALLBACKS). `memgpt` was removed — its persist
// nudge is now universal in the seam notice.

import FantasticJSON
import Foundation

/// What a strategy returns: the projected body + the artifact the seam needs
/// to compose the canonical context-notice. NEVER a fabricated user turn.
public struct AIProjection: Sendable {
    public var body: [JSON]
    public var summary: String?
    public var omittedMarker: Bool
    public init(body: [JSON], summary: String? = nil, omittedMarker: Bool = false) {
        self.body = body
        self.summary = summary
        self.omittedMarker = omittedMarker
    }
}

public enum AIStrategies {
    /// The stub used when a summarizer is unavailable / fails — a degraded
    /// artifact (the full transcript is whole in the durable store), NOT a
    /// fallback chain.
    public static let stubSummary = "[Earlier conversation omitted — summary unavailable]"

    static func isTool(_ turn: JSON) -> Bool {
        turn["role"].asString == "tool"
    }

    /// Drop leading `role:tool` messages whose owning `assistant.tool_calls`
    /// turn is not present — the model wire would reject them.
    public static func dropOrphanTools(_ turns: [JSON]) -> [JSON] {
        var i = 0
        while i < turns.count && isTool(turns[i]) { i += 1 }
        return Array(turns[i...])
    }

    /// Keep the largest SUFFIX of `turns` that fits `budget`, always including
    /// the last turn (the live request), then drop orphaned leading tools.
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
        return dropOrphanTools(kept)
    }

    /// Split `body` into (overflow, recent) at the last `recentN` turns,
    /// snapping the boundary back so `recent` never STARTS on an orphan tool.
    public static func recentSplit(_ body: [JSON], _ recentN: Int) -> ([JSON], [JSON]) {
        var start = max(0, body.count - recentN)
        while start > 0 && isTool(body[start]) { start -= 1 }
        return (Array(body[..<start]), Array(body[start...]))
    }

    /// Summarize a span of turns via the backend provider (tool-free
    /// completion); degrade to a stub on ANY failure.
    public static func safeSummary(provider: any AIProvider, overflow: [JSON]) async -> String {
        let rendered = overflow.map { m -> String in
            let role = m["role"].asString ?? "?"
            let content = m["content"].asString ?? m["content"].serialize()
            return "\(role): \(content)"
        }.joined(separator: "\n")
        let capped = String(rendered.prefix(20000))
        let prompt: [JSON] = [
            .object([
                "role": .string("system"),
                "content": .string(
                    "Summarize the conversation excerpt below concisely, PRESERVING names, "
                        + "decisions, facts, preferences, and unresolved tasks. Output ONLY "
                        + "the summary."),
            ]),
            .object(["role": .string("user"), "content": .string(capped)]),
        ]
        var parts = ""
        do {
            let stream = provider.chat(messages: prompt)
            for try await chunk in stream {
                if case .token(let t) = chunk { parts += t }
            }
        } catch {
            return stubSummary
        }
        let s = parts.trimmingCharacters(in: .whitespacesAndNewlines)
        return s.isEmpty ? stubSummary : s
    }

    /// `compact` (DEFAULT): keep the recent turns verbatim + an LLM summary of
    /// the overflow. The summary rides the artifact; the seam wraps it.
    public static func compact(
        body: [JSON], recentN: Int, budget: Int64, provider: any AIProvider
    ) async -> AIProjection {
        let (overflow, recent) = recentSplit(body, recentN)
        if overflow.isEmpty {
            return AIProjection(body: fitTail(body, budget))
        }
        let summary = await safeSummary(provider: provider, overflow: overflow)
        let summaryCost = AIContext.estimateOne(
            .object(["role": .string("user"), "content": .string(summary)]))
        let avail = max(0, budget - AIContext.noticeEnvelopeReserve - summaryCost)
        return AIProjection(body: fitTail(recent, avail), summary: summary)
    }

    /// `truncate`: keep the first (task-framing) turn + the recent turns, drop
    /// the middle. NO summarizer. The elision is reported via `omittedMarker`.
    public static func truncate(body: [JSON], budget: Int64) -> AIProjection {
        if body.count <= 1 {
            return AIProjection(body: fitTail(body, budget))
        }
        let first = body[0]
        let headCost = AIContext.estimateOne(first) + AIContext.noticeEnvelopeReserve
        let tail = fitTail(Array(body[1...]), max(0, budget - headCost))
        return AIProjection(body: [first] + tail, summary: nil, omittedMarker: true)
    }

    /// The known strategy names (config validity check; unknown ⇒ caller errors).
    public static func isKnown(_ name: String) -> Bool {
        name == "compact" || name == "truncate"
    }
}
