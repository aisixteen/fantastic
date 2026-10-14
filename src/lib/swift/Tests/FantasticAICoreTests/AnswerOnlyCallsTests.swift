// ONLY THE ANSWER ACTS — python `test_answer_only_calls.py`.
//
// A `<call>` written inside `<think>…</think>` is thinking — a draft, an
// illustration, a plan — never an action: not dispatched, not stored.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

private let DONE = "<call name=\"done\"></call>"

private func selfCall(_ verb: String) -> String {
    renderCall("send", ["target_id": "mock", "payload": .object(["type": .string(verb)])])
}

/// One scripted turn `first`, then `done`; the message the model was handed next.
private func nextAfter(_ first: String) async -> String {
    let script = ProviderScript([[first], [DONE]])
    let (kernel, ids, _) = await bootMockKernel(
        config: scriptedConfig(["mock": script]), backendIds: ["mock"])
    _ = await kernel.send([Item(ids["mock"]!, .object(["type": "send", "text": "go"]))])
    #expect(await script.waitForCalls(2), "the second pass never came")
    return script.request(1)?.last?["content"].asString ?? ""
}

@Suite("only the answer acts")
struct AnswerOnlyCallsTests {

    /// The `fanout` shape: drafted in the thought, written again in the answer.
    @Test func aCallInThinkingAndAnswerDispatchesOnce() async {
        let r = selfCall("reflect")
        let next = await nextAfter("<think>the shape is \(r) — do it</think>\(r)")
        #expect(next.components(separatedBy: "[reply to ").count - 1 == 1, "\(next)")
    }

    @Test func aDraftOnlyInThinkingDoesNotRun() async {
        let next = await nextAfter(
            "<think>maybe \(selfCall("status")) — no</think>\(selfCall("reflect"))")
        #expect(next.components(separatedBy: "[reply to ").count - 1 == 1, "\(next)")
        let tag = next.components(separatedBy: "\n").first ?? ""
        #expect(tag.contains(" reflect") && !tag.contains(" status"), "\(next)")
    }

    /// Nothing of it runs; the empty answer is rerolled with the no-answer line.
    @Test func callsOnlyInThinkingMakeADeadTurn() async {
        let next = await nextAfter("<think>\(selfCall("reflect"))</think>")
        #expect(!next.contains("[reply to "), "\(next)")
        #expect(next.hasPrefix("[no answer] Your last turn produced only thinking"), "\(next)")
    }

    @Test func doneInsideThinkingDoesNotEndTheChain() async {
        let next = await nextAfter("<think>\(DONE)</think>I am finished.")
        #expect(next.hasPrefix("[no call was dispatched]"), "\(next)")
    }

    @Test func anUnclosedThoughtWithACallDoesNotDispatch() async {
        let next = await nextAfter("<think>plan \(selfCall("reflect")) and more")
        #expect(!next.contains("[reply to "), "\(next)")
        #expect(next.hasPrefix("[no answer]"), "\(next)")
    }
}
