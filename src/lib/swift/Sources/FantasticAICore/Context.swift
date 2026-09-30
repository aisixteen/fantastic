// Context-window budgeting primitives for the overflow strategies — a
// char-based token ESTIMATE, deliberately tokenizer-agnostic (exactness is
// irrelevant for a fit-to-window budget, and a real tokenizer would be wrong
// for gemma/nemotron anyway). Plus window/budget resolution off the agent
// record meta. Mirrors the Python `ai_core/context.py` / Rust `context.rs`.

import FantasticJSON
import FantasticKernel
import Foundation

public enum AIContext {

    /// Rough token estimate for ONE message — counts the SERIALIZED form,
    /// because role + content + the JSON envelope all consume real context.
    ///
    /// SERIALIZED THE WAY PYTHON SERIALIZES: `json.dumps(message, default=str)`
    /// — `', '`/`': '` separators and `ensure_ascii`, so `—` costs six chars, not
    /// three — measured in CODE POINTS. A compact, raw-UTF-8 dump made every
    /// estimate on this path smaller than python's, which is a different budget.
    public static func estimateOne(_ message: JSON) -> Int64 {
        let n = pyJsonDumps(message).unicodeScalars.count
        return Int64((n + AIConstants.CHARS_PER_TOKEN - 1) / AIConstants.CHARS_PER_TOKEN)
    }

    /// Sum of `estimateOne` across a message slice.
    public static func estimateTokens(_ messages: [JSON]) -> Int64 {
        messages.reduce(0) { $0 + estimateOne($1) }
    }

    /// Every scalar of `s` is a python decimal digit and there is at least one —
    /// `str.isdigit()` as `int()` can still read it (Arabic-Indic digits
    /// included, so `'٣'` is 3).
    static func pyDigits(_ s: String) -> Bool {
        let scalars = s.unicodeScalars
        if scalars.isEmpty { return false }
        return scalars.allSatisfy { $0.properties.numericType == .decimal }
    }

    /// Python `_as_pos_int`: a bool is nothing; an int must be > 0; a float > 0
    /// TRUNCATES (`0.5` → 0, which is not > 0 and so falls through to the next
    /// key); a string counts only when `strip().isdigit()`, so `'+12'` is
    /// nothing. Swift's `asInt` folded all three together and read `0.5` as
    /// 131072 and `'+12'` as 12.
    private static func metaPosInt(_ agent: Agent, _ key: String) -> Int64? {
        guard let v = agent.metaValue(forKey: key) else { return nil }
        switch v {
        case .bool: return nil
        case .integer(let i): return i > 0 ? i : nil
        case .double(let d):
            // `int(0.5)` is 0 and python RETURNS it — a window of 0 whose budget
            // then floors at 256. Truncation is the answer, not a rejection.
            guard d > 0, d.magnitude < 9.2e18 else { return nil }
            return Int64(d)
        case .string(let s):
            let t = pyStrip(s)
            guard pyDigits(t), let i = try? pyInt(.string(t)), i > 0 else { return nil }
            return Int64(i)
        default: return nil
        }
    }

    /// The model's usable window, by STATIC precedence (no fallback-chain):
    /// `context_window` (explicit override — works on any backend) →
    /// `num_ctx` (ollama's real knob) → a conservative default.
    public static func resolveContextWindow(_ agent: Agent) -> Int64 {
        if let v = metaPosInt(agent, "context_window") { return v }
        if let v = metaPosInt(agent, "num_ctx") { return v }
        return AIConstants.DEFAULT_CONTEXT_WINDOW
    }

