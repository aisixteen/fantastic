// The Anthropic wire decode, pinned as a pure function.
//
// These exist for the reason `NimStreamDecodeTests` exists: reading one field
// instead of two is exactly how a backend ships broken, and the HTTP plumbing
// around it cannot be unit-tested. Every case here is a line the real API (or
// the `/cc` proxy in front of it) actually emits.

import FantasticAICore
import FantasticJSON
import Testing

@testable import FantasticAnthropicBackend

private func decode(_ lines: [String]) -> (tokens: [String], state: AnthropicStreamState) {
    var st = AnthropicStreamState()
    var out: [String] = []
    for l in lines { out += decodeAnthropicSSELine(l, state: &st).tokens }
    return (out, st)
}

/// Same fold, keeping the first `error` the stream produced.
private func decodeWithError(_ lines: [String]) -> (
    tokens: [String], state: AnthropicStreamState, error: String?
) {
    var st = AnthropicStreamState()
    var out: [String] = []
    var err: String? = nil
    for l in lines {
        let d = decodeAnthropicSSELine(l, state: &st)
        out += d.tokens
        if err == nil { err = d.error }
    }
    return (out, st, err)
}

@Suite("Anthropic SSE decode")
struct AnthropicStreamDecodeTests {

    @Test("text_delta streams through as bare tokens")
    func textDelta() {
        let (tokens, _) = decode([
            #"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hel"}}"#,
            #"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"lo"}}"#,
        ])
        #expect(tokens == ["hel", "lo"])
    }

    @Test("thinking_delta is WRAPPED once, not per chunk")
    func thinkingWrappedOnce() {
        // A reasoning model splits its output. Reading only `text_delta` drops
        // whole turns; wrapping per-chunk produces unbalanced markers.
        let (tokens, _) = decode([
            #"data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"a"}}"#,
            #"data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"b"}}"#,
            #"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"answer"}}"#,
        ])
        #expect(tokens.filter { $0 == THINK_OPEN }.count == 1)
        #expect(tokens.filter { $0 == THINK_CLOSE }.count == 1)
        #expect(tokens.last == "answer")
    }

    @Test("a thought that never closes is balanced by finish()")
    func unclosedThoughtBalanced() {
        var st = AnthropicStreamState()
        _ = decodeAnthropicSSELine(
            #"data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"x"}}"#,
            state: &st)
        #expect(st.wrap.finish() == [THINK_CLOSE])
    }

