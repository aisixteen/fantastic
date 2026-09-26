// The OpenAI-compatible SSE line decode — shared, because more than one
// backend speaks this wire.
//
// NVIDIA NIM and `llama-server` both expose `/v1/chat/completions` with the
// same streaming shape, so the decode is a property of the WIRE, not of either
// vendor. It lived inside the NIM backend until llama.cpp needed it; copying it
// would have meant two decoders drifting apart, and the one thing this function
// exists to get right — reading BOTH channels — is exactly the kind of detail
// that gets fixed in one copy and not the other.

import FantasticJSON
import Foundation

/// One decoded SSE line: the text pieces to stream (in order), and whether the
/// `[DONE]` sentinel arrived.
public struct OpenAIDecoded: Equatable, Sendable {
    /// Text pieces to yield as tokens, markers included, in order.
    public let tokens: [String]
    /// The `data: [DONE]` sentinel — stop reading.
    public let done: Bool

    public init(tokens: [String], done: Bool) {
        self.tokens = tokens
        self.done = done
    }
}

/// Decode one `data:` SSE line, folding its two channels through `wrap`.
///
/// REASONING MODELS SPLIT THEIR OUTPUT. nemotron/deepseek-style models — and
/// `llama-server` started with `--reasoning-format deepseek` — stream most
/// tokens as `reasoning_content` and only the conclusion as `content`: measured
/// 740 chars of reasoning to 129 of content on one NIM turn, and 602 to 176 on
/// gemma-4-12B. Reading `content` alone silently drops whole turns; observed
/// live, a model spent an entire pass reasoning, an EMPTY completion came back,
/// and the chain died with no error anywhere. A raw text protocol reads the text
/// the model produced — it does not let the provider decide which field counts
/// as output. WRAPPED once per turn: parsed for tool calls, stripped before the
/// turn is persisted.
///
/// The prefix test is `data:` and the payload is then trimmed. Requiring the
/// space (`"data: "`) would silently drop a compliant `data:{"choices":…}`
/// line, which is the same class of bug as reading one channel. Non-`data:`
/// lines (SSE comments, `event:`, blanks) and unparseable payloads carry no
/// model text, so they are skipped.
public func decodeOpenAISSELine(_ line: String, wrap: inout ThinkingWrap) -> OpenAIDecoded {
    let none = OpenAIDecoded(tokens: [], done: false)
    guard line.hasPrefix("data:") else { return none }
    let payload = line.dropFirst(5).trimmingCharacters(in: .whitespacesAndNewlines)
    if payload == "[DONE]" { return OpenAIDecoded(tokens: [], done: true) }
    guard !payload.isEmpty,
        let data = payload.data(using: .utf8),
        let parsed = try? JSON.parse(data),
        let choices = parsed["choices"].asArray,
        !choices.isEmpty
    else { return none }
    let delta = choices[0]["delta"]
    var tokens: [String] = []
    tokens += wrap.reasoning(delta["reasoning_content"].asString ?? "")
    tokens += wrap.content(delta["content"].asString ?? "")
    return OpenAIDecoded(tokens: tokens, done: false)
}

/// What ONE `data:` line REPORTS about the pass, beside its text: the prompt
/// size the endpoint counted (`usage.prompt_tokens`, on the last chunk when the
/// request asked `stream_options.include_usage`) and why the stream ended
/// (`choices[0].finish_reason`, `length` → `max_tokens`). `nil` fields: not on
/// this line. Python `NvidiaNimProvider.chat` / `LlamaCppProvider.chat`.
public func openAIReport(_ line: String) -> (promptTokens: Int?, stopReason: String?) {
    guard line.hasPrefix("data:") else { return (nil, nil) }
    let payload = line.dropFirst(5).trimmingCharacters(in: .whitespacesAndNewlines)
    guard !payload.isEmpty, payload != "[DONE]", let data = payload.data(using: .utf8),
        let parsed = try? JSON.parse(data)
    else { return (nil, nil) }
    var tokens: Int? = nil
    if case .object = parsed["usage"], pyTruthy(parsed["usage"]["prompt_tokens"]),
        let n = parsed["usage"]["prompt_tokens"].asInt
    {
        tokens = Int(n)
    }
    var stop: String? = nil
    if let choices = parsed["choices"].asArray, !choices.isEmpty,
        pyTruthy(choices[0]["finish_reason"])
    {
        let fr = pyStr(choices[0]["finish_reason"])
        stop = fr == "length" ? "max_tokens" : fr
    }
    return (tokens, stop)
}

/// Record what a line reported onto the provider's usage report.
public func recordOpenAIReport(_ line: String, into usage: AIUsage) {
    let (tokens, stop) = openAIReport(line)
    if let tokens { usage.lastPromptTokens = tokens }
    if let stop { usage.lastStopReason = stop }
}