    /// Output headroom reserved out of the window (default 1024).
    ///
    /// Python takes a non-bool int ≥ 0, or a string whose stripped form is all
    /// digits; EVERYTHING else is the default — `-5`, `"+5"`, `1000.0` and
    /// `"-5"` all read 1024. Swift accepted a negative string and a float, and a
    /// reserve of `-5` widens the budget past the window.
    public static func outputReserve(_ agent: Agent) -> Int64 {
        guard let v = agent.metaValue(forKey: "output_reserve") else { return AIConstants.DEFAULT_OUTPUT_RESERVE }
        switch v {
        case .integer(let i): return i >= 0 ? i : AIConstants.DEFAULT_OUTPUT_RESERVE
        case .string(let s):
            let t = pyStrip(s)
            guard pyDigits(t), let i = try? pyInt(.string(t)) else { return AIConstants.DEFAULT_OUTPUT_RESERVE }
            return Int64(i)
        default: return AIConstants.DEFAULT_OUTPUT_RESERVE
        }
    }

    /// Token budget for the INPUT (window minus output headroom), floored.
    public static func budget(_ agent: Agent) -> Int64 {
        max(resolveContextWindow(agent) - outputReserve(agent), AIConstants.BUDGET_FLOOR)
    }

    /// Tokens a reply may occupy INLINE in the model's next turn. Above this it
    /// is stored and digested. An eighth of the input budget: a 12k window
    /// inlines ~1.4k tokens (a reflect, an error, a short read) and digests a
    /// 6.8KB file body. Python `_inline_cap`.
    public static func inlineCap(_ agent: Agent) -> Int64 {
        max(AIConstants.BUDGET_FLOOR, budget(agent) / 8)
    }

    /// THE CAP INVARIANT. Every verb an agent calls on itself must answer UNDER
    /// the inline cap, or its answer is stored and digested and the model pulls
    /// its own reply (measured 2026-09-06: a 4788-byte grep result came back as
    /// a receipt; the run drowned in pulls). Keep items from the front while the
    /// rendered reply fits. Returns (kept, truncated). Python `_fit_reply`.
    ///
    /// MEASURED ON THE SERIALIZED FORM, with headroom for the envelope the verb
    /// wraps around these items (`total`, `next`, …). JSON escaping inflates raw
    /// text by up to ~30% on newline-heavy bodies; the margin is what keeps
    /// "fits" true at the seam.
    public static func fitReply(_ items: [JSON], capTokens: Int64, limit: Int? = nil)
        -> ([JSON], Bool)
    {
        let cap = Int64(Double(capTokens) * 0.8)
        var kept: [JSON] = []
        for it in items {
            if let limit, kept.count >= limit { return (kept, true) }
            let trial = kept + [it]
            let probe: JSON = .object([
                "role": .string("user"), "content": .string(pyJsonDumps(.array(trial))),
            ])
            if estimateOne(probe) > cap { return (kept, true) }
            kept = trial
        }
        return (kept, false)
    }

    /// The agent's configured `recent_n` (verbatim recent turns), clamped to
    /// `AIConstants.RECENT_N_RANGE`; default `AIConstants.RECENT_N`.
    ///
    /// Python is `int(rec.get("recent_n"))` inside a `TypeError/ValueError`
    /// guard, so `true` is 1, `7.9` is 7, `' 7 '` is 7 and `1000.0` clamps to
    /// 50 — none of which swift's int-or-string read produced.
    public static func recentN(_ agent: Agent) -> Int {
        var n = AIConstants.RECENT_N
        if let v = agent.metaValue(forKey: "recent_n"), let i = try? pyInt(v) { n = Int64(i) }
        let (lo, hi) = AIConstants.RECENT_N_RANGE
        return Int(min(max(n, lo), hi))
    }

    /// The agent's configured strategy, RAW — only an ABSENT key means
    /// `compact`.
    ///
    /// A record carrying `context_strategy: null` (or `5`) is a MISCONFIGURED
    /// agent, and python says so: `context_status` and `reflect` echo the value
    /// back and the projection refuses by name. Reading it as
    /// `asString ?? "compact"` silently ran compact instead — a fallback, and
    /// one that reported a posture the record does not have.
    public static func strategy(_ agent: Agent) -> JSON {
        agent.metaValue(forKey: "context_strategy") ?? .string("compact")
    }
}
