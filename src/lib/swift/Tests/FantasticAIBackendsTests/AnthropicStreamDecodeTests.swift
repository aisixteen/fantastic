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

    @Test("a tool_use block is TRANSLATED into the raw <tool_call> envelope")
    func toolUseTranslated() {
        // Arguments arrive as input_json_delta FRAGMENTS and must be
        // concatenated before parsing — a per-fragment parse yields nothing.
        let (tokens, st) = decode([
            #"data: {"type":"content_block_start","content_block":{"type":"tool_use","name":"send"}}"#,
            #"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"target_id\":"}}"#,
            #"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"\"store\"}"}}"#,
            #"data: {"type":"content_block_stop"}"#,
        ])
        #expect(st.sawToolUse)
        let envelope = tokens.first { $0.hasPrefix("<tool_call>") }
        #expect(envelope != nil)
        #expect(envelope?.contains("\"target_id\"") == true)
        #expect(envelope?.contains("store") == true)
    }

    @Test("a tool_use arriving mid-thought closes the thought first")
    func toolCallNotSwallowedByThink() {
        let (tokens, _) = decode([
            #"data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"plan"}}"#,
            #"data: {"type":"content_block_start","content_block":{"type":"tool_use","name":"send"}}"#,
            #"data: {"type":"content_block_stop"}"#,
        ])
        let closeIdx = tokens.firstIndex(of: THINK_CLOSE)
        let callIdx = tokens.firstIndex { $0.hasPrefix("<tool_call>") }
        #expect(closeIdx != nil && callIdx != nil)
        // Otherwise the envelope is inside <think> and gets stripped before parse.
        #expect(closeIdx! < callIdx!)
    }

    @Test("message_stop ends the stream")
    func messageStop() {
        var st = AnthropicStreamState()
        let d = decodeAnthropicSSELine(#"data: {"type":"message_stop"}"#, state: &st)
        #expect(d.done)
    }

    @Test("an error event is surfaced, not swallowed")
    func errorEvent() {
        var st = AnthropicStreamState()
        let d = decodeAnthropicSSELine(
            #"data: {"type":"error","error":{"message":"overloaded"}}"#, state: &st)
        #expect(d.error == "overloaded")
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

    @Test("stop_reason=tool_use is a WHOLE turn when the stream carried one")
    func toolUseIsTerminal() {
        // The `/cc` route answers tool_use for a request that declared no tools.
        // Gating on our own request flag failed every such turn on arrival — the
        // completion was whole and it was thrown away.
        var st = AnthropicStreamState()
        st.stopReason = "tool_use"
        st.sawToolUse = true
        #expect(anthropicTerminalityError(st) == nil)
    }

    @Test("max_tokens is a TRUNCATED turn and must be loud")
    func truncatedIsLoud() {
        var st = AnthropicStreamState()
        st.stopReason = "max_tokens"
        #expect(anthropicTerminalityError(st)?.contains("max_tokens") == true)
    }

    @Test("no stop_reason at all means the connection was cut")
    func noStopReason() {
        #expect(anthropicTerminalityError(AnthropicStreamState())?.contains("cut") == true)
    }

    // ── message translation ────────────────────────────────────────────────

    @Test("system messages lift to the top level and concatenate")
    func systemLifted() {
        let (system, turns) = translateAnthropicMessages([
            .object(["role": .string("system"), "content": .string("one")]),
            .object(["role": .string("system"), "content": .string("two")]),
            .object(["role": .string("user"), "content": .string("hi")]),
        ])
        #expect(system == "one\n\ntwo")
        #expect(turns.count == 1)
        #expect(turns[0]["role"].asString == "user")
    }
}
