// `strip` — the context is the ledger at MIXED RESOLUTION. Bodies go, titles stay.
//
// Every call the model makes carries a one-sentence `title` it wrote at the
// moment it made the call (see `TITLES_HOWTO`). The reply comes back tagged with
// that title. So when the window fills, the cheapest safe move is not to drop
// TURNS but to drop reply BODIES: the 6.8KB read becomes one line — its tag, its
// title, its size, its pid — and stays addressable by `recall`. Every assistant
// turn survives untouched, which matters more than it looks: the model's own
// prose after a reply IS its distillation of that reply, written with the task in
// mind, and it is tiny.
//
//     L0  full row     [reply to pid · notes read · "survey chunk 2"]\n<6800 bytes>
//     L1  stub         [reply to pid · notes read · "survey chunk 2"] (stripped: 6800 bytes)
//
// TWO RULES THAT KEEP THE KV CACHE WARM. Measured on this stack (vLLM /metrics):
// an append after the cached prefix reuses it in full; any edit inside it reuses
// NOTHING. So —
//   * HYSTERESIS. Strip only past the HIGH watermark, and strip down to AIConstants.LOW in
//     one batch: ten stubs in one event is one cache miss, not ten.
//   * MONOTONE, OLDEST FIRST. Once stripped, always stripped, and the stripped
//     region is a contiguous run from the front — the stable prefix only ever
//     EXTENDS, never gets holes.
// No LLM call. If every body is stripped and the prose alone still overflows, the
// second stage is the cut — not a fallback, the next stage of the same strategy,
// and it says so in the artifact.
//
// Python `ai_core/strategies/strip.py` is the reference; rust `strip.rs` the guide.

import FantasticJSON
import FantasticKernel
import Foundation

public enum AIStripStrategy {
    /// Per-agent set of stripped pids, as the TAG writes them. In-memory,
    /// process-lifetime — the same standing as the projection cache: a strip is
    /// a VIEW decision, the durable store is untouched, so losing this on restart
    /// costs one re-strip and nothing else. Python `_stripped`.
    private static let lock = NSLock()
    nonisolated(unsafe) private static var stripped: [String: Set<String>] = [:]

    /// Forget an agent's stripped set — `context_recall` drops RAM scratch the
    /// way python's `_strip_state.pop` does; tests clear it between cases.
    static func forget(_ selfId: String) {
        lock.lock()
        stripped[selfId] = nil
        lock.unlock()
    }

    /// TEST SEAM: is there a stripped set for this agent at all?
    static func has(_ selfId: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return stripped[selfId] != nil
    }

    /// TEST SEAM: seed an agent's stripped set directly.
    static func set(_ selfId: String, _ pids: Set<String>) {
        lock.lock()
        stripped[selfId] = pids
        lock.unlock()
    }

    /// The agent's stripped set, if one exists. SYNCHRONOUS: `NSLock` is
    /// unavailable from an async context, so the strategy reads through this.
    private static func stored(_ selfId: String) -> Set<String>? {
        lock.lock()
        defer { lock.unlock() }
        return stripped[selfId]
    }

    /// python's `_stripped.setdefault(self_id, seed)`: keep what is there, else
    /// store `seed`; answer what is stored.
    private static func setDefault(_ selfId: String, _ seed: Set<String>) -> Set<String> {
        lock.lock()
        defer { lock.unlock() }
        if let d = stripped[selfId] { return d }
        stripped[selfId] = seed
        return seed
    }

    /// A delivered reply turn — the only rows whose bodies strip. Python `_is_reply`.
    static func isReply(_ m: JSON) -> Bool {
        guard m["role"].asString == "user", let c = m["content"].asString else { return false }
        return c.hasPrefix("[reply to ")
    }

