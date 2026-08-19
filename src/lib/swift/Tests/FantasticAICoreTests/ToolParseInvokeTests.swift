// Claude's own envelope: `<invoke name="…"><parameter name="…">…</parameter></invoke>`.
//
// Mirrors the canonical Python `parse_invoke`. Python's suite exercises this
// envelope only through the interleave and malformed cases, so every fixture
// below was run against the python implementation and its expectations copied
// from what python actually produced — including object KEY ORDER, which is why
// the assertions compare serialized bytes.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticAICore

@Suite("tool-call parser: Claude's own <invoke> envelope")
struct ToolParseInvokeTests {

    @Test func theShapeThatDroppedWholeRunsAtPassOne() {
        // Claude models routinely emit their own wrapper instead of the taught one,
        // with the arguments perfectly correct. Refusing it dropped whole runs at
        // pass 1 (observed on claude-haiku-4.5), after which the model — never
        // receiving a reply — invented the entire remaining session. The anthropic
        // backends are the exposure.
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="arguments">"#
                + #"{"target_id":"notes","payload":{"type":"reflect"},"reply_to":"self"}"#
                + "</parameter></invoke>")[0]
        #expect(call.name == "send")
        #expect(
            call.arguments.serialize()
                == #"{"target_id":"notes","payload":{"type":"reflect"},"reply_to":"self"}"#)
        #expect(call.id.hasPrefix("call_"))
    }

    @Test func splitParametersAreArgumentsToo() {
        // Models split the arguments either way: one `arguments` blob, or one
        // `<parameter>` per key. Each split value is JSON-decoded so objects and
        // ints survive, and text that is not JSON (`notes`) is kept as text —
        // `self` is not JSON either, but it is exactly what `reply_to` wants.
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="target_id">notes</parameter>"#
                + #"<parameter name="payload">{"type":"reflect"}</parameter>"#
                + #"<parameter name="count">3</parameter></invoke>"#)[0]
        #expect(
            call.arguments.serialize()
                == #"{"target_id":"notes","payload":{"type":"reflect"},"count":3}"#)
    }

    @Test func aBlobAndSplitParametersMerge() {
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="arguments">{"target_id":"a"}</parameter>"#
                + #"<parameter name="reply_to">self</parameter></invoke>"#)[0]
        #expect(call.arguments.serialize() == #"{"target_id":"a","reply_to":"self"}"#)
    }

    @Test func aSplitParameterOverwritesTheBlob() {
        // The opposite precedence from the taught envelope, where `arguments` is the
        // nested slot and a top-level stray is the model's slip. Here both are
        // parameters of the same tag, so the later one is its correction — and the
        // key keeps the blob's position, as python's dict assignment does.
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="arguments">{"target_id":"blob"}</parameter>"#
                + #"<parameter name="target_id">split</parameter></invoke>"#)[0]
        #expect(call.arguments.serialize() == #"{"target_id":"split"}"#)
    }

    @Test func aBlobThatIsNotAnObjectContributesNothing() {
        // An unreadable or non-object blob is dropped rather than coerced; the split
        // parameters beside it may still carry the whole call.
        for blob in ["{oops", "[1,2]"] {
            let call = extractToolCalls(
                #"<invoke name="send"><parameter name="arguments">"# + blob + "</parameter>"
                    + #"<parameter name="target_id">a</parameter></invoke>"#)[0]
            #expect(call.arguments.serialize() == #"{"target_id":"a"}"#, "\(blob)")
        }
        // …and with nothing beside it there is no call at all — reported, never
        // silent.
        let alone = #"<invoke name="send"><parameter name="arguments">[1,2]</parameter></invoke>"#
        #expect(extractToolCalls(alone).isEmpty)
        #expect(malformedCalls(alone).count == 1)
    }

    @Test func aRepeatedParameterKeepsTheLastValue() {
        // Python collects the matches into a dict, so a repeated key keeps its first
        // POSITION and takes the last VALUE.
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="k">1</parameter>"#
                + #"<parameter name="k">2</parameter></invoke>"#)[0]
        #expect(call.arguments.serialize() == #"{"k":2}"#)
    }

    @Test func anAbsentNameFallsThroughToTheOnlyToolThereIs() {
        // `send` is the only tool, so an `<invoke>` with no name at all is a send.
        let call = extractToolCalls(
            #"<invoke><parameter name="target_id">a</parameter></invoke>"#)[0]
        #expect(call.name == "send")
        #expect(call.arguments.serialize() == #"{"target_id":"a"}"#)
    }

    @Test func aNamedVerbIsNotTranslatedInThisEnvelope() {
        // FAITHFUL TO PYTHON, and stated rather than assumed: the verb-in-the-name
        // translation lives in `parseOne` (the taught envelope) only, so a name
        // other than `send` reaches routing as-is here. That asymmetry is python's;
        // canonical is python, so a change belongs there first and lands in all
        // runtimes together.
        let call = extractToolCalls(
            #"<invoke name="reflect"><parameter name="arguments">{"target_id":"clock"}</parameter></invoke>"#
        )[0]
        #expect(call.name == "reflect")
        #expect(call.arguments.serialize() == #"{"target_id":"clock"}"#)
    }

    @Test func attributesAndWhitespaceAroundThemAreTolerated() {
        let call = extractToolCalls(
            #"<invoke name = "send" ><parameter name = "target_id" >a</parameter></invoke>"#)[0]
        #expect(call.arguments.serialize() == #"{"target_id":"a"}"#)
    }

    @Test func aParameterValueSpanningLinesIsStripped() {
        let call = extractToolCalls(
            "<invoke name=\"send\">\n  <parameter name=\"arguments\">\n"
                + #"  {"target_id":"m","payload":{"type":"x"}}"# + "\n  </parameter>\n</invoke>")[0]
        #expect(call.arguments.serialize() == #"{"target_id":"m","payload":{"type":"x"}}"#)
    }

    @Test func aValueEndsAtTheFirstCloserAndKeepsItsTextVerbatim() {
        // The value is NON-GREEDY (it ends at the first `</parameter>`) and is not
        // interpreted: an HTML entity or a stray tag inside it is the model's text,
        // and mangling it would corrupt a program body.
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="code">if a &lt; b: pass</parameter></invoke>"#)[
                0]
        #expect(call.arguments["code"].asString == "if a &lt; b: pass")
    }

    @Test func anUnterminatedInvokeStillCarriesItsCall() {
        // Same rule as the other two envelopes: the envelope is the LAST thing in
        // the completion, so there is nothing after it to misread, and dropping it
        // loses an unambiguous instruction.
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="arguments">"#
                + #"{"target_id":"m","payload":{"type":"x"}}</parameter>"#)[0]
        #expect(call.arguments.serialize() == #"{"target_id":"m","payload":{"type":"x"}}"#)
    }

    @Test func aTagThatDoesNotCompleteTheShapeIsSkippedNotFatal() {
        // A `<parameter>` carrying a second attribute does not match the shape, so
        // it is stepped over and the scan continues — the parameters that DO match
        // still form the call. (Python's regex retries one character on, which is
        // the same outcome.)
        let call = extractToolCalls(
            #"<invoke name="send"><parameter name="target_id">a</parameter>"#
                + #"<parameter type="str" name="k">1</parameter></invoke>"#)[0]
        #expect(call.arguments.serialize() == #"{"target_id":"a"}"#)
    }

    @Test func noParametersAtAllIsNotACallButIsReported() {
        // An `<invoke>` that named a tool and passed nothing is not a call we can
        // make. It opened an envelope, though, so the model is TOLD — silence would
        // read as "done".
        for text in [
            #"<invoke name="send"></invoke>"#,
            #"<invoke name="send"><parameter name="arguments">{}</parameter></invoke>"#,
        ] {
            #expect(extractToolCalls(text).isEmpty, "\(text)")
            let bad = malformedCalls(text)
            #expect(bad.count == 1, "\(text)")
            #expect(bad.first?["envelope"].asString == INVOKE_OPEN, "\(text)")
        }
    }
}
