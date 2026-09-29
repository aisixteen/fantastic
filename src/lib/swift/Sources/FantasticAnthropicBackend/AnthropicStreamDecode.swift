// Anthropic event-typed SSE → text pieces. The wire decode, as a PURE
// function.
//
// Mirrors the `match obj.get("type")` arm of Rust
// `fantastic-anthropic-backend::consume_sse`. It lives out here, apart from the
// HTTP/retry plumbing, because that is the only way it is testable under "unit
// tests only" — the same reason `decodeOpenAISSELine` does.
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
    /// `message_start`'s PROVIDER-REPORTED USAGE — what the context window really
    /// held, cache reads included (`nil` when the event reports nothing).
    var promptTokens: Int? = nil
}

/// State carried ACROSS lines within one turn.
struct AnthropicStreamState {
    var wrap = ThinkingWrap()
    var stopReason: String? = nil
    /// WHAT THE STREAM ACTUALLY CONTAINED — every `content_block_start`'s own
    /// `type`, in order, RAW (a block with no type is `None`, exactly as
    /// python's `blocks.append(cb.get("type"))` records it). A bare
    /// `stop_reason` names the symptom and nothing else; the block types are the
    /// one fact that separates "the model wrote a native tool block" from "the
    /// connection died before any content".
    var blocks: [JSON] = []
}

/// Decode one SSE line, folding it through `state`.
///
/// BOTH CHANNELS ARE READ. A reasoning model streams its thought as
/// `thinking_delta` and its answer as `text_delta`; reading only the latter
/// silently drops whole turns — the same failure `decodeOpenAISSELine` documents
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
        // A NATIVE tool_use BLOCK IS A LOUD ERROR. Its arguments are JSON we do
        // not read: the model wrote a call in a grammar this kernel does not
        // speak, and a silent drop would end the turn looking finished with the
        // call lost. A `/cc`-style route emits these for a request carrying no
        // `tools` key at all — that is exactly the case worth failing on.
        let cb = obj["content_block"]
        state.blocks.append(cb["type"])
        if cb["type"].asString == "tool_use" {
            // `{cb.get('name')!r}` — python's repr, so an absent name reads
            // `None` and a quoted one keeps its quotes.
            out.error =
                "anthropic stream carried a native tool_use block (\(pyRepr(cb["name"]))): "
                + "the model wrote a JSON tool call, which this kernel does not "
                + "parse — calls are `<call name=…>` text"
        }

    case "content_block_delta":
        let delta = obj["delta"]
        switch delta["type"].asString ?? "" {
        case "text_delta":
            out.tokens += state.wrap.content(delta["text"].asString ?? "")
        case "thinking_delta":
            out.tokens += state.wrap.reasoning(delta["thinking"].asString ?? "")
        default:
            break
        }

    case "message_start":
        // PROVIDER-REPORTED USAGE — what the context window really held, cache
        // reads included. ai_core calibrates its own estimate with it.
        let u = obj["message"]["usage"]
        var n: Int64 = 0
        for k in ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"] {
            n += pyTruthy(u[k]) ? (u[k].asInt ?? 0) : 0
        }
        out.promptTokens = n > 0 ? Int(n) : nil

    case "message_delta":
        if let reason = obj["delta"]["stop_reason"].asString {
            state.stopReason = reason
            out.stopReason = reason
        }

    case "error":
        // THE WHOLE ERROR OBJECT, python's way: `f"anthropic stream error: {err}"`
        // over `evt.get("error") or {}`. Reading `.message` alone threw away the
        // type and every other field the endpoint sent, and answered a bare
        // sentence of our own when the object carried no message at all.
        out.error =
            "anthropic stream error: "
            + pyStr(pyTruthy(obj["error"]) ? obj["error"] : .object([:]))

    case "message_stop":
        out.done = true

    default:
        break  // content_block_stop, ping
    }
    return out
}

/// Why a finished stream is (or is not) a whole turn.
///
/// TERMINALITY IS DECIDED BY WHAT THE STREAM CONTAINED, not by what we asked
/// for. A completion cut by `max_tokens`, a refusal, or a dropped connection was
/// once returned as if the model had finished: a create_agent turn stopped
/// mid-`code`, the partial parsed as an unterminated envelope, and the chain
/// died with no visible cause. A `/cc`-style route answers `stop_reason:
/// tool_use` for a request that declared no tools — those blocks now raise in
/// the decoder, so a `stop_reason` of `tool_use` reaching here is an anomaly
/// and says so.
func anthropicTerminalityError(_ state: AnthropicStreamState) -> String? {
    // TERMINAL ON WHAT THE STREAM CONTAINED. A turn that ended BECAUSE the model
    // called tools is a complete turn — but only when such a block was actually
    // seen; `tool_use` with no such block means the stream did not carry what
    // its stop_reason claims. (Python builds the same tuple.)
    var terminal = ["end_turn", "stop_sequence"]
    if state.blocks.contains(where: { $0.asString == "tool_use" }) { terminal.append("tool_use") }
    if let reason = state.stopReason, terminal.contains(reason) { return nil }
    let blocks = JSON.array(state.blocks)
    return "anthropic stream ended without end_turn: "
        + "stop_reason=\(pyRepr(state.stopReason.map(JSON.string) ?? .null)) "
        + "blocks=\(pyRepr(blocks))"
        + (state.stopReason == nil ? " (none seen — connection cut mid-stream)" : "")
}

/// Split ai-core's plain messages into Anthropic's top-level `system` string
/// plus the user/assistant turns — python `_to_anthropic`.
///
/// Tool-calling is RAW prompt-and-parse, so there is NO tool_use / tool_result
/// translation in EITHER direction — the envelope is already text in the turn.
///
/// THREE RULES, all of them python's and none of them what this did before: the
/// LAST system message wins (they do not concatenate), a user or assistant turn
/// whose content is empty or null is sent as `(no content)` rather than as an
/// empty string the API rejects, and every OTHER role is DROPPED instead of
/// riding through untranslated.
func translateAnthropicMessages(_ messages: [JSON]) -> (system: String?, messages: [JSON]) {
    var system: String? = nil
    var out: [JSON] = []
    for m in messages {
        let role = m["role"].asString
        // `m.get("content", "") or ""` — null, missing and false all read empty.
        let content = pyTruthy(m["content"]) ? (m["content"].asString ?? "") : ""
        if role == "system" {
            system = content
        } else if role == "user" || role == "assistant" {
            out.append(
                .object([
                    "role": .string(role!),
                    "content": .string(content.isEmpty ? "(no content)" : content),
                ]))
        }
    }
    return (system, out)
}