    /// Project `body` with every stripped block reduced to its stub. Deterministic
    /// in (body, stripped) — the same inputs render the same bytes, which is what
    /// keeps the prefix stable between strip events. Python `_render`.
    static func render(_ body: [JSON], _ done: Set<String>) -> ([JSON], Int, Int) {
        var out: [JSON] = []
        var n = 0
        var saved = 0
        for m in body {
            if !isReply(m) {
                out.append(m)
                continue
            }
            var parts: [String] = []
            for b in replyBlocks(m["content"].asString ?? "") {
                if let p = pidOf(b), done.contains(p) {
                    let s = stub(b)
                    parts.append(s)
                    n += 1
                    saved += pyLen(b) - pyLen(s)
                } else {
                    parts.append(b)
                }
            }
            out.append(withField(m, "content", .string(parts.joined(separator: "\n\n"))))
        }
        return (out, n, saved)
    }

    /// The back half — strip's second stage, what happens when every body is
    /// already a stub and the prose alone still overflows. Python `_halve`.
    static func halve(_ body: [JSON]) -> [JSON] {
        Array(body[(body.count / 2)...])
    }

    /// SEEDED FROM THE VIEW ARRAY on first use — stubs survive a restart, and the
    /// prefix bytes are the same before and after it.
    private static func seed(_ ctx: ProjectionCtx, _ body: [JSON]) async throws -> Set<String> {
        var out: Set<String> = []
        // No kernel behind the projection (python `ctx.kernel is None`): nothing
        // to seed from.
        guard let kernel = ctx.kernel,
            let view = try await loadView(ctx.selfId.value, kernel)
        else { return out }
        let stubbed: Set<Int64> = Set(
            (view["ids"].asArray ?? []).compactMap { e in
                e["res"].asString == "stub" ? e["id"].asInt : nil
            })
        for m in body {
            guard let id = rowId(m), stubbed.contains(id), isReply(m) else { continue }
            for b in replyBlocks(m["content"].asString ?? "") {
                if let p = pidOf(b) { out.insert(p) }
            }
        }
        return out
    }

    /// Reduce delivered reply BODIES to their stubs, oldest first, down to the
    /// low watermark; cut only when the prose alone still overflows. Python `strip`.
    public static func strip(
        _ body: [JSON], _ systemBlock: [JSON], _ rec: JSON, _ ctx: ProjectionCtx
    ) async throws -> AIProjection {
        if body.count <= 1 {
            return AIProjection(body: AIStrategies.fitTail(body, ctx.budget))
        }
        let key = ctx.selfId.value
        var done: Set<String>
        if let s = stored(key) {
            done = s
        } else {
            done = setDefault(key, try await seed(ctx, body))
        }
        let budget = ctx.budget - AIConstants.NOTICE_ENVELOPE_RESERVE
        var (rendered, n, saved) = render(body, done)
        if AIContext.estimateTokens(rendered) > budget {
            // OVER THE HIGH WATERMARK: strip oldest-first, in ONE batch, down to AIConstants.LOW.
            let target = Int64(Double(budget) * AIConstants.LOW)
            for m in body[1..<(body.count - 1)] {  // never the goal, never the live turn
                if AIContext.estimateTokens(rendered) <= target { break }
                if !isReply(m) { continue }
                for b in replyBlocks(m["content"].asString ?? "") {
                    if let p = pidOf(b) { done.insert(p) }
                }
                (rendered, n, saved) = render(body, done)
            }
            set(key, done)
        }
        var cut = false
        if AIContext.estimateTokens(rendered) > budget {
            // Everything strippable is stripped and the prose alone still overflows.
            // Stage two: the cut. The goal turn is pinned for the reason `halve`
            // documents.
            let first = rendered[0]
            let tail = AIStrategies.fitTail(
                halve(Array(rendered[1...])), max(0, budget - AIContext.estimateOne(first)))
            rendered = [first] + tail
            cut = true
        }
        return AIProjection(body: rendered, omittedMarker: cut, stripped: n, strippedBytes: saved)
    }
}
