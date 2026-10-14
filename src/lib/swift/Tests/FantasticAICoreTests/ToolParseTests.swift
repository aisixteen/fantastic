// The ONE call grammar: `<call name="…">` with bare child tags, args inline,
// `bytes` verbatim, no JSON anywhere. Parse or refuse with the fault named —
// never repair.
//
// Case for case the canonical python suite
// `bundled_agents/ai/ai_core/tests/test_tool_parse.py`, whose expectations were
// produced by running the fixtures through the python implementation. Rust's
// `tool_parse/tests_grammar.rs` is the same translation.
//
// ONE runtime divergence, deliberate: `bytes` lands as a swift `String`, not
// python's `bytes` object. It rides that way to the kernel too, because the file
// bundle REFUSES a `write`/`patch`/`append` whose `bytes` is not a string.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

private let WRITE = """
    <call name="send">
    <target_id>notes</target_id>
    <reply_to>self</reply_to>
    <title>write the report</title>
    <type>write</type>
    <path>report.md</path>
    <bytes>PRJ-1BCA4F

    # Report
    - the phrase "quick brown fox", verbatim
    </bytes>
    </call>
    """

private let PATCH = """
    <call name="send">
    <target_id>notes</target_id>
    <type>patch</type>
    <edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
    <bytes><<<<<<< SEARCH
        msg = "Hello, " + name
    =======
        return f"Hello, {name}!"
    >>>>>>> REPLACE
    </bytes>
    </call>
    """

/// Exactly one call, or the failure names what the parser saw instead.
private func one(_ text: String, _ loc: SourceLocation = #_sourceLocation) throws -> ToolCall {
    let calls = extractToolCalls(text)
    try #require(
        calls.count == 1,
        "\(calls.count) calls; malformed: \(malformedCalls(text).map(\.error))",
        sourceLocation: loc)
    return calls[0]
}

private func payload(_ c: ToolCall) -> JSON { c.arguments["payload"] }

@Suite("tool-call grammar")
struct ToolParseTests {

    // ── the taught shapes ────────────────────────────────────────

