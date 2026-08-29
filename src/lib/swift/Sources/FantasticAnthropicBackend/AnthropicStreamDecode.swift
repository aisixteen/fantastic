// Anthropic event-typed SSE → text pieces. The wire decode, as a PURE
// function.
//
// Mirrors the `match obj.get("type")` arm of Rust
// `fantastic-anthropic-backend::consume_sse`. It lives out here, apart from the
// HTTP/retry plumbing, because that is the only way it is testable under "unit
// tests only" — the same reason `decodeNimSSELine` does.
//
// Anthropic frames each event as an `event:` line plus a `data:` JSON line, and
// the JSON carries its own `type`, so dispatch is on that and `event:` lines are
// ignored.

import FantasticAICore
import FantasticJSON
import Foundation

/// Everything one decoded line can produce.
struct AnthropicDecoded: Equatable {
    /// Text pieces to yield as `.token`s, in order (markers included).
    var tokens: [String] = []
    /// `message_stop` arrived — stop reading.
    var done: Bool = false
    /// A `stop_reason` from `message_delta`, if this line carried one.
    var stopReason: String? = nil
    /// The stream carried an `error` event; the message it named.
    var error: String? = nil
}

/// State carried ACROSS lines within one turn.
///
/// A `tool_use` block streams its arguments as `input_json_delta` fragments, so
/// they must be concatenated and re-emitted as ONE envelope when the block
/// closes. And whether the stream CONTAINED a tool_use decides terminality —
/// see `finish(stopReason:)`.
struct AnthropicStreamState {
    var wrap = ThinkingWrap()
    /// The name of the `tool_use` block currently open, if any.
    var toolName: String? = nil
    /// Its arguments, accumulated across `input_json_delta` fragments.
    var toolArgs = ""
    /// What the stream ACTUALLY carried — the honest basis for terminality.
    var sawToolUse = false
    var stopReason: String? = nil
}

/// Decode one SSE line, folding it through `state`.
///
/// BOTH CHANNELS ARE READ. A reasoning model streams its thought as
/// `thinking_delta` and its answer as `text_delta`; reading only the latter
/// silently drops whole turns — the same failure `decodeNimSSELine` documents
/// for `reasoning_content`. The thought is WRAPPED once per turn in
/// `<think>…</think>`: parsed for tool calls, stripped before the turn persists.
func decodeAnthropicSSELine(_ line: String, state: inout AnthropicStreamState)
    -> AnthropicDecoded
{
    guard line.hasPrefix("data:") else { return AnthropicDecoded() }
    let payload = line.dropFirst(5).trimmingCharacters(in: .whitespacesAndNewlines)
    guard !payload.isEmpty,
        let data = payload.data(using: .utf8),
        let obj = try? JSON.parse(data)
    else { return AnthropicDecoded() }

    var out = AnthropicDecoded()
    switch obj["type"].asString ?? "" {
    case "content_block_start":
        // A tool_use block can arrive WHETHER OR NOT we declared tools — the
        // `/cc` route emits them for a request carrying no `tools` key at all.
        // Record what came; terminality reads that rather than our own flag.
        let cb = obj["content_block"]
        if cb["type"].asString == "tool_use" {
            state.sawToolUse = true
            state.toolName = cb["name"].asString ?? "send"
            state.toolArgs = ""
        }

    case "content_block_delta":
        let delta = obj["delta"]
        switch delta["type"].asString ?? "" {
        case "input_json_delta":
            state.toolArgs += delta["partial_json"].asString ?? ""
        case "text_delta":
            out.tokens += state.wrap.content(delta["text"].asString ?? "")
        case "thinking_delta":
            out.tokens += state.wrap.reasoning(delta["thinking"].asString ?? "")
        default:
            break
        }

    case "content_block_stop":
        // One `tool_use` block finished: re-emit it as the text envelope
        // `ai_core` parses — the same shape a raw-route model writes itself.
        // TRANSLATE, never adopt: this backend has no native tool-calling.
        if let name = state.toolName {
            state.toolName = nil
            let args: JSON =
                state.toolArgs.isEmpty
                ? .object([:])
                : ((try? JSON.parse(Data(state.toolArgs.utf8))) ?? .object([:]))
            let envelope = JSON.object(["name": .string(name), "arguments": args])
            // A thought may still be open when the model stops to call: close it
            // so the envelope is not swallowed inside `<think>`.
            out.tokens += state.wrap.finish()
            out.tokens.append("<tool_call>\(envelope.serialize())</tool_call>")
            state.toolArgs = ""
        }

    case "message_delta":
        if let reason = obj["delta"]["stop_reason"].asString {
            state.stopReason = reason
            out.stopReason = reason
        }

    case "error":
        out.error = obj["error"]["message"].asString ?? "anthropic stream error"

    case "message_stop":
        out.done = true

    default:
        break  // message_start, ping
    }
    return out
}

/// Why a finished stream is (or is not) a whole turn.
///
/// TERMINALITY IS DECIDED BY WHAT THE STREAM CONTAINED, not by what we asked
/// for. A completion cut by `max_tokens`, a refusal, or a dropped connection was
/// once returned as if the model had finished: a create_agent turn stopped
/// mid-`code`, the partial parsed as an unterminated envelope, and the chain
/// died with no visible cause. And a `/cc`-style route answers `stop_reason:
/// tool_use` for a request that declared no tools, so gating on our own flag
/// failed EVERY such turn on arrival — the completion was whole and we threw it
/// away.
func anthropicTerminalityError(_ state: AnthropicStreamState) -> String? {
    switch state.stopReason {
    case "end_turn", "stop_sequence":
        return nil
    // The model stopped to CALL something. A finished turn, not a truncated
    // one — the blocks are the answer.
    case "tool_use" where state.sawToolUse:
        return nil
    case .some(let other):
        return "anthropic stream ended without end_turn: stop_reason=\(other)"
    case .none:
        return "anthropic stream ended without end_turn: no stop_reason seen "
            + "(connection cut mid-stream)"
    }
}

/// Split ai-core's plain messages into Anthropic's top-level `system` string
/// plus the user/assistant turns.
///
/// Tool-calling is RAW prompt-and-parse, so there is NO tool_use / tool_result
/// translation on the way OUT — the envelope is already text in the turn.
/// System blocks concatenate, matching rust's `translate_messages`.
func translateAnthropicMessages(_ messages: [JSON]) -> (system: String?, messages: [JSON]) {
    var system: String? = nil
    var out: [JSON] = []
    for m in messages {
        if m["role"].asString == "system" {
            let text = m["content"].asString ?? ""
            system = system.map { $0 + "\n\n" + text } ?? text
        } else {
            out.append(m)
        }
    }
    return (system, out)
}
