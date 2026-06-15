// Context-window budgeting primitives for the overflow strategies — a
// char-based token ESTIMATE, deliberately tokenizer-agnostic (exactness is
// irrelevant for a fit-to-window budget, and a real tokenizer would be wrong
// for gemma/nemotron anyway). Plus window/budget resolution off the agent
// record meta. Mirrors the Python `ai_core/context.py` / Rust `context.rs`.

import FantasticJSON
import FantasticKernel
import Foundation

public enum AIContext {
    /// ~chars per token for the estimate.
    public static let charsPerToken = 4
    /// Conservative default window when nothing is configured.
    public static let defaultContextWindow: Int64 = 4096
    /// Default output headroom reserved out of the window.
    public static let defaultOutputReserve: Int64 = 1024
    /// Never project to a budget below this.
    public static let budgetFloor: Int64 = 256
    /// Fixed token budget reserved at the seam for the notice WRAPPER prose
    /// (everything except the summary the strategy already pays for).
    public static let noticeEnvelopeReserve: Int64 = 80

    /// Rough token estimate for ONE message — counts the SERIALIZED form,
    /// because role + content + tool_calls + the JSON envelope all consume
    /// real context.
    public static func estimateOne(_ message: JSON) -> Int64 {
        let n = message.serialize().count
        return Int64((n + charsPerToken - 1) / charsPerToken)
    }

    /// Sum of `estimateOne` across a message slice.
    public static func estimateTokens(_ messages: [JSON]) -> Int64 {
        messages.reduce(0) { $0 + estimateOne($1) }
    }

    /// Read a positive integer meta value (number or numeric string); `nil`
    /// otherwise.
    private static func metaPosInt(_ agent: Agent, _ key: String) -> Int64? {
        guard let v = agent.metaValue(forKey: key) else { return nil }
        if case .bool = v { return nil }
        if let i = v.asInt { return i > 0 ? i : nil }
        if let s = v.asString, let i = Int64(s.trimmingCharacters(in: .whitespaces)) {
            return i > 0 ? i : nil
        }
        return nil
    }

    /// The model's usable window, by STATIC precedence (no fallback-chain):
    /// `context_window` (explicit override — works on any backend) →
    /// `num_ctx` (ollama's real knob) → a conservative default.
    public static func resolveContextWindow(_ agent: Agent) -> Int64 {
        if let v = metaPosInt(agent, "context_window") { return v }
        if let v = metaPosInt(agent, "num_ctx") { return v }
        return defaultContextWindow
    }

    /// Output headroom reserved out of the window (default 1024).
    public static func outputReserve(_ agent: Agent) -> Int64 {
        guard let v = agent.metaValue(forKey: "output_reserve") else { return defaultOutputReserve }
        if case .bool = v { return defaultOutputReserve }
        if let i = v.asInt { return i >= 0 ? i : defaultOutputReserve }
        if let s = v.asString, let i = Int64(s.trimmingCharacters(in: .whitespaces)) { return i }
        return defaultOutputReserve
    }

    /// Token budget for the INPUT (window minus output headroom), floored.
    public static func budget(_ agent: Agent) -> Int64 {
        max(resolveContextWindow(agent) - outputReserve(agent), budgetFloor)
    }

    /// The agent's configured `recent_n` (verbatim recent turns), clamped to
    /// [1, 50]; default 6.
    public static func recentN(_ agent: Agent) -> Int {
        var n: Int64 = 6
        if let v = agent.metaValue(forKey: "recent_n") {
            if let i = v.asInt { n = i } else if let s = v.asString, let i = Int64(s) { n = i }
        }
        return Int(min(max(n, 1), 50))
    }

    /// The agent's configured strategy name (default `compact`).
    public static func strategyName(_ agent: Agent) -> String {
        agent.metaValue(forKey: "context_strategy")?.asString ?? "compact"
    }
}
