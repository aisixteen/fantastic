// The CALL CHECKER — python `tests/test_call_check.py`, case for case and string
// for string (the reports are the parity proof).

import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@testable import FantasticAICore

private let E59 = "I'll look at the files agent.\n<tool_call>\n<function=call name=\"send\">\n"
    + "<target_id>files</target_id>\n<type>reflect</type>\n</call>\n</tool_call>"

private let TAIL = "A call is <call name=\"…\">, one child tag per argument, closed by </call>."

private func last(_ script: ProviderScript, _ i: Int) -> String {
    script.request(i)?.last?["content"].asString ?? ""
}

@Suite("the call checker: calls written wrong, reported like a compiler")
struct CallCheckTests {

    @Test func theMeasuredShapeIsReportedWithTheFormToWrite() {
        #expect(
            CallCheck.check(E59, ran: 0) == [
                "[malformed call — nothing ran] line 3, col 1:\n  <function=call name=\"send\">\n"
                    + "  ^~~~~~~~~~~~~~~~~~~~~~~~~~~ not a call tag\nwrite it as:\n<call name=\"send\">\n"
                    + "<target_id>files</target_id>\n<type>reflect</type>\n</call>\n"
                    + "also: line 2 `<tool_call>` — drop the wrapper.\n" + TAIL
            ])
    }

    @Test func aCallNameAsATagNextToAGoodCallSaysTheGoodOneRan() {
        let answer = "<call name=\"send\"><target_id>a</target_id><type>reflect</type></call>\n"
            + "<send><target_id>b</target_id><type>read</type></send>"
        #expect(
            CallCheck.check(answer, ran: 1) == [
                "[malformed call — nothing ran] line 2, col 1:\n"
                    + "  <send><target_id>b</target_id><type>read</type></send>\n  ^~~~~~ not a call tag\n"
                    + "write it as:\n<call name=\"send\">\n<target_id>b</target_id>\n<type>read</type>\n"
                    + "</call>\nalso: `</send>` — close with </call>.\n" + TAIL + "\n"
                    + "The other call of this answer ran — its reply is in this batch; do not write it again."
            ])
        #expect(
            CallCheck.check(answer, ran: 2)[0].hasSuffix(
                "The other 2 calls of this answer ran — their replies are in this batch; do not "
                    + "write them again."))
    }

    /// Measured 2026-10-07 (misleading_fold:rust): a grep scoped by `<ids>` lost
    /// its leading `<` on the 3090 and the form to write kept only
    /// target/title/type — the model rewrote it unscoped. Every argument the model
    /// wrote carries over, a nested `<item>` as part of its list.
    @Test func theVerbsOwnArgumentsCarryIntoTheFormToWrite() {
        let broken =
            "call name=\"send\">\n<target_id>ai_fold</target_id>\n"
            + "<title>grep for checksum</title>\n<type>grep</type>\n"
            + "<query>checksum</query>\n<ids><item>32</item></ids>\n</call>"
        let r = CallCheck.check(broken, ran: 0)
        #expect(r.count == 1)
        let form =
            r.first?.components(separatedBy: "write it as:\n").last?
            .components(separatedBy: "\nA call is").first ?? ""
        #expect(
            form
                == "<call name=\"send\">\n<target_id>ai_fold</target_id>\n"
                + "<title>grep for checksum</title>\n<type>grep</type>\n"
                + "<query>checksum</query>\n<ids><item>32</item></ids>\n</call>",
            Comment(rawValue: r.first ?? ""))
    }

    @Test func aJsonCallIsShownAsTags() {
        let r = CallCheck.check(
            "{\"name\": \"send\", \"arguments\": {\"target_id\": \"files\", \"payload\": "
                + "{\"type\": \"list\", \"path\": \".\"}}}", ran: 0)[0]
        #expect(r.contains("^~~~~~~~~~~~~~~ calls are tags, not JSON\n"), "\(r)")
        #expect(
            r.contains(
                "write it as:\n<call name=\"send\">\n<target_id>files</target_id>\n<type>list</type>\n"
                    + "<path>.</path>\n</call>\n"), "\(r)")
    }

    @Test func qwenCoderParametersBecomeChildTags() {
        let r = CallCheck.check(
            "<function=send>\n<parameter=target_id>\nfiles\n</parameter>\n"
                + "<parameter=type>reflect</parameter>\n</function>", ran: 0)[0]
        #expect(
            r.hasPrefix(
                "[malformed call — nothing ran] line 1, col 1:\n  <function=send>\n  ^~~~~~~~~~~~~~~ not a call tag\n"),
            "\(r)")
        #expect(r.contains("<call name=\"send\">\n<target_id>files</target_id>\n<type>reflect</type>"))
        #expect(
            r.contains(
                "also: `<parameter=target_id>` — one child tag per argument: <target_id>…</target_id>; "
                    + "`<parameter=type>` — one child tag per argument: <type>…</type>."), "\(r)")
    }

    @Test func aControlCallAsABareTag() {
        let r = CallCheck.check("finished.\n<done/>", ran: 0)[0]
        #expect(r.contains("line 2, col 1:\n  <done/>\n  ^~~~~~~ not a call tag\n"), "\(r)")
        #expect(r.contains("write it as:\n<call name=\"done\"></call>\n"), "\(r)")
    }

    @Test func argumentsWithNoCallOpen() {
        let r = CallCheck.check("<target_id>notes</target_id>\n<type>read</type>", ran: 0)[0]
        #expect(r.contains("^~~~~~~~~~~ an argument outside a call — open the call above it"), "\(r)")
        #expect(r.contains("<call name=\"send\">\n<target_id>notes</target_id>\n<type>read</type>"))
    }

    @Test func aStrayCloseAfterAGoodCall() {
        let r = CallCheck.check("<call name=\"done\"></call></call>", ran: 0)[0]
        #expect(r.contains("line 1, col 26:"), "\(r)")
        #expect(r.contains("\u{2039}call name=\"done\">\u{2039}/call\u{203A}\u{2039}/call\u{203A}"), "\(r)")
        #expect(
            r.split(separator: "\n", omittingEmptySubsequences: false)[2]
                .hasSuffix("^~~~~~~ a close with no call open — delete it"), "\(r)")
        #expect(!r.contains("write it as"), "\(r)")
    }

    @Test func proseAndForeignMarkupWithoutOurVocabularyAreNotCalls() {
        for answer in [
            "All done, the file is written.",
            "Use <b>bold</b> and a <div class='x'>box</div>; 3 < 4 > 2.",
            "<tool_call>\n</tool_call>",
            "<call name=\"done\"></call>",
        ] {
            #expect(CallCheck.check(answer, ran: 0).isEmpty, "\(answer)")
        }
    }

    @Test func whatACallEnvelopeHoldsIsNeverChecked() {
        let good = "<call name=\"send\"><target_id>notes</target_id><type>write</type><path>x.md</path>"
            + "<bytes><send>no</send> </call> {\"name\": \"send\"}</bytes></call>"
        #expect(CallCheck.check(good, ran: 1).isEmpty)
        #expect(CallCheck.check("<call name=\"send\" target_id=\"x\"></call>", ran: 0).isEmpty)
    }

    @Test func reportsAreCapped() {
        let answer = (0..<5).map { "<send><target_id>a\($0)</target_id></send>" }.joined(separator: "\n")
        let reports = CallCheck.check(answer, ran: 0)
        #expect(reports.count == CallCheck.MAX_REPORTS)
        #expect(reports.last?.hasSuffix("(2 more malformed calls not shown.)") == true)
    }

    @Test func aLongLineIsWindowedAndTheCaretFollows() {
        let answer = String(repeating: "x", count: 100) + "<function=call name=\"send\"></call>"
        let lines = CallCheck.check(answer, ran: 0)[0].split(separator: "\n", omittingEmptySubsequences: false)
        let (quoted, caret) = (Array(lines[1].unicodeScalars), Array(lines[2].unicodeScalars))
        #expect(String(lines[1]).hasPrefix("  …"))
        #expect(quoted.firstIndex(of: "<") == caret.firstIndex(of: "^"), "\(lines[1])\n\(lines[2])")
    }

    @Test func aMultibyteLineKeepsTheCaretUnderTheToken() {
        let r = CallCheck.check("é→ <send><target_id>a</target_id></send>", ran: 0)[0]
        #expect(r.hasPrefix("[malformed call — nothing ran] line 1, col 4:\n  é→ <send>"), "\(r)")
        #expect(r.contains("\n     ^~~~~~ not a call tag"), "\(r)")
    }

    // ─── through the agent ──────────────────────────────────────

    @Test func aBrokenCallIsReportedInsteadOfTheNoCallReminder() async throws {
        let script = ProviderScript([[E59], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0]
        #expect(await script.waitForCalls(2), "the report did not come back")
        let nxt = last(script, 1)
        #expect(!nxt.contains("[no call was dispatched]"), "\(nxt)")
        #expect(nxt.contains("[malformed call — nothing ran] line 3, col 1:"), "\(nxt)")
        #expect(nxt.contains("<call name=\"send\">\n<target_id>files</target_id>"), "\(nxt)")
    }

    @Test func theGoodCallsRunAndTheBrokenOneComesBackBesideThem() async throws {
        let good = "<call name=\"send\">\n<target_id>kernel_state</target_id>\n<type>reflect</type>\n"
            + "<tree>none</tree>\n</call>"
        let script = ProviderScript([
            [good + "\n<send><target_id>kernel_state</target_id><type>ps</type></send>"],
            [doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0]
        #expect(await script.waitForCalls(2), "the batch did not come back")
        let batch = last(script, 1)
        #expect(batch.contains("kernel_state reflect"), "\(batch)")
        #expect(batch.contains("[malformed call — nothing ran] line 6, col 1:"), "\(batch)")
        #expect(batch.contains("The other call of this answer ran"), "\(batch)")
    }

    /// A model stuck on one broken shape still ends: a turn whose only output is
    /// broken calls ran nothing, and counts as silent.
    @Test func brokenOnlyTurnsCountTowardGivingUp() async throws {
        let script = ProviderScript(Array(repeating: [E59], count: 10))
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        AISilentStreaks.setCap(kernel, 3)
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0]
        #expect(await script.waitForCalls(3))
        try await Task.sleep(nanoseconds: 500_000_000)
        #expect(script.calls == 3, "a broken-only turn reset the silent streak")
    }
}