    @Test("a native tool_use block is a LOUD error, not a silent drop")
    func toolUseIsLoud() {
        // Some `/cc`-style routes emit one for a request carrying no `tools` key
        // at all. Its arguments are JSON this kernel does not read, so a dropped
        // block ends the turn looking finished with the call lost.
        let (_, _, err) = decodeWithError([
            #"data: {"type":"content_block_start","content_block":{"type":"tool_use","name":"send"}}"#
        ])
        // B4.1: `{cb.get('name')!r}` — python's repr, single quotes.
        #expect(
            err
                == "anthropic stream carried a native tool_use block ('send'): the model "
                + "wrote a JSON tool call, which this kernel does not parse — calls are "
                + "`<call name=…>` text")
        // A block with no name reads `None`, not a placeholder of our own.
        let (_, _, nameless) = decodeWithError([
            #"data: {"type":"content_block_start","content_block":{"type":"tool_use"}}"#
        ])
        #expect(nameless?.contains("tool_use block (None):") == true)
    }

    @Test("message_stop ends the stream")
    func messageStop() {
        var st = AnthropicStreamState()
        let d = decodeAnthropicSSELine(#"data: {"type":"message_stop"}"#, state: &st)
        #expect(d.done)
    }

    // B4.1: `f"anthropic stream error: {err}"` over the WHOLE error object — the
    // python dict repr, type and all. Reading `.message` alone dropped the type.
    @Test("an error event is surfaced whole, python's way")
    func errorEvent() {
        var st = AnthropicStreamState()
        let d = decodeAnthropicSSELine(
            #"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            state: &st)
        #expect(
            d.error
                == "anthropic stream error: "
                + "{'type': 'overloaded_error', 'message': 'Overloaded'}")
        // `evt.get("error") or {}` — a missing object is the empty dict.
        var bare = AnthropicStreamState()
        let e = decodeAnthropicSSELine(#"data: {"type":"error"}"#, state: &bare)
        #expect(e.error == "anthropic stream error: {}")
    }

    @Test("non-data lines and junk carry no model text")
    func ignoresNonData() {
        let (tokens, _) = decode(["event: content_block_delta", "", ": ping", "data: {oops"])
        #expect(tokens.isEmpty)
    }

    // ── terminality ────────────────────────────────────────────────────────

    @Test("end_turn and stop_sequence are whole turns")
    func terminalOK() {
        for reason in ["end_turn", "stop_sequence"] {
            var st = AnthropicStreamState()
            st.stopReason = reason
            #expect(anthropicTerminalityError(st) == nil)
        }
    }

    // B4.1: `tool_use` is terminal ONLY when such a block was seen — python
    // `test_tool_use_stop_reason_without_a_tool_use_block_is_an_anomaly`.
    @Test("stop_reason=tool_use with no tool_use block is an anomaly and is loud")
    func toolUseIsAnAnomaly() {
        var st = AnthropicStreamState()
        let text = #"data: {"type":"content_block_start","content_block":{"type":"text"}}"#
        _ = decodeAnthropicSSELine(text, state: &st)
        st.stopReason = "tool_use"
        #expect(
            anthropicTerminalityError(st)
                == "anthropic stream ended without end_turn: "
                + "stop_reason='tool_use' blocks=['text']")
    }

    @Test("max_tokens is a TRUNCATED turn and must be loud, with the blocks it held")
    func truncatedIsLoud() {
        var st = AnthropicStreamState()
        let start = #"data: {"type":"content_block_start","content_block":{"type":"thinking"}}"#
        let delta = #"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#
        _ = decodeAnthropicSSELine(start, state: &st)
        _ = decodeAnthropicSSELine(delta, state: &st)
        #expect(
            anthropicTerminalityError(st)
                == "anthropic stream ended without end_turn: stop_reason='max_tokens' "
                + "blocks=['thinking']")
    }

    @Test("no stop_reason at all means the connection was cut")
    func noStopReason() {
        #expect(
            anthropicTerminalityError(AnthropicStreamState())
                == "anthropic stream ended without end_turn: stop_reason=None blocks=[] "
                + "(none seen — connection cut mid-stream)")
    }

    // ── message translation ────────────────────────────────────────────────

    // B1.8: python `_to_anthropic`. The LAST system message wins (they do not
    // concatenate), an empty user/assistant content is sent as `(no content)`,
    // and every other role is DROPPED.
    @Test("to_anthropic matches python")
    func toAnthropicMatchesPython() {
        let (system, turns) = translateAnthropicMessages([
            .object(["role": .string("system"), "content": .string("A")]),
            .object(["role": .string("user"), "content": .string("")]),
            .object(["role": .string("assistant"), "content": .null]),
            .object(["role": .string("tool"), "content": .string("x")]),
            .object(["role": .string("user"), "content": .string("hi")]),
        ])
        #expect(system == "A")
        #expect(turns.count == 3)
        #expect(turns[0] == .object(["role": .string("user"), "content": .string("(no content)")]))
        #expect(
            turns[1] == .object(["role": .string("assistant"), "content": .string("(no content)")])
        )
        #expect(turns[2] == .object(["role": .string("user"), "content": .string("hi")]))
    }

    @Test("a later system message overrides the earlier one")
    func lastSystemWins() {
        let (system, _) = translateAnthropicMessages([
            .object(["role": .string("system"), "content": .string("A")]),
            .object(["role": .string("system"), "content": .string("")]),
            .object(["role": .string("user"), "content": .string("hi")]),
        ])
        #expect(system == "")
    }
}
