// Thinking-channel tests — the `<think>…</think>` convention that lets a
// reasoning model's turn be PARSED in full and PERSISTED stripped.
//
// Mirrors the Python `ai_core.core` unit (`without_thinking`) and the
// once-per-turn wrapping every provider does around its reasoning channel.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

@Suite("thinking: parsed, not persisted")
struct ThinkingTests {

    @Test func stripsASpanAndKeepsSurroundingText() {
        #expect(withoutThinking("before <think>deliberation</think>after") == "before after")
        // No span at all: the text is returned as-is (trimmed).
        #expect(withoutThinking("  plain answer\n") == "plain answer")
        // Several spans, all dropped.
        #expect(withoutThinking("a<think>one</think>b<think>two</think>c") == "abc")
    }

    @Test func aToolCallInsideASpanSurvivesIntoHistory() {
        // The RAW text is parsed (the call dispatches) and the STRIPPED text is
        // persisted — so a call the model wrote mid-thought stays in the record.
        //
        // It used to be stripped with the reasoning, and the consequence was a
        // record we had silently edited: the call RAN, returned a reply, and
        // left no cause behind it. The model then read an effect it never
        // caused. Reasoning is still dropped; the INVOCATION is not.
        let call =
            #"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"reflect"}}}</tool_call>"#
        let raw = "<think>I should check the tree first. \(call)</think>Checking."
        let calls = extractToolCalls(raw)
        #expect(calls.count == 1)
        #expect(calls[0].arguments["target_id"].asString == "core")

        let kept = withoutThinking(raw)
        #expect(kept.contains(call), "the invocation was erased: \(kept)")
        #expect(kept.contains("Checking."))
        #expect(!kept.contains("I should check"), "the reasoning must still go")
        // ...and what we persisted still parses to the same one call, so history
        // and dispatch agree.
        #expect(extractToolCalls(kept).count == 1)
    }

    @Test func anUnterminatedSpanDropsItsTail() {
        // The stream was cut mid-thought. Keeping the tail would persist a
        // half-thought AND leak an unbalanced marker into the conversation.
        #expect(withoutThinking("answer so far <think>still reasoning") == "answer so far")
        #expect(withoutThinking("<think>only ever thought about it") == "")
    }

    @Test func wrapOpensAndClosesOnceForManyReasoningChunks() {
        // 3 reasoning chunks + 1 content chunk = exactly ONE open and ONE close.
        // Per-chunk wrapping would make the text a confetti of tags, and every
        // stray marker is text the parser has to survive.
        var wrap = ThinkingWrap()
        var out: [String] = []
        out += wrap.reasoning("first ")
        out += wrap.reasoning("second ")
        out += wrap.reasoning("third")
        out += wrap.content("the answer")
        out += wrap.finish()
        let text = out.joined()
        #expect(text == "<think>first second third</think>the answer")
        #expect(text.components(separatedBy: THINK_OPEN).count - 1 == 1)
        #expect(text.components(separatedBy: THINK_CLOSE).count - 1 == 1)
        #expect(withoutThinking(text) == "the answer")
    }

    @Test func wrapClosesAThinkingOnlyTurnAtStreamEnd() {
        // A turn spent ENTIRELY reasoning: the model never emitted content, so
        // nothing closes the span except the end of the stream. Without this the
        // text is unbalanced and the whole turn looks like an empty completion.
        var wrap = ThinkingWrap()
        var out: [String] = []
        out += wrap.reasoning("all of it")
        out += wrap.finish()
        #expect(out.joined() == "<think>all of it</think>")
        // Nothing is durable — but the RAW text was there to be parsed.
        #expect(withoutThinking(out.joined()) == "")
    }

    @Test func wrapEmitsNoMarkersForAContentOnlyTurn() {
        // A non-reasoning model must be untouched: no markers, no wrapping.
        var wrap = ThinkingWrap()
        var out: [String] = []
        out += wrap.content("Hel")
        out += wrap.content("lo")
        out += wrap.finish()
        #expect(out == ["Hel", "lo"])
    }

    @Test func wrapReopensForAsecondThoughtAfterContent() {
        // Interleaved channels (content, then more reasoning) stay balanced:
        // each span opens and closes exactly once.
        var wrap = ThinkingWrap()
        var out: [String] = []
        out += wrap.reasoning("a")
        out += wrap.content("X")
        out += wrap.reasoning("b")
        out += wrap.content("Y")
        out += wrap.finish()
        #expect(out.joined() == "<think>a</think>X<think>b</think>Y")
        #expect(withoutThinking(out.joined()) == "XY")
    }

    @Test func wrapIgnoresEmptyPieces() {
        // Providers hand through whatever the wire gave them (ollama sends empty
        // `content` on nearly every chunk, and an explicit `null` sometimes). An
        // empty piece must not open a span or emit a stray marker.
        var wrap = ThinkingWrap()
        var out: [String] = []
        out += wrap.reasoning("")
        out += wrap.content("")
        out += wrap.finish()
        #expect(out.isEmpty)
    }
}
