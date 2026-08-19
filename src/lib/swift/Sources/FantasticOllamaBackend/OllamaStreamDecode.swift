// Ollama NDJSON line → text pieces. The wire decode, as a PURE function.
//
// Mirrors the body of Python `ollama_backend.provider.OllamaProvider.chat`'s
// `async for` (provider.py:95-111). It lives out here, apart from the HTTP
// plumbing, because that is the only way it is testable under "unit tests only"
// — and reading one field instead of two is exactly how it shipped broken: the
// backend requested thinking and then read `message.content` alone, so a
// reasoning model's whole turn arrived as an EMPTY completion.

import FantasticAICore
import FantasticJSON

/// One decoded NDJSON line: the text pieces to stream (in order), and whether
/// ollama marked the stream finished.
struct OllamaDecoded: Equatable {
    /// Text pieces to yield as `.token`s, markers included, in order.
    let tokens: [String]
    /// `done:true` on the line — stop reading.
    let done: Bool
}

/// Decode one ollama NDJSON line, folding its two channels through `wrap`.
///
/// THINKING MODELS SPLIT THEIR OUTPUT — read every text field, not just one.
/// ollama streams a reasoning model's tokens as `message.thinking` and only its
/// conclusion as `message.content`. Measured on gemma4:12b: 336 chars of
/// thinking to 38 of content. Reading `content` alone silently drops whole
/// turns — observed live on 2026-08-02, where gemma spent all 578 generated
/// tokens reasoning, we saw an EMPTY completion, and the chain ended at pass one
/// with no tool call and no error. ollama's own log showed the request answered
/// normally, so it looked like the server had wedged.
///
/// Same defect and same fix as NIM's `reasoning_content`. A raw text protocol
/// reads the text the model produced; it does not let the provider decide which
/// of its fields counts as output. (The `?? ""` also normalizes ollama's
/// occasional explicit `null` — protocol shape, not bug masking.) The reasoning
/// is WRAPPED, once per turn, so ai-core can parse it but not persist it (see
/// `FantasticAICore.withoutThinking`); `ThinkingWrap` owns the once-ness.
///
/// An unparseable line is skipped, not an error: ollama's stream is NDJSON and a
/// non-JSON line carries no model text to lose.
func decodeOllamaLine(_ line: String, wrap: inout ThinkingWrap) -> OllamaDecoded {
    guard let data = line.data(using: .utf8), let parsed = try? JSON.parse(data) else {
        return OllamaDecoded(tokens: [], done: false)
    }
    let message = parsed["message"]
    var tokens: [String] = []
    tokens += wrap.reasoning(message["thinking"].asString ?? "")
    tokens += wrap.content(message["content"].asString ?? "")
    return OllamaDecoded(tokens: tokens, done: parsed["done"].asBool == true)
}
