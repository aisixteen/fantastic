// Reply routing on the AI agent — the model decides where its output goes.
//
// Mirrors the canonical python suite
// `bundled_agents/ai/ollama/ollama_backend/tests/test_send_to.py`.
//
// After ONE provider pass the output is routed. Two paths, and the FIRST is the
// model's:
//
//   its own `<call>`s — each names a target, a payload and `reply_to`. Every
//     call is dispatched and each REPLY is delivered to its own `reply_to`.
//     `reply_to:'self'` is what makes a chain, one step at a time. NO CALL ENDS
//     THE TURN — stop is the default, so a one-pass task costs one pass.
//
//   the caller's `send_to` — hands this pass's TEXT to another agent, once,
//     without propagating.
//
// SWIFT-SPECIFIC: python's headline regression is `test_self_chain_does_not
// _deadlock`, because its FIFO lock is a non-reentrant asyncio.Lock and a
// self-addressed send awaited inline would block on a lock its own caller holds.
// Swift's `sendVerb` never holds the FIFO across a nested send, so there is no
// deadlock to regress against. `selfAddressedFollowUpSeesThePersistedTurn` carries
// that weight instead, and asserts something strictly stronger: a 3-pass chain
// where pass 3's prompt contains pass 1's assistant text — i.e. the save→release
// →route ordering held, not merely that nothing hung.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// The envelope exactly as the prompt teaches it. `replyTo` is `.some(nil)` for an
/// explicit `null` and `nil` for the key being ABSENT — the two mean opposite
/// things, so the helper has to be able to express both.
private func callEnvelope(
    target: String, payload: JSON, replyTo: String?? = .some("self")
) -> String {
    var args: OrderedDictionary<String, JSON> = ["target_id": .string(target)]
    if let outer = replyTo {
        // The literal word `null` is how the notation says "discard it" — there
        // is no JSON null in a grammar whose values are text.
        args["reply_to"] = .string(outer ?? "null")
    }
    args["payload"] = payload
    return renderCall("send", args)
}

/// A cheap reflect: `tree:"none"` keeps the reply small, so a 3-hop chain's
/// prompts stay well inside the window and a failure is about routing.
private let cheapReflect: JSON = .object([
    "type": .string("reflect"), "tree": .string("none"),
])

@Suite("AI reply routing: the model decides where its output goes")
struct RoutingTests {

    // ─── stop is the default ────────────────────────────────────

