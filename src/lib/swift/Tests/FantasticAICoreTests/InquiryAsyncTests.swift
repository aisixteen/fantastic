// INQUIRY, FULLY ASYNC — python `test_inquiry_async.py`.
//
// An `inquiry` sent to an AI is ACCEPTED at once (`{accepted, re, request_id}`) and
// runs as its own chain on the receiver. The answer the receiver gives with
// `inquiry_answer` in ANY turn of that chain — after whatever work it needs — reaches
// the asker LATER as a message under the inquiry's pid, `[reply to <pid> · <who>
// inquiry]`. A chain that ends without answering tells the asker so. Nothing waits:
// the asker keeps working or `await`s, and its open inquiry counts as work in flight.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

private let DONE = "<call name=\"done\"></call>"
private let AWAIT = "<call name=\"await\"></call>"

private func ask(_ target: String, _ text: String = "ship?") -> String {
    renderCall(
        "send",
        ["target_id": .string(target), "payload": .object(["type": .string("inquiry"), "text": .string(text)])])
}

private func answer(_ text: String) -> String {
    renderCall("inquiry_answer", ["answer": .string(text)])
}

/// The last message of every pass the script was asked for — python `_Script.incoming`.
private func incoming(_ s: ProviderScript) -> [String] {
    (0..<s.calls).map { s.request($0)?.last?["content"].asString ?? "" }
}

/// An asker AI and a receiver AI granted both ways (the answer is the receiver's
/// own send, so it needs the reverse edge). The receiver holds each pass 200 ms,
/// so the asker's `accepted` reply lands before the answer — python `hold=0.2`.
private func pair(_ asker: [String], _ rcv: [String], answerGrant: Bool = true) async
    -> (Kernel, ProviderScript, ProviderScript)
{
    // An exhausted swift script streams NOTHING (a dead turn, nudged); python's
    // yields `done`. Padding keeps the two equal.
    let a = ProviderScript((asker + [DONE, DONE]).map { [$0] })
    let r = ProviderScript((rcv + [DONE, DONE]).map { [$0] })
    var cfg = scriptedConfig(["ia_asker": a, "ia_rcv": r])
    cfg.makeProvider = { agent, _, _ in
        agent.id.value == "ia_rcv"
            ? MockProvider(script: r, chunkDelayNanos: 200_000_000) : MockProvider(script: a)
    }
    let (k, _, _) = await bootMockKernel(config: cfg, backendIds: ["ia_asker", "ia_rcv"])
    if !answerGrant {
        var cb = k.agent(AgentId("ia_asker"))?.metaValue(forKey: "callable_by")?.asObject ?? [:]
        cb.removeValue(forKey: "ia_rcv")
        k.update(AgentId("ia_asker"), ["callable_by": .object(cb)])
    }
    return (k, a, r)
}

private func go(_ k: Kernel) async {
    _ = await k.send([
        Item(AgentId("ia_asker"), .object(["type": .string("send"), "text": .string("decide")]))
    ])
}

@Suite("inquiry, fully async")
struct InquiryAsyncTests {

    @Test func theAnswerArrivesLaterUnderTheInquiryPid() async throws {
        let (k, a, r) = await pair([ask("ia_rcv"), AWAIT, DONE], [answer("ship it")])
        await go(k)
        #expect(await a.waitForCalls(3))
        let got = incoming(a)
        #expect(got[1].contains("ia_rcv inquiry") && got[1].contains("<accepted>true</accepted>"), "\(got[1])")
        // the receiver's own send, so it carries the sender header, then the tag
        #expect(got[2].hasPrefix("[from ia_rcv]") && got[2].contains("[reply to "), "\(got[2])")
        #expect(got[2].contains(" ia_rcv inquiry]"), "\(got[2])")
        #expect(got[2].contains("<answer>ship it</answer>"), "\(got[2])")
        #expect(!got.contains { $0.contains("[await refused]") }, "\(got)")
        // the receiver saw the question with its head: who asked, under which pid
        #expect(incoming(r)[0].hasPrefix("[inquiry from ia_asker · re "), "\(incoming(r)[0])")
    }

