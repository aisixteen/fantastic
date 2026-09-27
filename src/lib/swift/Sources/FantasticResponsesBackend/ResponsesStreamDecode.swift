// Responses event-typed SSE → text pieces. The wire decode, as a PURE function —
// the only way it is testable under "unit tests only", apart from the HTTP/retry
// plumbing (the same reason `decodeAnthropicSSELine` lives on its own).
//
// Each event is an `event:` line plus a `data:` JSON line whose own `type` is what
// dispatch reads; `event:` lines are ignored.

import FantasticAICore
import FantasticJSON
import Foundation

/// Everything one decoded line can produce.
struct ResponsesDecoded: Equatable {
    /// Text pieces to yield as `.token`s, in order (markers included).
    var tokens: [String] = []
    /// The terminal event arrived — stop reading.
    var done: Bool = false
    /// The stream failed (`response.failed`, `error`, a native `function_call`).
    var error: String? = nil
    /// The terminal response's reported `usage.input_tokens` (`nil`: none).
    var promptTokens: Int? = nil
}

/// State carried ACROSS lines within one turn.
struct ResponsesStreamState {
    var wrap = ThinkingWrap()
    /// The terminal response object (`response.completed` / `.incomplete`) — `nil`
    /// until one arrives. A stream that ends without one did not finish.
    var finalResponse: JSON? = nil
    /// Every `response.output_item.added` item's own `type`, in order, RAW (a
    /// missing type is `None`) — python's `kinds`.
    var kinds: [JSON] = []
}

/// Reasoning deltas: raw chain of thought (an open-weights model behind a proxy) or
/// OpenAI's own summary — both the thought channel, wrapped, never output.
private let reasoningDeltas: Set<String> = [
    "response.reasoning_text.delta", "response.reasoning_summary_text.delta",
]

/// Output deltas. A refusal is text the model produced; dropping it would return a
/// refused turn as an EMPTY completion.
private let textDeltas: Set<String> = ["response.output_text.delta", "response.refusal.delta"]

/// Decode one SSE line, folding it through `state`.
func decodeResponsesSSELine(_ line: String, state: inout ResponsesStreamState)
    -> ResponsesDecoded
{
    guard line.hasPrefix("data:") else { return ResponsesDecoded() }
    let payload = line.dropFirst(5).trimmingCharacters(in: .whitespacesAndNewlines)
    guard !payload.isEmpty,
        let data = payload.data(using: .utf8),
        let obj = try? JSON.parse(data)
    else { return ResponsesDecoded() }

    var out = ResponsesDecoded()
    let etype = obj["type"].asString ?? ""
    if reasoningDeltas.contains(etype) {
        // BOTH CHANNELS ARE READ: a turn spent thinking would otherwise arrive
        // EMPTY, and a `<call>` written mid-thought would be lost with it.
        out.tokens += state.wrap.reasoning(obj["delta"].asString ?? "")
        return out
    }
    if textDeltas.contains(etype) {
        out.tokens += state.wrap.content(obj["delta"].asString ?? "")
        return out
    }
    switch etype {
    case "response.output_item.added":
        // A NATIVE function_call ITEM IS A LOUD ERROR — JSON arguments this kernel
        // does not read; a silent drop would end the turn with the call lost.
        let item = obj["item"]
        state.kinds.append(item["type"])
        if item["type"].asString == "function_call" {
            out.error =
                "responses stream carried a native function_call item (\(pyRepr(item["name"]))): "
                + "the model wrote a JSON tool call, which this kernel does not parse "
                + "— calls are `<call name=…>` text"
        }

    case "response.completed", "response.incomplete":
        let resp = pyTruthy(obj["response"]) ? obj["response"] : .object([:])
        state.finalResponse = resp
        // PROVIDER-REPORTED USAGE — what the window really held.
        let n = resp["usage"]["input_tokens"].asInt ?? 0
        out.promptTokens = n != 0 ? Int(n) : nil
        out.done = true

    case "response.failed":
        out.error = "responses stream failed: \(pyStr(obj["response"]["error"]))"

    case "error":
        out.error = "responses stream error: \(pyStr(obj))"

    default:
        break  // created / in_progress / *.done / content_part.*
    }
    return out
}

/// python `"max_tokens" if reason == "max_output_tokens" else (reason or status)`.
func responsesStopReason(_ state: ResponsesStreamState) -> String? {
    let resp = state.finalResponse ?? .null
    let status = resp["status"]
    let reason = resp["incomplete_details"]["reason"]
    if reason.asString == "max_output_tokens" { return "max_tokens" }
    if pyTruthy(reason) { return pyStr(reason) }
    if pyTruthy(status) { return pyStr(status) }
    return nil
}

/// Why a finished stream is (or is not) a whole turn: only `completed` is. A cut
/// returned as finished parses as a truncated envelope and the chain dies
/// downstream with the cause hidden.
func responsesTerminalityError(_ state: ResponsesStreamState) -> String? {
    let resp = state.finalResponse ?? .null
    let status = resp["status"]
    if status.asString == "completed" { return nil }
    let reason = resp["incomplete_details"]["reason"]
    return "responses stream ended without completing: "
        + "status=\(pyRepr(status)) reason=\(pyRepr(reason)) items=\(pyRepr(JSON.array(state.kinds)))"
        + (state.finalResponse == nil ? " (no terminal event — connection cut mid-stream)" : "")
}

/// Split ai-core's plain messages into `instructions` plus typed input items —
/// python `_to_responses`. The LAST system message wins; an empty user/assistant
/// content is `(no content)`; every other role is dropped. Tool calls are already
/// inline text, so there is no function_call translation in either direction.
func translateResponsesMessages(_ messages: [JSON]) -> (instructions: String?, items: [JSON]) {
    var instructions: String? = nil
    var items: [JSON] = []
    for m in messages {
        let role = m["role"].asString
        let content = pyTruthy(m["content"]) ? (m["content"].asString ?? "") : ""
        if role == "system" {
            instructions = content
        } else if role == "user" || role == "assistant" {
            let part = role == "user" ? "input_text" : "output_text"
            items.append(
                .object([
                    "type": .string("message"),
                    "role": .string(role!),
                    "content": .array([
                        .object([
                            "type": .string(part),
                            "text": .string(content.isEmpty ? "(no content)" : content),
                        ])
                    ]),
                ]))
        }
    }
    return (instructions, items)
}