    @Test func doneEndsTheTurnAtOnePass() async throws {
        // THE point of model-owned routing: a one-pass task costs ONE pass. What
        // ends it is `done`, EXPLICITLY — stopping is a call, and silence is not
        // stopping (see `aFirstTurnWithNoCallIsNudgedToo`). The second scripted
        // pass exists only so an over-eager chain has something to consume and
        // fails the count instead of stalling.
        let script = ProviderScript([["the answer " + doneEnvelope], ["must never run"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("hi"), "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 1, "done must end the turn, got \(script.calls) passes")
        let msgs = await historyRows(kernel: kernel, id: id, client: "c")
        #expect(msgs.count == 2)
        #expect(msgs[1]["content"].asString?.hasPrefix("the answer") == true)
    }

    // ─── the model acts, and sees the result ────────────────────

    @Test func modelCallIsDispatchedAndTheResultReturns() async throws {
        // The model emits a send; it really runs; its REPLY comes back as the next
        // turn, tagged so the model can tell it from a human's message.
        let script = ProviderScript([
            [callEnvelope(target: "core", payload: .object(["type": .string("list_agents")]))],
            ["done looking " + doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("what is here?"),
                "client_id": .string("c"),
            ]))])[0].result
        // One call = one follow-up turn. That turn ends the chain EXPLICITLY
        // with the `done` call — a silent turn would draw the nudge now.
        #expect(await script.waitForCalls(2), "the call's reply never came back")
        try await Task.sleep(nanoseconds: 200_000_000)
        #expect(script.calls == 2, "the no-call follow-up ended the chain")
        let turn2 = script.requestText(1)
        #expect(turn2.contains(" · core "), "untagged reply: \(turn2.suffix(400))")
        #expect(turn2.contains("agents"), "the real list_agents result must be in the turn")
    }

    @Test func selfAddressedFollowUpSeesThePersistedTurn() async throws {
        // Swift's stand-in for python's deadlock regression, and a stronger claim.
        // A 3-pass chain must run to completion AND pass 3's prompt must contain
        // pass 1's assistant text — which is only true if, for every hop, the
        // assistant turn was SAVED before the FIFO guard was RELEASED before
        // routing started. Get that order wrong and the chain still "works" while
        // quietly answering questions from a history that is one turn behind.
        let reflect = callEnvelope(target: "core", payload: cheapReflect)
        let script = ProviderScript([
            ["PASS-ONE-SPEAKING " + reflect],
            ["PASS-TWO-SPEAKING " + reflect],
            ["enough " + doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        // Three scripted passes; the last ends the chain with the explicit
        // `done` call — three passes in total, no nudge.
        #expect(await script.waitForCalls(3), "chain stalled at \(script.calls) passes")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 3, "expected 3 passes, got \(script.calls)")
        let turn3 = script.requestText(2)
        #expect(turn3.contains("PASS-ONE-SPEAKING"), "pass 3 read a stale history")
        #expect(turn3.contains("PASS-TWO-SPEAKING"))
        // 3 passes = 3 user turns + 3 assistant turns, two rows each.
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 6)
        #expect((await historyRows(kernel: kernel, id: id, client: "c")).count == 6)
    }

    @Test func replyToOmittedDefaultsToSelf() async throws {
        // AN OMITTED `reply_to` MEANS SELF — the model gets its own answer back.
        //
        // The old default was fire-and-forget, and it produced the one failure a
        // model cannot debug: gemma4:12b left the key out of an otherwise perfect
        // call, the send ran, its result was dropped, and the chain died at pass 1
        // with no reply and no error. Indistinguishable, from inside the model,
        // from being finished. Stopping is still free — it is emitting NO call.
        let mine = ProviderScript([
            [
                callEnvelope(
                    target: "peer",
                    payload: .object(["type": .string("send"), "text": .string("ping")]),
                    replyTo: nil)
            ],
            ["x"],
        ])
        let theirs = ProviderScript([["peer answered"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": mine, "peer": theirs]),
            backendIds: ["mock", "peer"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await theirs.waitForCalls(1), "the send itself must still happen")
        #expect(await mine.waitForCalls(2), "an omitted reply_to must come back to self")
        #expect(mine.requestText(1).contains(" · peer "), "and arrive tagged as a reply")
    }

    @Test func discardingAReplyMustBeSaidExplicitly() async throws {
        // Fire-and-forget still exists — it just has to be ASKED for.
        // `reply_to:null` (or "none") acts and ends the chain; that is the right way
        // round, because throwing an answer away is the surprising choice, not the
        // default one.
        let mine = ProviderScript([
            [
                callEnvelope(
                    target: "peer",
                    payload: .object(["type": .string("send"), "text": .string("ping")]),
                    replyTo: .some(nil))
            ],
            ["x"],
        ])
        let theirs = ProviderScript([["peer answered"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": mine, "peer": theirs]),
            backendIds: ["mock", "peer"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await theirs.waitForCalls(1), "the send itself must still happen")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(mine.calls == 1, "an explicit null must mean no follow-up turn")
    }

    @Test func aNonAddressReplyToIsADiscardNotADefault() async throws {
        // The word forms python normalises to "discard" (`none` / `null` /
        // `nobody`), plus the SWIFT trap this is really guarding: `args["reply_to"]`
        // returns `.null` for a MISSING key, so presence has to be read off the
        // dictionary. Read it with the subscript and every absent key becomes an
        // explicit discard — the default inverts, silently, and every chain dies at
        // pass 1 exactly as it did before the default was fixed.
        for word in ["none", "NULL", "nobody"] {
            let mine = ProviderScript([
                [
                    callEnvelope(
                        target: "core", payload: .object(["type": .string("reflect")]),
                        replyTo: .some(word))
                ],
                ["must never run"],
            ])
            let (kernel, ids, _) = await bootMockKernel(
                config: scriptedConfig(["mock": mine]), backendIds: ["mock"])
            _ = await kernel.send([Item(ids["mock"]!, .object([
                    "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                ]))])[0].result
            try await Task.sleep(nanoseconds: 300_000_000)
            #expect(mine.calls == 1, "reply_to '\(word)' must discard, got \(mine.calls) passes")
        }
    }

    @Test func failedCallComesBackToTheModel() async throws {
        // A call that fails must be VISIBLE to the model — a silent failure is the
        // one thing it can never debug.
        let script = ProviderScript([
            [callEnvelope(target: "no_such_agent", payload: .object(["type": .string("reflect")]))],
            ["ok"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the failure never reached the model")
        let turn2 = script.requestText(1).lowercased()
        #expect(turn2.contains("no_such_agent"))
        #expect(
            turn2.contains("error") || turn2.contains("unknown"),
            "not an error: \(turn2.suffix(300))")
    }

    @Test func aRaisingHandlerComesBackAsAnErrorNotADeadChain() async throws {
        // A handler that RAISES must reach the model as `{"error": …}`.
        //
        // Python's routing ran bare until 2026-08-02: a file_bridge whose ingress
        // rule could not be resolved raised out of its handler, the exception
        // escaped `_route_reply`, and asyncio logged "Task exception was never
        // retrieved". The model got no reply, no error and no further turn — the
        // run stopped mid-verification and looked finished. An exception is a
        // result; the model can act on one and cannot act on silence.
        //
        // In swift the catch is the SUBSTRATE's (`kernel.send` never throws), so
        // what is asserted here is that the caught body reaches the model whole:
        // the error text, and the `target` routing adds to it.
        let script = ProviderScript([
            [callEnvelope(target: "raiser", payload: .object(["type": .string("reflect")]))],
            ["saw it"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(
            await script.waitForCalls(2), "the raise killed the chain instead of routing")
        let turn2 = script.requestText(1)
        #expect(
            turn2.contains("IngressRuleError"),
            "the exception must be visible: \(turn2.suffix(400))")
        #expect(
            turn2.contains("password:whatever"), "and must carry what actually went wrong")
        #expect(turn2.contains("raised"), "and be marked as a raise, not a refusal")
        #expect(turn2.contains("raiser"), "and name the agent it happened in")
    }

    // ─── the three model-facing errors ──────────────────────────

    @Test func malformedCallIsReportedNotSwallowed() async throws {
        // A `<call>` with no target tells the model so, instead of vanishing.
        let bad = #"<call name="send"><reply_to>self</reply_to><type>reflect</type></call>"#
        let script = ProviderScript([[bad], ["ok"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "malformed call was swallowed")
        // The GRAMMAR refuses it — a targetless send never reaches routing now,
        // so what comes back is the parser's fault line, not `NO_TARGET_ERROR`.
        #expect(script.requestText(1).contains("a send names its target"))
    }

    @Test func aSendWithNoPayloadNamesWhatIsMissing() async throws {
        // Measured 2026-08-06: a payload-less send reached the handler as `None`
        // and came back `'NoneType' object has no attribute 'get'` — a Python
        // error about our internals, naming neither the omitted verb nor the
        // agent. It must say what was missing instead, and must never dispatch.
        let bad = #"<call name="send"><target_id>probe</target_id></call>"#
        let script = ProviderScript([[bad], ["ok"]])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the payload-less send was swallowed")
        let turn2 = script.requestText(1)
        #expect(turn2.contains("no payload"), "\(turn2.suffix(300))")
        #expect(turn2.contains("probe"), "and must name the agent it was addressed to")
        // NOTHING WAS DISPATCHED — the named agent never heard from us.
        #expect(recorder.payloads().isEmpty, "a payload-less send must not dispatch")
    }

    @Test func anUnparseableCallReachesTheModelAsAnError() async throws {
        // END TO END, which the parser-level tests do not cover — and the gap that
        // let this ship broken in python: on 2026-08-06 `malformed_calls` returned
        // the right report for gemma's truncated `create_agent` and the model still
        // received nothing, so the run went on believing its agent existed.
        // A document opened and never closed: the model wrote a program and the
        // call ran off the end of the turn.
        let broken =
            #"<call name="send"><target_id>core</target_id><type>write</type>"#
            + #"<bytes>x = "unclosed"#
        let script = ProviderScript([[broken], ["ok"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the malformed call produced no next turn")
        #expect(script.requestText(1).contains("NOT dispatched"))
    }

    @Test func malformedIsReportedEvenWhenASiblingCallParsed() async throws {
        // A PARTLY-read turn is the dangerous one: half the model's intent
        // dispatched, and which half was lost is not something it can infer from
        // the replies it got. So the broken envelope is reported even though its
        // sibling ran — and it is reported FIRST, to SELF, because the `reply_to`
        // that would have said otherwise lives in the body we could not read.
        let good = callEnvelope(target: "probe", payload: .object(["type": .string("noted")]))
        let broken =
            #"<call name="send"><target_id>core</target_id><path>a</path>"#
            + #"<path>b</path></call>"#
        // Pass 2 ends the chain EXPLICITLY: every unit is tagged now (python tags
        // them all), so the delivered turn opens with `[reply to …` and a silent
        // follow-up would earn the no-call nudge — correctly, and forever.
        let script = ProviderScript([[broken + "\n" + good], ["ok " + doneEnvelope], ["ok again"]])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        // The sibling really dispatched…
        #expect(await recorder.waitForPayloads(1), "the parsed sibling was dropped")
        #expect(recorder.payloads()[0]["type"].asString == "noted")
        // …and the broken one still produced a report the model can read. BOTH
        // are owed to SELF, so they arrive as ONE delivered turn — that is the
        // batch contract: the model never sees a partial turn, so it never
        // re-asks for a missing half. Exactly ONE follow-up pass happens.
        #expect(await script.waitForCalls(2), "the delivery went missing")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 2, "expected 2 passes, got \(script.calls)")
        let followUp = script.requestText(1)
        #expect(followUp.contains("NOT dispatched"), "the malformed sibling was swallowed")
        #expect(followUp.contains(" · probe "), "the parsed sibling's reply was lost")
        // Both blocks are stamped, so the model can count what it received.
        #expect(followUp.contains("1 of 2 in batch "), "the batch stamp is missing")
        #expect(followUp.contains("2 of 2 in batch "))
    }

    @Test func theErrorPromptsAreVerbatim() async throws {
        // These strings are PROMPTS, not diagnostics — the only thing a model has
        // to go on when a call it wrote did not run. Pinned as literal text so a
        // "clearer" rewording in one runtime shows up as a failing test rather
        // than as cross-runtime drift the model pays for.
        #expect(
            NO_TARGET_ERROR == """
                no target_id, so nothing was dispatched — a send must name the AGENT it \
                goes to. Shape: <call name="send"><target_id>agent</target_id>\
                <type>verb</type></call>. Send `kernel` <type>reflect</type> for the \
                list of agent ids.
                """)
        #expect(
            noPayloadError(target: "totals") == """
                send to 'totals' carried no payload, so nothing was dispatched. A call \
                names its verb in `<type>`, e.g. <type>reflect</type>.
                """)
        // The malformed report is composed in `Malformed.swift`; its opening
        // sentence is the part the model keys on.
        let report = malformedCalls(#"<call name="send"><type>reflect</type></call>"#)
        #expect(report.count == 1)
        #expect(
            report[0].error.hasPrefix("call NOT dispatched: a send names its target") == true)
        #expect(report[0].error.contains("Nothing ran.") == true)
    }

    // ─── the tool set is CLOSED ─────────────────────────────────

    @Test func aCallNamingSomethingThatIsNotAToolIsRefusedNotTranslated() async throws {
        // The JSON grammar translated `{"name":"reflect"}` into a send, because a
        // model that compressed verb and tool into one name still meant something.
        // The tag grammar does not: `TOOLS` is CLOSED (send/done/await/
        // inquiry_answer), the verb rides in `<type>`, and a head naming anything
        // else is a refusal the model can read.
        let call = #"<call name="reflect"><target_id>probe</target_id></call>"#
        let script = ProviderScript([[call], ["ok"]])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the refusal never reached the model")
        #expect(script.requestText(1).contains("`reflect` is not a tool"))
        #expect(await recorder.payloads().isEmpty, "nothing may dispatch from a bad head")
    }

    @Test func aCallWrittenMidThoughtStillDispatches() async throws {
        // The routing half of "thinking is PARSED, not PERSISTED". The RAW
        // completion is what routing reads, so a call the model wrote inside its
        // deliberation still runs; only the DURABLE turn is stripped. Reading the
        // stripped text instead would drop the call and end the chain silently —
        // the gemma4:12b failure (2026-08-02) in a different disguise.
        let call = callEnvelope(target: "core", payload: .object(["type": .string("list_agents")]))
        let raw =
            "\(THINK_OPEN)Let me work out which agent stores data.\(THINK_CLOSE)" + call + "on it"
        let script = ProviderScript([[raw], ["done"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(
            await script.waitForCalls(2), "a call inside thinking must still dispatch")
        #expect(script.requestText(1).contains(" · core "))
        // And the deliberation is not in the durable record.
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        let msgs = await historyRows(kernel: kernel, id: id, client: "c")
        #expect(msgs[1]["content"].asString?.contains("Let me work out") == false)
        #expect(msgs[1]["content"].asString?.contains("on it") == true)
    }

    // ─── the caller's one-shot handoff ──────────────────────────

    @Test func callerSendToHandsOffOnce() async throws {
        // `send_to` delivers this pass's TEXT to a peer — once. It must NOT
        // propagate: a caller cannot spend the model's turns for it.
        //
        // It rides the DONE turn, and only that one: a call-less turn is nudged
        // now rather than ended, so the hand-off would otherwise never fire.
        let a = ProviderScript([["the answer from a " + doneEnvelope], ["a must not run again"]])
        let b = ProviderScript([["b replied " + doneEnvelope], ["b must not run again"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": a, "peer": b]), backendIds: ["mock", "peer"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("probe_client"), "send_to": .string("peer"),
            ]))])[0].result
        #expect(await b.waitForCalls(1), "the peer never received the reply")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(b.requestText(0).contains("the answer from a"))
        #expect(a.calls == 1 && b.calls == 1, "send_to must not propagate")
    }

    @Test func sendToIsIgnoredWhenTheModelRoutedItself() async throws {
        // The model's own calls WIN. `send_to` is the fallback for a pass that
        // named nobody; a pass that named someone has already said where its work
        // goes, and delivering the text to the caller's peer as well would
        // duplicate the turn.
        let mine = ProviderScript([
            [callEnvelope(target: "core", payload: cheapReflect)],
            ["ok"],
        ])
        let theirs = ProviderScript([["must never run"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": mine, "peer": theirs]),
            backendIds: ["mock", "peer"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                "send_to": .string("peer"),
            ]))])[0].result
        #expect(await mine.waitForCalls(2), "the model's own call must still route")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(theirs.calls == 0, "send_to must not fire when the model routed itself")
    }

    @Test func errorReplyDoesNotRoute() async throws {
        // A failed pass is terminal — a broken chain must not keep spending turns.
        // `runStream` returns nil at every non-ok exit, which is python's
        // `if not reply.get("error")` gate expressed in the type.
        let script = ProviderScript([
            [callEnvelope(target: "core", payload: cheapReflect)],
            ["must never run"],
        ])
        var cfg = scriptedConfig(["mock": script])
        cfg.makeProvider = { _, _, _ in
            .provider(MockProvider(script: script, throwsError: true))
        }
        let (kernel, ids, _) = await bootMockKernel(config: cfg, backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("probe_client"),
            ]))])[0].result
        try await Task.sleep(nanoseconds: 400_000_000)
        #expect(script.calls == 1)
    }

    // ─── the delivered turn's own shape ─────────────────────────

    @Test func theSelfChainCarriesNoCallStack() async throws {
        // Stops someone "tightening" the recursion guard and killing the chain.
        // A delivered turn is a PLAIN `send` — `{type, text, client_id}` and the
        // caller's `system_prompt` when there was one. It carries NO `_call_stack`,
        // exactly as python's `_deliver` does not: a `reply_to:'self'` chain would
        // otherwise read as a cycle on turn 2 and be refused, which is the one
        // thing this whole path exists to make possible.
        let script = ProviderScript([
            [
                callEnvelope(target: "core", payload: cheapReflect, replyTo: .some("probe"))
            ]
        ])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await recorder.waitForPayloads(1), "the reply never reached reply_to")
        let turn = recorder.payloads()[0]
        #expect(turn["type"].asString == "send")
        #expect(turn["client_id"].asString == "c")
        #expect(turn["text"].asString?.contains(" · core ") == true)
        #expect(turn.asObject?["_call_stack"] == nil, "routing must not propagate _call_stack")
        // No caller prompt was supplied, so no `system_prompt` is pinned onto the
        // hop — each hop rebuilds the default block from the live tree.
        #expect(turn.asObject?["system_prompt"] == nil)
    }

    @Test func routingIsItsOwnTaskRow() async throws {
        // `ps` must not label routing as `"send"`. A separate row is what makes
        // each hop of a chain individually visible and individually killable — and
        // that is the honest claim, not "cancel kills a chain": orphan re-parenting
        // means killing hop 1 does not reach hop 3.
        //
        // `slowpoke` sleeps inside its handler, so routing — which AWAITS each call
        // it dispatches — is still standing when `ps` is read.
        let script = ProviderScript([
            [callEnvelope(target: "slowpoke", payload: .object(["type": .string("work")]))],
            ["ok"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        var seen: [String] = []
        for _ in 0..<100 {
            seen = await psRows(kernel)
                .filter { $0["agent"].asString == id.value }
                .compactMap { $0["verb"].asString }
            if seen.contains("route_reply") { break }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        #expect(seen.contains("route_reply"), "routing had no row of its own: \(seen)")
        #expect(await script.waitForCalls(2), "and the slow call still came back")
    }

    // ─── the emergence discipline, guarded ──────────────────────

    /// The SHORT how-to, for the one backend with a hard 4096-token window (the
    /// on-device Apple FM model). Length is the only thing cut: every mechanism
    /// the model cannot work without has to survive, and no strategy may creep
    /// in under cover of "it is the compact one".
    @Test func theCompactHowToTeachesTheSameMechanisms() {
        for needle in [
            #"<call name="send">"#,  // the envelope
            "<target_id>", "<type>", "<reply_to>",  // addressing
            "<item>",  // lists
            "<bytes>", "first `</bytes>`",  // the verbatim document
            #"<call name="done">"#, #"<call name="await">"#,  // stopping and waiting
            "[reply to <pid>", "ASYNC",  // how replies arrive
        ] {
            #expect(SEND_TO_HOWTO_COMPACT.contains(needle), "the compact block dropped \(needle)")
        }
        // It is genuinely short — the whole point — and it is NOT the long one.
        #expect(SEND_TO_HOWTO_COMPACT.count < SEND_TO_HOWTO.count / 2)
        for banned in ["loop", "iterate", "step by step", "keep going", "final answer"] {
            #expect(!SEND_TO_HOWTO_COMPACT.lowercased().contains(banned), "strategy leaked: \(banned)")
        }
    }

    @Test func promptStatesTheMechanismNotAStrategy() async throws {
        // The default system block may state FACTS — how to reach an agent, what
        // reflect returns, where `reply_to` sends the answer. It must NOT name the
        // STRATEGY.
        //
        // The removed `_SEND_HOWTO` (recoverable at 2ce23d1) did exactly that:
        // "either call again or write your final answer to finish", "ORIENT FIRST",
        // "NEVER claim you don't have access". That is the loop, dictated — reusing
        // that wording because it is "tuned" would quietly void every emergence
        // result taken afterwards.
        //
        // Asserted on what the PROVIDER was actually handed, not on the builder's
        // return value: the prompt the model reads is the only one that counts.
        let script = ProviderScript([["hi"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("hi"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(1))
        let messages = script.request(0) ?? []
        #expect(messages.first?["role"].asString == "system")
        let sysBlock = (messages.first?["content"].asString ?? "").lowercased()

        // the mechanism IS stated
        #expect(sysBlock.contains("reply_to"))
        #expect(sysBlock.contains("reflect"))
        #expect(sysBlock.contains(#"<call name="send">"#))

        let banned = [
            "loop",
            "iterate",
            "iteration",
            "step by step",
            "keep going",
            "until done",
            "until you are done",
            "agentic",
            "continue working",
            "call again",
            "final answer",
            "to finish",
            "orient first",
        ]
        let leaked = banned.filter { sysBlock.contains($0) }
        #expect(leaked.isEmpty, "system prompt leaks strategy \(leaked) — voids the test")
    }

    // ─── the end-of-chain contract: DONE is explicit; silence is nudged ───
    // Reinstated 2026-09-01 (mirrors python `test_done_and_nudge.py`).

    @Test func silentChainTurnsAreNudgedUntilACallOrDone() async throws {
        // The model is QUERIED UNTIL IT CALLS (python-canonical, 2026-09-01): a
        // second silent turn used to end the chain silently.
        let script = ProviderScript([
            [callEnvelope(target: "core", payload: cheapReflect)],
            ["thinking out loud, no envelope"],
            ["still just prose"],
            ["ok, finishing"],
            [doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(5), "the chain did not reach done")
        for n in [2, 3, 4] {
            #expect(
                script.requestText(n).contains("[no call was dispatched] Your turn emitted no"),
                "pass \(n + 1) must be a nudge")
        }
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 5, "done ended it; nothing reminds after")
    }

    @Test func nudgeLimitBoundsANeverCallingChain() async throws {
        let script = ProviderScript([
            [callEnvelope(target: "core", payload: cheapReflect)],
            ["prose 1"],
            ["prose 2"],
            ["prose 3"],
            ["prose 4 — would be nudged without the limit"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("update_agent"), "id": .string(ids["mock"]!.value),
                "nudge_limit": .integer(2),
            ]))])[0].result
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(4), "two nudges must arrive")
        try await Task.sleep(nanoseconds: 300_000_000)
        // call, silent→nudge 1, silent→nudge 2, silent→over the limit: 4, never 5
        #expect(script.calls == 4)
    }

    @Test func aDoneCallEndsTheChainWithoutANudge() async throws {
        let script = ProviderScript([
            [callEnvelope(target: "core", payload: cheapReflect)],
            ["finished. " + doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2))
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 2, "done must end the chain silently")
    }

    /// EVERY call-less turn is nudged, chain or not (python-canonical; user,
    /// 2026-09-09: "all is tool calls"). The nudge used to fire only when the
    /// incoming text was itself a reply or a nudge, so a FIRST turn of pure prose
    /// from a person ended the work silently — the model had decided on a call,
    /// written it in thought, and never emitted it.
    @Test func aFirstTurnWithNoCallIsNudgedToo() async throws {
        let script = ProviderScript([["just an answer, no calls"], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("hi"),
                "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the first silent turn was not nudged")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 2, "and `done` ends it")
        #expect(script.requestText(1).contains("[no call was dispatched]"))
    }
    // ─── the batch: one deadline, one stamp, one token per turn ─

    /// EVERY dispatched model call carries the AI layer's per-call deadline. The
    /// KERNEL defaults nothing, so a call to a handler that never returns used to
    /// hang the whole batch forever: no reply, no timeout block, the chain dead with
    /// nothing to read. Python has always set it; rust/swift did not until
    /// 2026-09-02.
    @Test func everyModelCallCarriesTheLayerDefaultTimeout() async throws {
        // `slowpoke` takes 500ms. The layer's deadline is 100ms, and NO `timeout`
        // arg is written in the envelope — the default has to do the work.
        let script = ProviderScript([["read it " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        await routeReply(
            selfId: id, kernel: kernel,
            text: callEnvelope(target: "slowpoke", payload: .object(["type": .string("crawl")])),
            clientId: "c", sendTo: nil, systemPrompt: nil, incoming: "",
            sendTimeout: 0.1)
        #expect(
            await script.waitForCalls(1),
            "the timed-out call produced no block at all — the batch hung")
        #expect(
            script.requestText(0).contains("timed out after 0.1s"),
            "the block must REPORT the timeout: \(script.requestText(0).suffix(400))")
    }

    /// A turn whose calls all carried `reply_to:null` DISPATCHED REAL WORK. It is
    /// owed no reply, and it is not a silent turn: gating the nudge on the blocks
    /// instead of python's `not calls and not units` tells the model "your turn
    /// emitted no <call> envelope" about a turn that did — and, on a chat turn,
    /// hands that text to `send_to` on top.
    @Test func allFireAndForgetCallsAreNotASilentTurn() async throws {
        let fireAndForget = callEnvelope(
            target: "probe", payload: .object(["type": .string("noted")]), replyTo: .some(nil))
        let a = ProviderScript([
            [fireAndForget + "\n" + fireAndForget], ["the nudge would land here"],
        ])
        let b = ProviderScript([["the handoff would land here"]])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": a, "peer": b]), backendIds: ["mock", "peer"])
        // A CHAIN turn (its incoming text is a delivered reply) — exactly the turn
        // the nudge exists for, so nothing but the dispatch can keep it quiet.
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"),
                "text": .string("[reply to core:1 · core ping]\nok"),
                "client_id": .string("c"),
                "send_to": .string("peer"),
            ]))])[0].result
        #expect(await recorder.waitForPayloads(2), "both fire-and-forget calls must RUN")
        try await Task.sleep(nanoseconds: 400_000_000)
        #expect(a.calls == 1, "a dispatching turn must not be nudged")
        #expect(b.calls == 0, "and must not fall through to send_to")
    }

    /// The batch stamp lives INSIDE the `[reply to …]` tag, where the shared prompt
    /// says it does. It rode on its own trailing line in rust/swift until 2026-09-02,
    /// which python's history trimmers (`_REPLY_TAG` / `_BLOCK_SPLIT`, both anchored
    /// at the START of a block) cannot read at all.
    @Test func theBatchStampLivesInsideTheReplyTag() async throws {
        let call = callEnvelope(target: "core", payload: .object(["type": .string("list_agents")]))
        let script = ProviderScript([[call + "\n" + call], ["both back " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the batch never came back")
        let turn2 = script.requestText(1)
        #expect(
            turn2.contains(" · 1 of 2 in batch ") && turn2.contains(" · 2 of 2 in batch "),
            "the stamp must be composed into the tag: \(turn2.suffix(500))")
        #expect(
            !turn2.contains("[1 of 2 in batch"),
            "no trailing stamp line may survive: \(turn2.suffix(500))")
    }

    /// Read a batch token out of a delivered turn. Anchored on the STAMP, not on the
    /// words: the system block teaches the shape, so a bare `in batch ` search finds
    /// the prompt's own `<token>` placeholder first.
    private func batchToken(_ turn: String) -> String {
        guard let r = turn.range(of: "1 of 2 in batch ") else {
            Issue.record("no batch stamp in the delivered turn")
            return ""
        }
        return String(turn[r.upperBound...].prefix(4))
    }

    /// A FRESH token per turn. Rust/swift derived it from the batch's own content
    /// (destinations, counts, body lengths), so two turns that made the SAME calls
    /// collided — which is the exact confusion the token exists to end: a model that
    /// read one file twice saw `1 of 2 in batch 7f3a` both times (measured
    /// 2026-08-24, the re-read loop).
    @Test func twoIdenticalTurnsGetDifferentBatchTokens() async throws {
        let call = callEnvelope(target: "core", payload: .object(["type": .string("list_agents")]))
        let twice = call + "\n" + call
        let script = ProviderScript([[twice], [twice], ["enough " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(3), "the second batch never landed")
        #expect(
            batchToken(script.requestText(1)) != batchToken(script.requestText(2)),
            "two identical turns must be distinguishable at a glance")
    }

    /// A FAILED HANDOFF is announced as an ordinary `status` event — `detail.reason`,
    /// the shape reflect's `emits` documents and a UI subscribes to. Rust/swift put
    /// `reason`/`error` at the TOP level until 2026-09-02, so every subscriber saw
    /// nothing: the silent-death regression the announcement exists to end, wearing
    /// the announcement's own clothes.
    @Test func aFailedHandoffIsAStatusEventWithDetail() async throws {
        // `raiser` throws on every real verb, so the delivered turn cannot land.
        let script = ProviderScript([
            [callEnvelope(
                target: "core", payload: .object(["type": .string("list_agents")]),
                replyTo: .some("raiser"))]
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let sink = EventSink()
        let inbox = kernel.ensureInbox(AgentId("raiser"))
        let pump = Task { for await ev in inbox { sink.add(ev) } }
        defer { pump.cancel() }
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        try await waitUntil("the dead handoff to be announced") {
            sink.first(type: "status") != nil
        }
        let status = sink.first(type: "status") ?? .null
        #expect(status["phase"].asString == "done", "\(status.serialize())")
        #expect(status["detail"]["reason"].asString == "error", "\(status.serialize())")
        #expect(
            status["detail"]["error"].asString != nil,
            "the announcement must say WHY: \(status.serialize())")
        #expect(status["source"].asString == "raiser", "\(status.serialize())")
        #expect(status["ts"].asDouble != nil, "\(status.serialize())")
        #expect(status["client_id"].asString == "c", "\(status.serialize())")
    }

    /// A `reflect` reply is LARGE and TRANSIENT, and the instruction for what to
    /// record before it goes rides ON THE REPLY — measured 2026-08-20 (`live_plan`):
    /// stated once as system-block policy, the model wrote a `SEEN` line for 0 of 10
    /// reflects. Only reflect bodies are dropped later, so only they carry it.
    @Test func reflectRepliesCarryTheDistillationNudge() async throws {
        let reflect = callEnvelope(target: "core", payload: cheapReflect)
        let list = callEnvelope(target: "core", payload: .object(["type": .string("list_agents")]))
        let script = ProviderScript([
            [reflect + "\n" + list], ["SEEN core " + doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the reflect never came back")
        let turn2 = script.requestText(1)
        #expect(
            turn2.contains("this body is TRANSIENT"),
            "the reflect block must carry the distillation nudge")
        #expect(turn2.contains("SEEN <id>"), "and must show the format it asks for")
        #expect(
            turn2.components(separatedBy: "this body is TRANSIENT").count == 2,
            "and ONLY the reflect block carries it")
    }
}