    @Test func theReceiverMayWorkForSeveralTurnsFirst() async throws {
        let look = renderCall(
            "send", ["target_id": .string("ia_rcv"), "payload": .object(["type": .string("reflect")])])
        let (k, a, r) = await pair([ask("ia_rcv"), AWAIT, DONE], [look, answer("ship it")])
        await go(k)
        #expect(await a.waitForCalls(3))
        #expect(r.calls == 2, "it worked a turn, then answered: \(incoming(r))")
        #expect(incoming(a)[2].contains("<answer>ship it</answer>"), "\(incoming(a))")
    }

    @Test func endingWithoutAnAnswerTellsTheAsker() async throws {
        let (k, a, _) = await pair([ask("ia_rcv"), AWAIT, DONE], [DONE])
        await go(k)
        #expect(await a.waitForCalls(3))
        #expect(incoming(a)[2].contains("ia_rcv ended its run without answering"), "\(incoming(a))")
    }

    @Test func aPlaceholderIsRefusedAndTheInquiryStaysOpen() async throws {
        let (k, a, r) = await pair(
            [ask("ia_rcv"), AWAIT, DONE], [answer("..."), answer("ship it")])
        await go(k)
        #expect(await a.waitForCalls(3))
        #expect(incoming(r)[1].contains("PLACEHOLDER"), "\(incoming(r))")
        #expect(incoming(a)[2].contains("<answer>ship it</answer>"), "\(incoming(a))")
    }

    /// The answer is the receiver's own send: without the
    /// reverse grant it is refused, and the RECEIVER is told — never dropped.
    @Test func anAnswerTheAskerDoesNotAdmitIsToldToTheReceiver() async throws {
        let (k, a, r) = await pair(
            [ask("ia_rcv"), DONE], [answer("ship it"), DONE], answerGrant: false)
        await go(k)
        #expect(await r.waitForCalls(2))
        #expect(incoming(r)[1].contains("your inquiry answer did not reach ia_asker"), "\(incoming(r))")
        #expect(!incoming(a).contains { $0.contains("ship it") }, "\(incoming(a))")
    }

    @Test func anAnswerWithNothingOpenIsToldSo() async throws {
        let s = ProviderScript([[answer("ship it")], [DONE], [DONE]])
        let (k, ids, _) = await bootMockKernel(
            config: scriptedConfig(["ia_lone": s]), backendIds: ["ia_lone"])
        _ = await k.send([Item(ids["ia_lone"]!, .object(["type": .string("send"), "text": .string("hi")]))])
        #expect(await s.waitForCalls(2))
        #expect(incoming(s)[1].contains("inquiry_answer: nothing asked you in this run"), "\(incoming(s))")
    }

    /// An operator's inquiry is accepted at once and runs as its own turn, the
    /// question under a head naming who asked and the pid — python
    /// `test_inquiry_texts`. Its answer has no conversation to land in: it is an
    /// event on the receiver's inbox.
    @Test func anOperatorInquiryIsAcceptedAndHeaded() async throws {
        let s = ProviderScript([[answer("ship it")], [DONE]])
        let (k, ids, _) = await bootMockKernel(
            config: scriptedConfig(["ia_op": s]), backendIds: ["ia_op"])
        let got = await k.send([
            Item(
                ids["ia_op"]!,
                .object([
                    "type": .string("inquiry"), "text": .string("ship?"), "request_id": .string("r1"),
                    "options": .array([.string("a"), .string("b")]),
                    "action": .object(["type": .string("deploy")]),
                ]))
        ])[0].result
        #expect(got["accepted"] == .bool(true) && got["request_id"].asString == "r1", "\(got)")
        let pid = try #require(got["re"].asInt)
        #expect(await s.waitForCalls(1))
        let turn = incoming(s)[0]
        #expect(turn.hasPrefix("[inquiry from the operator · re \(pid) — "), "\(turn)")
        #expect(turn.hasSuffix("ship?\n\nOptions: a, b\n\nProposed action: {\"type\": \"deploy\"}"), "\(turn)")
    }
}