    @Test("write: args inline, routing out, bytes verbatim")
    func writeArgsInlineRoutingOutBytesVerbatim() throws {
        let c = try one(WRITE)
        #expect(c.name == "send")
        #expect(c.id.hasPrefix("call_"))
        #expect(c.arguments["target_id"].asString == "notes")
        #expect(c.arguments["reply_to"].asString == "self")
        #expect(c.arguments["title"].asString == "write the report")
        #expect(
            payload(c)
                == .object([
                    "type": .string("write"),
                    "path": .string("report.md"),
                    "bytes": .string(
                        "PRJ-1BCA4F\n\n# Report\n- the phrase \"quick brown fox\", verbatim\n"),
                ]))
    }

    @Test("patch: a nested list of objects, hunks typed")
    func patchNestedListAndTypedHunks() throws {
        let p = payload(try one(PATCH))
        #expect(p["type"].asString == "patch")
        #expect(
            p["edits"]
                == .array([.object(["path": .string("hello.py"), "hunks": .integer(2)])]))
        #expect(p["bytes"].asString?.hasPrefix("<<<<<<< SEARCH\n    msg = ") == true)
    }

    @Test("JSON-looking content rides untouched")
    func jsonLookingContentRidesUntouched() throws {
        let text =
            #"<call name="send"><target_id>notes</target_id><type>write</type>"#
            + "<path>c.json</path><bytes>{\"name\": \"demo\"}\n{\"tags\": [\"a\", \"b\"]}\n"
            + "</bytes></call>"
        #expect(
            payload(try one(text))["bytes"].asString
                == "{\"name\": \"demo\"}\n{\"tags\": [\"a\", \"b\"]}\n")
    }

    @Test("a list is <item> children, at any count")
    func listsAreItemChildrenAnyCount() throws {
        let text =
            #"<call name="send"><target_id>me</target_id><type>grep</type>"#
            + "<query>marker</query><ids><item>12</item><item>15</item></ids></call>"
        #expect(payload(try one(text))["ids"] == .array([.integer(12), .integer(15)]))
        let single = text.replacingOccurrences(of: "<item>15</item>", with: "")
        #expect(payload(try one(single))["ids"] == .array([.integer(12)]))
        let allowed =
            #"<call name="send"><target_id>kernel</target_id><type>update_agent</type>"#
            + #"<id>worker</id><allowed><item>python3 \S+</item></allowed></call>"#
        #expect(payload(try one(allowed))["allowed"] == .array([.string(#"python3 \S+"#)]))
    }

    @Test("a one-field object is not a list")
    func objectWithOneFieldIsNotAList() throws {
        let text =
            #"<call name="send"><target_id>notes</target_id><type>patch</type>"#
            + "<edit><path>a.py</path></edit><bytes>x</bytes></call>"
        #expect(payload(try one(text))["edit"] == .object(["path": .string("a.py")]))
    }

    @Test("scalars are typed by the ONE rule")
    func scalarsTypedByTheOneRule() throws {
        let text =
            #"<call name="send"><target_id>me</target_id><type>pull</type>"#
            + "<offset>0</offset><length>1.5</length><checkpoint>true</checkpoint>"
            + "<why>keep</why><empty></empty></call>"
        #expect(
            payload(try one(text))
                == .object([
                    "type": .string("pull"),
                    "offset": .integer(0),
                    "length": .double(1.5),
                    "checkpoint": .bool(true),
                    "why": .string("keep"),
                    "empty": .string(""),
                ]))
    }

    @Test("done and inquiry_answer are control calls")
    func doneAndInquiryAnswer() throws {
        let done = try one(#"<call name="done"></call>"#)
        #expect(done.name == "done")
        #expect(done.arguments == .object([:]))
        let ans = try one(#"<call name="inquiry_answer"><answer>yes, "both"</answer></call>"#)
        #expect(ans.name == "inquiry_answer")
        #expect(ans.arguments == .object(["answer": .string("yes, \"both\"")]))
    }

    @Test("await is a control call")
    func awaitIsAControlCall() throws {
        let c = try one(#"<call name="await"></call>"#)
        #expect(c.name == "await")
        #expect(c.arguments == .object([:]))
    }

    // ── verbatim mode ────────────────────────────────────────────

    @Test("a </call> inside bytes does not end the call")
    func aClosingCallTagInsideBytesDoesNotEndTheCall() throws {
        let text =
            #"<call name="send"><target_id>notes</target_id><type>write</type>"#
            + "<path>a.md</path><bytes>the text says </call> and goes on\n</bytes>"
            + "<checkpoint>true</checkpoint></call>"
        let p = payload(try one(text))
        #expect(p["bytes"].asString == "the text says </call> and goes on\n")
        #expect(p["checkpoint"] == .bool(true))
    }

    @Test("bytes is never trimmed; other values are")
    func bytesIsNeverTrimmedAndOtherValuesAre() throws {
        let text =
            #"<call name="send"><target_id>notes</target_id><type>write</type>"#
            + "<path>  a.md  </path><bytes>  two spaces \n</bytes></call>"
        let p = payload(try one(text))
        #expect(p["path"].asString == "a.md")
        #expect(p["bytes"].asString == "  two spaces \n")
    }

    /// No `</call>`, no call: the model stopped generating
    /// before it closed — nothing dispatches, nothing is "malformed"; the cut
    /// marker names it.
    @Test("an unterminated call is not a call")
    func anUnterminatedCallIsNotACall() {
        let text = #"<call name="send"><target_id>notes</target_id><type>reflect</type>"#
        #expect(extractToolCalls(text).isEmpty)
        #expect(malformedCalls(text).isEmpty)
    }

    @Test("several calls in order, including one inside <think>")
    func severalCallsInOrderAndACallInsideThink() {
        let calls = extractToolCalls("<think>first \(WRITE)</think>then\n\(PATCH)\n")
        #expect(calls.map { $0.arguments["payload"]["type"].asString } == ["write", "patch"])
    }

    @Test("a mention in prose is stepped over, not reported")
    func aMentionInProseIsSteppedOverNotReported() {
        let text = "I will emit a <call soon. \(PATCH)"
        #expect(extractToolCalls(text).count == 1)
        #expect(malformedCalls(text).isEmpty)
        #expect(extractToolCalls("a <caller> is not an envelope").isEmpty)
    }

    // ── refusals: fault named, shape shown, nothing repaired ─────

    /// Nothing dispatched, exactly one report, every needle in the error, and
    /// the echoed block defanged.
    @discardableResult
    private func refused(
        _ text: String, _ needles: [String], _ loc: SourceLocation = #_sourceLocation
    ) throws -> MalformedCall {
        #expect(extractToolCalls(text).isEmpty, sourceLocation: loc)
        let bad = malformedCalls(text)
        try #require(bad.count == 1, "\(bad.map(\.error))", sourceLocation: loc)
        for n in needles {
            #expect(
                bad[0].error.contains(n), "missing \(n) in: \(bad[0].error)", sourceLocation: loc)
        }
        #expect(bad[0].error.hasPrefix("call NOT dispatched: "), sourceLocation: loc)
        #expect(bad[0].error.contains("The shape:"), sourceLocation: loc)
        #expect(
            !bad[0].yourBlock.contains("<call"), "the echoed block is defanged", sourceLocation: loc
        )
        return bad[0]
    }

    @Test("a </bytes> inside a document ends the value early and is refused")
    func bytesCloserInsideADocumentIsRefused() throws {
        let text =
            #"<call name="send"><target_id>notes</target_id><type>write</type>"#
            + "<path>a.md</path><bytes>first </bytes> no\nreal end</bytes></call>"
        try refused(text, ["text after the last tag", "no"])
    }

    @Test("bytes never closed is a cut, not a call")
    func bytesNeverClosedIsACutNotACall() {
        // an open `<bytes>` swallows any `</call>`, so the envelope never closes
        let text =
            #"<call name="send"><target_id>notes</target_id><type>write</type><bytes>open</call>"#
        #expect(extractToolCalls(text).isEmpty)
        #expect(malformedCalls(text).isEmpty)
    }

    @Test("an element never closed")
    func elementNeverClosed() throws {
        try refused(
            #"<call name="send"><target_id>notes</target_id><type>write<path>a</path></call>"#,
            ["`<type>` opened and never closed"])
    }

    @Test("a duplicate scalar tag names the list shape")
    func duplicateScalarTag() throws {
        try refused(
            #"<call name="send"><target_id>n</target_id><path>a</path><path>b</path></call>"#,
            ["duplicate `<path>`", "<path><item>"])
    }

    @Test("text between tags")
    func textBetweenTags() throws {
        try refused(
            #"<call name="send"><target_id>n</target_id> stray words <type>x</type></call>"#,
            ["text outside a tag"])
    }

    /// Python `test_send_without_target`: the SAME input as the
    /// neither-target-nor-to case, pinned on its own so the one needle python
    /// asserts stays asserted when the other test's needles change.
    @Test("a send without a target is refused")
    func sendWithoutTarget() throws {
        try refused(#"<call name="send"><type>reflect</type></call>"#, ["a send names its target"])
    }

    @Test("a send with neither target nor to is refused naming both")
    func sendWithoutTargetNamesBothShapes() throws {
        try refused(
            #"<call name="send"><type>reflect</type></call>"#,
            ["a send names its target", "<target_id>", "<to><item>"])
    }

    /// Python's `coerce_scalar` types a numeric-shaped target_id
    /// as an (unbounded) `int`, so `isinstance(target, str)` fails and the
    /// send is refused even though the digits are too wide for this
    /// runtime's `Int64` carrier — it must not fall back to dispatching to a
    /// literal numeral agent id.
    @Test("a numeric-shaped target_id, even too wide for Int64, is not a string target")
    func numericTargetIdIsNotAStringTarget() throws {
        try refused(
            #"<call name="send"><target_id>123456789012345678901234567890</target_id><type>reflect</type></call>"#,
            ["a send names its target"])
        try refused(
            #"<call name="send"><target_id>42</target_id><type>reflect</type></call>"#,
            ["a send names its target"])
    }

    @Test("a head with an extra attribute")
    func headWithExtraAttribute() throws {
        try refused(
            #"<call name="send" id="7"><target_id>n</target_id></call>"#,
            [#"the call head must be exactly `name="<tool>"`"#])
    }

    @Test("defang breaks every delimiter")
    func defangBreaksEveryDelimiter() {
        let d = defang(#"<call name="x"><bytes>a</bytes></call>"#)
        #expect(!d.contains("<call"))
        #expect(!d.contains("</call>"))
        #expect(!d.contains("<bytes>"))
        #expect(d.contains("\u{2039}call"))
    }

    @Test("the broken block is shown WHOLE")
    func theBrokenBlockIsShownWhole() {
        // The fault's LOCATION is why: an unclosed document breaks at the END of
        // a long payload, so a head-only echo shows the model text that looks
        // fine and it re-sends the identical envelope.
        let big = String(repeating: "x", count: 40_000)
        let text =
            #"<call name="send"><target_id>n</target_id><type>write</type><bytes>"# + big
            + "</bytes><path>a</path><path>b</path></call>"
        let bad = malformedCalls(text)
        #expect(bad.count == 1)
        #expect(bad.first?.yourBlock.contains(big) == true)
    }

    // ── render ↔ parse ───────────────────────────────────────────

    @Test("renderCall round-trips the parser's shape")
    func renderCallRoundTripsTheParserShape() throws {
        let args: OrderedDictionary<String, JSON> = [
            "target_id": .string("notes"),
            "reply_to": .string("self"),
            "title": .string("t"),
            "payload": .object([
                "type": .string("patch"),
                "edits": .array([
                    .object(["path": .string("a.py"), "hunks": .integer(2)]),
                    .object(["path": .string("b.py"), "hunks": .integer(1)]),
                ]),
                "ids": .array([.integer(3)]),
                "dry_run": .bool(true),
                "bytes": .string("x = \"1\"\n</call>\n"),
            ]),
        ]
        #expect(try one(renderCall("send", args)).arguments == .object(args))
        #expect(try one(renderCall("done", [:])).arguments == .object([:]))
        #expect(
            try one(renderCall("inquiry_answer", ["answer": .string("ok")])).arguments
                == .object(["answer": .string("ok")]))
    }

    @Test("grant maps keyed by ids and relative tokens are tags")
    func grantMapsKeyedByIdsAndRelativeTokensAreTags() throws {
        let text =
            #"<call name="send"><target_id>kernel</target_id><type>update_agent</type>"#
            + "<id>vault</id><callable_by><$self>*</$self><$parent>*</$parent>"
            + "<ai-rev.2>*</ai-rev.2><notes><item>read</item><item>write</item></notes>"
            + "</callable_by></call>"
        let p = payload(try one(text))
        #expect(
            p["callable_by"]
                == .object([
                    "$self": .string("*"),
                    "$parent": .string("*"),
                    "ai-rev.2": .string("*"),
                    "notes": .array([.string("read"), .string("write")]),
                ]))
        let round = renderCall("send", ["target_id": .string("k"), "payload": p])
        #expect(payload(try one(round)) == p)
    }

    @Test("a send may name several recipients with <to>")
    func aSendMayNameSeveralRecipientsWithTo() throws {
        let text =
            #"<call name="send"><to><item>solver_2</item><item>solver_3</item></to>"#
            + "<re>412</re><type>send</type><text>CANDIDATE 586</text></call>"
        let a = try one(text).arguments
        #expect(a["to"] == .array([.string("solver_2"), .string("solver_3")]))
        #expect(a["re"] == .integer(412))
        #expect(a.asObject?["target_id"] == nil)
        #expect(
            a["payload"]
                == .object(["type": .string("send"), "text": .string("CANDIDATE 586")]))
        #expect(try one(renderCall("send", a.asObject ?? [:])).arguments == a)
    }

    @Test("<item> beside other tags is an ordinary field")
    func itemBesideOtherTagsIsAField() throws {
        // `<payload><type>buy</type><item>rocket</item></payload>` is a field named
        // item, never a list error.
        let c = try one(
            #"<call name="send"><target_id>user</target_id><type>inquiry</type>"#
                + "<action><payload><type>buy</type><item>rocket</item></payload></action></call>")
        #expect(
            payload(c)["action"]["payload"]
                == .object(["type": .string("buy"), "item": .string("rocket")]))
    }

    // ── code points, not graphemes ───────────────────────

    // Python indexes a str by CODE POINT. Swift's `[Character]` scan was a
    // second index space: a CRLF is one Character, an NFD `é` is one Character,
    // and two Characters compare EQUAL when canonically equivalent.
    @Test("the scanner indexes code points like python")
    func scannerIndexesCodePointsLikePython() throws {
        // 1. head40 counts CODE POINTS: `ab\r\n` × 10 is 40 scalars but only 30
        // Characters, so a grapheme-indexed excerpt quoted a third more text.
        // TERMINATED: an unterminated block is a cut, never reported malformed.
        let tail = String(repeating: "ab\r\n", count: 20)
        let faults = malformedCalls(
            "<call name=\"send\"><target_id>a</target_id>" + tail + "</call>")
        #expect(faults.count == 1)
        #expect(
            faults[0].error.contains(
                "text after the last tag (`" + String(repeating: "ab\r\n", count: 10) + "`)"),
            "\(faults[0].error)")

        // 2. An NFC opener is NOT closed by an NFD closer — python compares code
        // points, and swift's Character `==` folded them together.
        let nfc = "<call name=\"send\"><target_id>a</target_id><t\u{e9}>x</t\u{65}\u{301}></call>"
        #expect(extractToolCalls(nfc).isEmpty, "an NFD closer must not close an NFC tag")

        // 3. `s\u{e9}nd` is not a tool: the head class is ASCII, so the name stops
        // at `s` and the block is refused.
        #expect(
            extractToolCalls("<call name=\"s\u{e9}nd\"><target_id>a</target_id></call>").isEmpty)

        // 4. A tag name may not continue with Other_Alphabetic or a circled
        // letter — python's `\w` is L*/numeric/underscore.
        for bad in [
            "<k_\u{928}\u{93e}\u{92e}>v</k_\u{928}\u{93e}\u{92e}>", "<x\u{24b6}>v</x\u{24b6}>",
        ] {
            let text =
                "<call name=\"send\"><target_id>a</target_id><type>t</type>" + bad + "</call>"
            #expect(
                extractToolCalls(text).isEmpty,
                "`\(bad)` must read as text outside a tag, not dispatch")
        }

        // 5. `<call` + an INFORMATION SEPARATOR is an envelope: python's
        // `str.isspace()` includes U+001C..U+001F.
        let sep = "<call\u{1c}name=\"send\"><target_id>a</target_id><type>reflect</type></call>"
        #expect(extractToolCalls(sep).count == 1)
    }

    // ── `to` ─────────────────────────────────────

    // Python keeps a truthy non-list `to` EXACTLY as written and dispatches the
    // call; routing answers the no-target reply. Only a FALSY `to` with no
    // string target is refused at parse.
    @Test("a truthy non-list `to` is kept raw")
    func aTruthyNonListToIsKeptRaw() throws {
        let c = try one("<call name=\"send\"><to>42</to><type>reflect</type></call>")
        #expect(c.arguments["to"] == .integer(42))
        #expect(c.arguments.asObject?["target_id"] == nil)
        // `<to>0</to>` is falsy, so there is no target at all.
        let zero = "<call name=\"send\"><to>0</to><type>reflect</type></call>"
        #expect(extractToolCalls(zero).isEmpty)
        #expect(malformedCalls(zero).first?.error.contains("a send names its target") == true)
    }

    // Each recipient is `str(t).strip()` — python's `str`, not JSON.
    @Test("to items are python str")
    func toItemsArePythonStr() throws {
        let c = try one(
            "<call name=\"send\"><to><item>true</item><item>1e20</item>"
                + "<item><a>x</a></item><item><item>y</item></item></to>"
                + "<type>reflect</type></call>")
        #expect(
            c.arguments["to"]
                == .array([
                    .string("True"), .string("1e+20"), .string("{'a': 'x'}"), .string("['y']"),
                ]))
    }

    // ── reply rendering ──────────────────────────

    // `_reply_text` is `str(x)`: python float reprs, python container reprs, and
    // an EMPTY tag for null.
    @Test("reply scalars are python str")
    func replyScalarsArePythonStr() {
        #expect(renderReply(.object(["c": .double(0.00001)])) == "<c>1e-05</c>")
        #expect(renderReply(.object(["c": .double(1e16)])) == "<c>1e+16</c>")
        #expect(renderReply(.object(["c": .double(.infinity)])) == "<c>inf</c>")
        #expect(renderReply(.object(["c": .double(.nan)])) == "<c>nan</c>")
        #expect(renderReply(.object(["c": .null])) == "<c></c>")
        #expect(renderReply(.object(["c": .bool(true)])) == "<c>true</c>")
        // A STRUCTURE inside a MIXED list nests, each element on its own.
        #expect(
            renderReply(.object(["c": .array([.integer(1), .object(["a": .integer(1)])])]))
                == "<c>\n<item>1</item>\n<item>\n<a>1</a>\n</item>\n</c>")
    }

    // A CRLF body holds a LF code point, so python spans the tag across lines.
    @Test("render_reply spans CRLF text like python")
    func renderReplySpansCrlfTextLikePython() {
        #expect(renderReply(.object(["c": .string("a\r\nb")])) == "<c>\na\r\nb\n</c>")
    }
}
