// NIM Server-Sent-Events line → text pieces. The wire decode, as a PURE
// function.
//
// Mirrors the body of Python
// `nvidia_nim_backend.provider.NvidiaNimProvider.chat`'s `async for`
// (provider.py:61-101). It lives out here, apart from the HTTP/429 plumbing,
// because that is the only way it is testable under "unit tests only" — and
// reading one field instead of two is exactly how it shipped broken.

import FantasticAICore
import FantasticJSON

/// One decoded SSE line: the text pieces to stream (in order), and whether the
/// `[DONE]` sentinel arrived.
struct NimDecoded: Equatable {
    /// Text pieces to yield as `.token`s, markers included, in order.
    let tokens: [String]
    /// The `data: [DONE]` sentinel — stop reading.
    let done: Bool
}

/// Decode one `data:` SSE line, folding its two channels through `wrap`.
///
/// REASONING MODELS SPLIT THEIR OUTPUT. nemotron/deepseek-style NIM models
/// stream most tokens as `reasoning_content` and only the conclusion as
/// `content` — measured 740 chars of reasoning to 129 of content on one turn.
/// Reading `content` alone silently drops whole turns: observed live, the model
/// spent an entire pass reasoning, we saw an EMPTY completion, and the chain died
/// with no error anywhere. A raw text protocol reads the text the model produced;
/// it does not let the provider decide which field counts as output. WRAPPED
/// once per turn (see the ollama backend + `FantasticAICore.withoutThinking`):
/// parsed for tool calls, stripped before the turn is persisted.
///
/// The prefix test is `data:` and the payload is then trimmed — matching python.
/// Requiring the space (`"data: "`) would silently drop a compliant
/// `data:{"choices":…}` line, which is the same class of bug as reading one
/// channel. Non-`data:` lines (SSE comments, `event:`, blanks) and unparseable
/// payloads carry no model text, so they are skipped.
func decodeNimSSELine(_ line: String, wrap: inout ThinkingWrap) -> NimDecoded {
    let none = NimDecoded(tokens: [], done: false)
    guard line.hasPrefix("data:") else { return none }
    let payload = line.dropFirst(5).trimmingCharacters(in: .whitespacesAndNewlines)
    if payload == "[DONE]" { return NimDecoded(tokens: [], done: true) }
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
    return NimDecoded(tokens: tokens, done: false)
}
