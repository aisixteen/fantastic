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
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 1, "done must end the turn, got \(script.calls) passes")
        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 2)
        #expect(msgs[1]["content"].asString?.hasPrefix("the answer") == true)
    }

    // ─── the model acts, and sees the result ────────────────────

    @Test func modelCallIsDispatchedAndTheResultReturns() async throws {
        // The model emits a send; it really runs; its REPLY comes back as the next
        // turn, tagged so the model can tell it from a human's message.
        let script = ProviderScript([
            [callEnvelope(target: "kernel_state", payload: .object(["type": .string("list_agents")]))],
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
        #expect(turn2.contains(" · kernel_state "), "untagged reply: \(turn2.suffix(400))")
        #expect(turn2.contains("agents"), "the real list_agents result must be in the turn")
    }

    @Test func selfAddressedFollowUpSeesThePersistedTurn() async throws {
        // Swift's stand-in for python's deadlock regression, and a stronger claim.
        // A 3-pass chain must run to completion AND pass 3's prompt must contain
        // pass 1's assistant text — which is only true if, for every hop, the
        // assistant turn was SAVED before the FIFO guard was RELEASED before
        // routing started. Get that order wrong and the chain still "works" while
        // quietly answering questions from a history that is one turn behind.
        let reflect = callEnvelope(target: "kernel_state", payload: cheapReflect)
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
        try await waitForHistory(kernel: kernel, id: id, count: 6)
        #expect((conversationRows(kernel: kernel, id: id)).count == 6)
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
                        target: "kernel_state", payload: .object(["type": .string("reflect")]),
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
        // what is asserted here is that the caught body reaches the model whole.
        // Routing adds NOTHING to it (B1.26): the tag line already names the
        // target, and python renders `render_reply(result)` unchanged.
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
        // B5.20: `{target!r}` is python's repr, not "wrap it in single quotes".
        #expect(noPayloadError(target: "it's").hasPrefix("send to \"it's\" carried"))
        #expect(noPayloadError(target: #"C:\notes"#).hasPrefix(#"send to 'C:\\notes' carried"#))
        #expect(noPayloadError(target: "tab\there").hasPrefix(#"send to 'tab\there' carried"#))
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
        let call = callEnvelope(target: "kernel_state", payload: .object(["type": .string("list_agents")]))
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
        #expect(script.requestText(1).contains(" · kernel_state "))
        // And the deliberation is not in the durable record.
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        let msgs = conversationRows(kernel: kernel, id: id)
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
            [callEnvelope(target: "kernel_state", payload: cheapReflect)],
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
            [callEnvelope(target: "kernel_state", payload: cheapReflect)],
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
                callEnvelope(target: "kernel_state", payload: cheapReflect, replyTo: .some("probe"))
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
        #expect(turn["text"].asString?.contains(" · kernel_state ") == true)
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
            [callEnvelope(target: "kernel_state", payload: cheapReflect)],
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

    // B1.3: THERE IS NO STREAK AND NO CAP. A `nudge_limit` on the record buys
    // nothing — every call-less turn is nudged, and the chain ends where the
    // MODEL says it ends. Python `test_silent_chain_turns_are_nudged_until_a_call_or_done`.
    @Test func everySilentTurnIsNudgedWithNoCap() async throws {
        let script = ProviderScript([
            [callEnvelope(target: "kernel_state", payload: cheapReflect)],
            ["prose 1"],
            ["prose 2"],
            ["prose 3"],
            ["prose 4 — nudged too, there is no limit"],
            ["finished. " + doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        // A `nudge_limit` of 2 used to stop the chain at pass 4. It is not read.
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("update_agent"), "id": .string(ids["mock"]!.value),
                "nudge_limit": .integer(2),
            ]))])[0].result
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(6), "every silent turn must be nudged")
        for n in [2, 3, 4, 5] {
            #expect(
                script.requestText(n).contains("[no call was dispatched] Your turn emitted no"),
                "pass \(n + 1) must be a nudge")
        }
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 6, "done ended it; nothing reminds after")
    }

    /// A turn that is ONLY thinking did not answer: it is never stored and never
    /// counts as completed — it is rerolled by the no-call reminder, told why
    /// (measured on vLLM 2026-09-30: an 8-token thinking budget, then a clean
    /// `stop` with no answer; failing it outright ended a 30-pass chain). Python
    /// `test_a_dead_turn_is_discarded_and_rerolled`.
    @Test func aDeadTurnIsDiscardedAndRerolled() async throws {
        let script = ProviderScript([["\(THINK_OPEN)hmm\(THINK_CLOSE)\n\n"], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let out = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))])[0]
        #expect(out.result["dead_turn"].asBool == true, "\(out.result)")
        #expect(await script.waitForCalls(2), "the dead turn was not rerolled")
        let reroll = script.requestText(1)
        #expect(
            reroll.contains("[no answer] Your last turn produced only thinking and no answer"),
            "\(reroll.suffix(400))")
        #expect(reroll.contains("discarded. [no call was dispatched]"), "\(reroll.suffix(400))")
        // the turn the dead one answered is KEPT (a reply batch must not vanish);
        // only its empty answer is dropped — python records `seen` per pass
        let seen = script.request(1) ?? []
        #expect(
            seen.contains { m in
                let c = m["content"].asString ?? ""
                return c == "go" || c.hasSuffix("go")
            }, "\(seen)")
        #expect(
            !seen.contains { m in
                m["role"].asString == "assistant"
                    && pyStrip(m["content"].asString ?? "").isEmpty
            }, "\(seen)")
    }

    /// Silent turns in a row are bounded: at MAX_SILENT_TURNS the chain stops
    /// reminding and tells its caller the model could not answer. Python
    /// `test_a_chain_gives_up_after_max_silent_turns` (cap lowered to 3 for THIS
    /// kernel only — the test seam, not the constant).
    @Test func aChainGivesUpAfterMaxSilentTurns() async throws {
        let script = ProviderScript(Array(repeating: ["just prose"], count: 10))
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        AISilentStreaks.setCap(kernel, 3)
        let id = ids["mock"]!
        // a browser-style client: its events land on the agent's own inbox,
        // where a watcher sees them (`cli` events go to the terminal instead)
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("tab1"),
            ]))])[0]
        #expect(await script.waitForCalls(3))
        let gaveUp: () -> JSON? = {
            sink.snapshot().last {
                $0["type"].asString == "status" && $0["phase"].asString == "done"
                    && $0["detail"]["reason"].asString == "gave_up"
            }
        }
        try await waitUntil("status(done, gave_up)") { gaveUp() != nil }
        try await Task.sleep(nanoseconds: 500_000_000)
        #expect(script.calls == 3, "no reminder after the cap")
        let detail = try #require(gaveUp()?["detail"])
        #expect(
            detail["error"].asString
                == "The LLM was not able to answer — 3 turns in a row produced no call. "
                + "Reiterate the inquiry.")
        #expect(detail["silent_turns"].asInt == 3)
    }

    @Test func aDoneCallEndsTheChainWithoutANudge() async throws {
        let script = ProviderScript([
            [callEnvelope(target: "kernel_state", payload: cheapReflect)],
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
            script.requestText(0).contains(
                "send: slowpoke 'crawl' exceeded its 0.1s timeout; task cancelled"),
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
        let call = callEnvelope(target: "kernel_state", payload: .object(["type": .string("list_agents")]))
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

    /// EVERY REPLY NAMES THE TURN THAT MADE THE CALL. The tag carries `↑<turn>` —
    /// the assistant row's id — and the batch token IS that same id, so `in batch
    /// <b>` and `↑<turn>` name one row the model can see. The delivered turn's own
    /// row then takes that id as its `parent`: a thread is the parent chain.
    /// Python `test_row_ids.py` (`_reply_tag`, `_row_ids_for_incoming`).
    @Test func aReplyTagNamesItsTurnAndTheDeliveredRowTakesItAsParent() async throws {
        let call = callEnvelope(target: "kernel_state", payload: .object(["type": .string("list_agents")]))
        let script = ProviderScript([[call + "\n" + call], ["both back " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        let r = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        let turn = try #require(r["message_id"].asInt, "send answers the assistant row's id: \(r.serialize())")
        #expect(await script.waitForCalls(2), "the batch never came back")
        let turn2 = script.requestText(1)
        #expect(turn2.contains(" \u{b7} \u{2191}\(turn) \u{b7} 1 of 2 in batch \(turn)]"), "\(turn2.suffix(400))")
        #expect(turn2.contains(" \u{b7} \u{2191}\(turn) \u{b7} 2 of 2 in batch \(turn)]"), "\(turn2.suffix(400))")
        try await waitForHistory(kernel: kernel, id: id, count: 4)
        let rows = conversationRows(kernel: kernel, id: id)
        #expect(rows[1]["id"].asInt == turn, "the assistant row IS the turn the tags name")
        #expect(rows[2]["parent"].asInt == turn, "the delivered reply turn names its parent from the tag")
        #expect(rows[3]["parent"].asInt == rows[2]["id"].asInt)
        #expect(rows[0]["parent"].isNull, "an external message is a root")
        // The parser behind it, on python's own fixture.
        #expect(incomingParent("[reply to 40 \u{b7} notes read \u{b7} \"read the spec\" \u{b7} \u{2191}17]\nok") == 17)
        #expect(incomingParent("please survey the corpus") == nil)
    }

    /// THE MODEL'S `<title>` RIDES THE REPLY TAG — `[reply to <pid> · <target> <verb>
    /// · "<title>" …]` — so a reply names what it answered forever, in the store
    /// and in every digest. Swift built the dispatched call's route unit without
    /// it (measured 2026-09-13: every swift reply tag read `[reply to N · notes
    /// read · ↑T]` while python/rust carried `· "read filler_1.txt (2 of 8)"`).
    @Test func theCallTitleRidesTheReplyTag() async throws {
        let call = """
            <call name="send"><target_id>core</target_id><title>list the roster (1 of 1)</title>\
            <type>list_agents</type></call>
            """
        let script = ProviderScript([[call], ["seen " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([Item(ids["mock"]!, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the reply never came back")
        try await waitForHistory(kernel: kernel, id: ids["mock"]!, count: 4)
        // The delivered reply is the second user row — read RAW off the store,
        // not through the serialized request text (where quotes are escaped).
        let delivered = conversationRows(kernel: kernel, id: ids["mock"]!)
            .filter { $0["role"].asString == "user" }.dropFirst().first?["content"].asString ?? ""
        #expect(delivered.contains(" \u{b7} core list_agents \u{b7} \"list the roster (1 of 1)\" \u{b7} \u{2191}"), "\(delivered.prefix(200))")
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
        let call = callEnvelope(target: "kernel_state", payload: .object(["type": .string("list_agents")]))
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
                target: "kernel_state", payload: .object(["type": .string("list_agents")]),
                replyTo: .some("raiser"))]
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let sink = EventSink()
        let inbox = kernel.ensureInbox(AgentId("raiser"))
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
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
        // B2.4: the announcement goes through the SHARED emitter, so it carries
        // the target's `queue_depth` like every other status event, and its
        // `error` is the reply's own — never a `delivery <status>` sentence
        // python does not write.
        #expect(
            status["detail"].asObject.map { Array($0.keys) } == ["reason", "error", "queue_depth"],
            "\(status.serialize())")
        #expect(
            status["detail"]["error"].asString?.hasPrefix("delivery ") == false,
            "\(status.serialize())")
    }

    /// B2.4: a CANCELLED handoff is not announced. An operator kill is a
    /// decision, not a silent death, and waking the target with an error for
    /// work someone deliberately stopped is noise. Swift announced every
    /// non-completed outcome, cancels included.
    @Test func aCancelledHandoffIsNotAnnounced() async throws {
        // A slow destination, killed through the front door while the handoff is
        // in flight: the outcome is `cancelled` with `{cancelled:true}` and no
        // error at all.
        let (kernel, _, _) = await bootStubKernel(deliveryPauseNanos: 2_000_000_000)
        await seedStub(kernel, "ai", "delivery.tools")
        let sink = EventSink()
        let inbox = kernel.ensureInbox(AgentId("ai"))
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }
        let handoff = Task {
            await deliverTurn(
                kernel: kernel, target: "ai", text: "hi", clientId: "c", systemPrompt: nil)
        }
        let taskId = try await waitForTask(kernel: kernel, agent: AgentId("ai"), verb: "send")
        let cancel: JSON = .object(["type": .string("cancel"), "task_id": .integer(taskId)])
        let c = await kernel.send([Item(AgentId("kernel"), cancel)])[0].result
        #expect(c["cancelled"].asBool == true)
        await handoff.value
        #expect(
            !sink.snapshot().contains { $0["type"].asString == "status" },
            "an operator kill is a decision, not a silent death to announce")
    }

    /// B2.5: a delivery is a SPAWNED kernel task (`verb:"deliver_reply"`), not
    /// an awaited call. Awaiting each one inside the router's loop made reply 2
    /// of a 4-call turn wait for the whole conversation reply 1 started — 26 of
    /// 36 calls never answered, measured 2026-08-24 (`live_build`).
    @Test func deliveriesRunAsDeliverReplyTasks() async throws {
        let (kernel, _, sent) = await bootStubKernel(deliveryPauseNanos: 300_000_000)
        await seedStub(kernel, "ai", "delivery.tools")
        await seedStub(kernel, "meter", "echo.tools")
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: stubCall("meter"), clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 180.0)

        // The router returned while the delivery is still running…
        #expect(sent.messages().isEmpty, "the router must not wait for the handoff")
        // …and the work is a `ps` row, so it is visible and killable.
        let rows = await psRows(kernel)
        #expect(
            rows.contains {
                $0["agent"].asString == "ai" && $0["verb"].asString == "deliver_reply"
            }, "\(JSON.array(rows).serialize())")
        // C36 — and it is a `task` ROW on the AI, under the same id, so the
        // handoff is in the message log and not only in the live table.
        let stored = kernel.rows.all().filter { $0["agent"]?.asString == "ai" }
        #expect(
            stored.contains {
                $0["kind"]?.asString == "task" && $0["verb"]?.asString == "deliver_reply"
            }, "no deliver_reply task row: \(stored.map { JSON.object($0).serialize() })")
        #expect(await sent.settled(1).count == 1)
    }

    /// A `reflect` reply is LARGE and TRANSIENT, and the instruction for what to
    /// record before it goes rides ON THE REPLY — measured 2026-08-20 (`live_plan`):
    /// stated once as system-block policy, the model wrote a `SEEN` line for 0 of 10
    /// reflects. Only reflect bodies are dropped later, so only they carry it.
    /// SYNC OR ASYNC ALIKE (decided 2026-09-26): a reply over the inline cap is
    /// held in RAM under its pid (never persisted) and the model is handed
    /// python's stub — the exact bytes, its ORIGIN line, grep named — with the
    /// size counted AFTER the reflect nudge rode on. The body stays one `grep`
    /// away for this run.
    @Test func anOverCapReplyIsStoredAndDeliveredAsTheStub() async throws {
        // A window whose inline cap (an eighth of the budget) is smaller than the
        // mock's own reflect + nudge, so that reply is the one that dehydrates.
        let reflectSelf = callEnvelope(target: "mock", payload: .object(["type": .string("reflect")]))
        let script = ProviderScript([[reflectSelf], ["SEEN mock " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"],
            contextWindow: 8192, meta: ["context_async": .bool(true)])
        let id = ids["mock"]!
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the reflect never came back")
        try await waitForHistory(kernel: kernel, id: id, count: 3)
        // HELD IN RAM, never persisted — what the stored turn keeps is the
        // origin line.
        let held = heldBodies(id.value, kernel)
        let firstHeld = held.elements.first
        let (pid, body) = try #require(firstHeld.map { ($0.key, $0.value) }, "the over-cap reply must be HELD")
        #expect(body.hasSuffix(REFLECT_NUDGE), "the nudge is inside the held body, so it is inside the size")
        #expect(
            AIContext.estimateOne(.object(["role": .string("user"), "content": .string(body)]))
                > AIContext.inlineCap(kernel.agent(id)!))
        let stub =
            "(settled, \(body.unicodeScalars.count) bytes — NOT inlined, held in memory only "
            + "until this agent restarts. origin: mock reflect {} — re-send it; the output may differ. "
            + "To find a line in it: send yourself <type>grep</type> <query>substring</query> "
            + "<ids><item>\(pid)</item></ids>)"
        let delivered = conversationRows(kernel: kernel, id: id)[2]["content"].asString ?? ""
        #expect(delivered.hasPrefix("[reply to \(pid) · mock reflect"), Comment(rawValue: delivered))
        #expect(delivered.hasSuffix("]\n" + stub), Comment(rawValue: delivered))
        #expect(!delivered.contains("this body is TRANSIENT"), "the body itself never rode the turn")
        // and the body is one grep away, by the pid the stub names
        let g = await kernel.send([Item(id, .object([
                "type": .string("grep"), "query": .string("TRANSIENT"), "ids": .array([.integer(pid)]),
            ]))])[0].result
        #expect(g["total"].asInt == 1 && g["matches"][0]["id"].asInt == pid, Comment(rawValue: g.serialize()))
    }

    @Test func reflectRepliesCarryTheDistillationNudge() async throws {
        let reflect = callEnvelope(target: "kernel_state", payload: cheapReflect)
        let list = callEnvelope(target: "kernel_state", payload: .object(["type": .string("list_agents")]))
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

    // ─── every call in a turn gets its reply — delivered TOGETHER ──
    // Mirrors python `test_reply_delivery.py`. Two defects pinned there, both
    // measured live: 2026-08-24 (`live_build`) replies were delivered
    // SEQUENTIALLY and each delivery blocked on a whole model turn — 26 of 36
    // calls never answered; 2026-08-25 (`live_subai_langgraph`) one reply per
    // turn — a turn that made 7 calls spent the next 7 turns collecting its own
    // answers, asked `list_agents` at t3, received the t1 reflect instead, and
    // re-asked until the repeat tripwire killed the run with nothing built.
    //
    // Python drives `_route_reply` against an ARRAY-SHAPED KERNEL STUB (one
    // outcome per item, `sent` = the delivered `type:send` handoffs). Swift has
    // no stub seam — `routeReply` takes a real `Kernel` — so the stub's two
    // halves are two bundles on a bare in-memory kernel: `EchoBundle` answers
    // `{ok:<id>}` and records what it answered (the stub's `batches`),
    // `DeliveryRecorder` records `(agent, payload)` per destination (the stub's
    // `sent`). Delivery is AWAITED inside `routeReply` here, so there is no
    // `_settle` poll: when the call returns, every message has landed.

    @Test func aTurnOfFourCallsIsAnsweredInOneMessage() async throws {
        // Four calls out → ONE delivered message holding four tagged blocks, in
        // the model's own emission order.
        //
        // "ONE kernel batch" is python's `k.batches == [[…]]`. The kernel
        // launches every item of a batch before its first await
        // (BatchSend.swift), so the honest twin is: all four rows are in `ps` AT
        // ONCE — which a serial dispatch (the 2026-08-24 shape) can never show.
        // The echo agents pause so the window is wide enough to read.
        let (kernel, echo, sent) = await bootStubKernel(echoPauseNanos: 200_000_000)
        await seedStub(kernel, "ai", "delivery.tools")
        let targets = ["meter", "readings", "totals", "runner"]
        for t in targets { await seedStub(kernel, t, "echo.tools") }
        let text = targets.map { stubCall($0) }.joined()
        let route = Task {
            await routeReply(
                selfId: AgentId("ai"), kernel: kernel, text: text, clientId: "cli",
                sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 180.0)
        }
        var inFlight: Set<String> = []
        for _ in 0..<100 {
            inFlight = Set(
                await psRows(kernel).filter { $0["verb"].asString == "ping" }
                    .compactMap { $0["agent"].asString })
            if inFlight.count == targets.count { break }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        #expect(inFlight == Set(targets), "all four calls must dispatch in ONE kernel batch: \(inFlight)")
        await route.value
        #expect(Set(echo.answered().map(\.0)) == Set(targets))

        let delivered = await sent.settled(1)
        #expect(delivered.count == 1, "one destination -> exactly one delivered message")
        let (target, payload) = try #require(delivered.first)
        #expect(target == "ai")
        let blocks = (payload["text"].asString ?? "").components(separatedBy: "\n\n")
        #expect(blocks.count == 4, Comment(rawValue: payload["text"].asString ?? ""))
        for (n, (block, agent)) in zip(blocks, targets).enumerated() {
            #expect(
                block.hasReplyTag("\(agent) ping \u{b7} \(n + 1) of 4 in batch "),
                "block \(n + 1) is not \(agent)'s: \(block.prefix(120))")
        }
    }

    @Test func twoDestinationsGetTwoMessagesWithTheirOwnCounts() async throws {
        // Per-call `reply_to` splits the batch: each destination's M counts only
        // what IT receives; both messages share the turn's batch token.
        let (kernel, _, sent) = await bootStubKernel()
        for d in ["ai", "scribe"] { await seedStub(kernel, d, "delivery.tools") }
        for t in ["meter", "totals", "readings"] { await seedStub(kernel, t, "echo.tools") }
        let text = stubCall("meter") + stubCall("totals", replyTo: "scribe") + stubCall("readings")
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: text, clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 180.0)

        let byDest = Dictionary(
            await sent.settled(2).map { ($0.0, $0.1["text"].asString ?? "") },
            uniquingKeysWith: { a, _ in a })
        #expect(Set(byDest.keys) == ["ai", "scribe"])
        let ai = byDest["ai"] ?? ""
        #expect(ai.contains(" \u{b7} 1 of 2 in batch "), Comment(rawValue: ai))
        #expect(ai.contains(" \u{b7} 2 of 2 in batch "), Comment(rawValue: ai))
        #expect(!(byDest["scribe"] ?? "").contains("in batch"), "a lone block carries no suffix")
        let token = ai.components(separatedBy: "in batch ").dropFirst().first?.prefix(4) ?? ""
        // the same turn token names the round on the multi-block side
        #expect(!token.isEmpty)
    }

    @Test func fireAndForgetStillDispatchesButIsNotCounted() async throws {
        // `reply_to:null` runs the call and discards the answer — it appears in
        // the kernel batch and in NO delivered message, and M ignores it.
        let (kernel, echo, sent) = await bootStubKernel()
        await seedStub(kernel, "ai", "delivery.tools")
        for t in ["meter", "totals"] { await seedStub(kernel, t, "echo.tools") }
        let text = stubCall("meter") + stubCall("totals", replyTo: .some(nil))
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: text, clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 180.0)

        #expect(Set(echo.answered().map(\.0)) == ["meter", "totals"], "the discarded call still ran")
        let delivered = await sent.settled(1)
        #expect(delivered.count == 1)
        let body = delivered.first?.1["text"].asString ?? ""
        #expect(!body.contains("totals"), Comment(rawValue: body))
        #expect(!body.contains("in batch"), "one awaited call -> no batch suffix")
    }

    @Test func aFailedOutcomeIsRenderedNotHidden() async throws {
        // A call that failed or timed out occupies its slot in M with its error
        // body — a partial batch is the one thing the model must never see.
        //
        // Python's stub ANSWERS `timed_out` for `hung`; here the kernel produces
        // it for real: `hung` pauses 500ms and the layer's deadline is 100ms. The
        // third call is the same agent under the model's OWN `<timeout>` — the
        // override python pins in `test_the_layer_default_timeout_rides_every_item`
        // — so one turn shows a slot that died on the layer deadline beside one
        // the model rescued, and neither is hidden.
        let (kernel, _, sent) = await bootStubKernel(hungPauseNanos: 500_000_000)
        await seedStub(kernel, "ai", "delivery.tools")
        await seedStub(kernel, "hung", "hung.tools")
        await seedStub(kernel, "meter", "echo.tools")
        var own: OrderedDictionary<String, JSON> = ["target_id": .string("hung")]
        own["reply_to"] = .string("self")
        own["timeout"] = .integer(2)
        own["payload"] = stubPing
        let text = stubCall("hung") + stubCall("meter") + renderCall("send", own)
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: text, clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 0.1)

        let body = (await sent.settled(1)).first?.1["text"].asString ?? ""
        let blocks = body.components(separatedBy: "\n\n")
        #expect(blocks.count == 3, Comment(rawValue: body))
        #expect(blocks[0].hasReplyTag("hung ping \u{b7} 1 of 3 in batch "), Comment(rawValue: blocks[0]))
        #expect(
            blocks[0].contains("send: hung 'ping' exceeded its 0.1s timeout; task cancelled"),
            Comment(rawValue: blocks[0]))
        #expect(blocks[1].hasReplyTag("meter ping \u{b7} 2 of 3 in batch "), Comment(rawValue: blocks[1]))
        #expect(blocks[2].hasReplyTag("hung ping \u{b7} 3 of 3 in batch "), Comment(rawValue: blocks[2]))
        #expect(blocks[2].contains("<ok>hung</ok>"), "the model's own timeout must override the layer's: \(blocks[2])")
    }

    @Test func theModelsExecIdRidesTheItemAsItsId() async throws {
        // `<exec_id>` is the call-level execution NAME; core hands it to the
        // kernel as the batch item's `id` (a bare `<id>` would collide with
        // payload ids).
        //
        // Python reads the item off its stub. Here the kernel is real, and a
        // named item is exactly what the LEDGER records: the name resolves to
        // the execution's int id, the row carries the retry budget, and the
        // reply tag names that same id as its pid.
        let (kernel, echo, sent) = await bootStubKernel()
        await seedStub(kernel, "ai", "delivery.tools")
        await seedStub(kernel, "vault", "echo.tools")
        var args: OrderedDictionary<String, JSON> = ["target_id": .string("vault")]
        args["reply_to"] = .string("self")
        args["exec_id"] = .string("job-alpha")
        args["retries"] = .integer(2)
        args["payload"] = .object(["type": .string("withdraw")])
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: renderCall("send", args), clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 42.0)

        let jid = try #require(kernel.ledger.resolveName("job-alpha"), "the exec_id opened no named execution")
        let row = try #require(kernel.ledger.entry(jid))
        #expect(row.retries == 2)
        #expect(row.named && row.target.value == "vault")
        #expect(echo.answered().first?.1 == .object(["type": .string("withdraw")]), "the payload must reach the target verbatim")
        let body = (await sent.settled(1)).first?.1["text"].asString ?? ""
        #expect(body.hasPrefix("[reply to \(jid) \u{b7} vault withdraw]"), Comment(rawValue: body))
    }

    /// C38: `<timeout>soon</timeout>` used to coerce with `.asDouble`, which
    /// silently DROPPED the malformed value — defaulting to "wait forever" —
    /// instead of reaching the kernel's `bad_arg` refusal. Forwarded RAW
    /// (tag-typed, untouched) now, so the model reads the kernel's exact text
    /// and the call never reaches the target at all.
    @Test func aRawTimeoutThatIsNotANumberReachesTheKernelsRefusal() async throws {
        let (kernel, echo, sent) = await bootStubKernel()
        await seedStub(kernel, "ai", "delivery.tools")
        await seedStub(kernel, "vault", "echo.tools")
        var args: OrderedDictionary<String, JSON> = ["target_id": .string("vault")]
        args["reply_to"] = .string("self")
        args["timeout"] = .string("soon")
        args["payload"] = .object(["type": .string("withdraw")])
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: renderCall("send", args), clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 42.0)
        #expect(
            echo.answered().isEmpty,
            "a bad timeout must refuse before dispatch, never reach the target")
        let body = (await sent.settled(1)).first?.1["text"].asString ?? ""
        #expect(
            body.contains("send: timeout must be a positive number of seconds, got 'soon'"),
            Comment(rawValue: body))
    }

    /// C38: an int `<exec_id>` JOINS an existing execution instead of being
    /// silently coerced with `.asString` (which dropped it — an int has no
    /// string form there — and minted a fresh unnamed pid instead).
    @Test func anIntExecIdJoinsAnOpenExecutionInsteadOfMintingAFreshOne() async throws {
        let (kernel, echo, sent) = await bootStubKernel()
        await seedStub(kernel, "ai", "delivery.tools")
        await seedStub(kernel, "vault", "echo.tools")
        var opener: OrderedDictionary<String, JSON> = ["target_id": .string("vault")]
        opener["reply_to"] = .string("self")
        opener["exec_id"] = .string("job-int")
        opener["payload"] = .object(["type": .string("withdraw")])
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: renderCall("send", opener), clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 42.0)
        let jid = try #require(kernel.ledger.resolveName("job-int"))

        var joiner: OrderedDictionary<String, JSON> = ["target_id": .string("vault")]
        joiner["reply_to"] = .string("self")
        joiner["exec_id"] = .integer(jid)
        joiner["payload"] = .object(["type": .string("withdraw")])
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: renderCall("send", joiner), clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 42.0)
        #expect(echo.answered().count == 1, "the int exec_id must JOIN, not open a second dispatch")
        let bodies = (await sent.settled(2)).map { $0.1["text"].asString ?? "" }
        #expect(bodies.allSatisfy { $0.hasPrefix("[reply to \(jid) ") }, "\(bodies)")
    }

    @Test func toFansOutOneCopyPerRecipientCarryingTheListAndMsg() async throws {
        // `<to>` becomes N kernel items; every copy carries the whole list, the
        // sending turn's id (`msg`) and the answered message (`re`).
        let (kernel, _, sent) = await bootStubKernel()
        await seedStub(kernel, "ai", "delivery.tools")
        for s in ["solver_2", "solver_3"] { await seedStub(kernel, s, "delivery.tools") }
        var args: OrderedDictionary<String, JSON> = [
            "to": .array([.string("solver_2"), .string("solver_3")])
        ]
        args["re"] = .integer(398)
        args["reply_to"] = .string("null")
        args["payload"] = .object(["type": .string("send"), "text": .string("CANDIDATE 586")])
        let call = renderCall("send", args)

        // The ITEMS, in emission order — python reads them off its stub's batch;
        // here `fanOut` is the step that makes them, read before dispatch.
        let items = fanOut(extractToolCalls(call), messageId: 412)
        #expect(items.map { $0.arguments["target_id"].asString } == ["solver_2", "solver_3"])
        // …and each copy REALLY reaches its recipient, carrying the same fields.
        await routeReply(
            selfId: AgentId("ai"), kernel: kernel, text: call, clientId: "cli",
            sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 42.0, messageId: 412)
        let landed = await sent.settled(2)
        let peers = landed.filter { ["solver_2", "solver_3"].contains($0.0) }
        #expect(Set(peers.map(\.0)) == ["solver_2", "solver_3"], "\(peers.map(\.0))")
        for p in items.map({ $0.arguments["payload"] }) + peers.map(\.1) {
            #expect(p["to"] == .array([.string("solver_2"), .string("solver_3")]), Comment(rawValue: p.serialize()))
            #expect(p["msg"].asInt == 412 && p["re"].asInt == 398, Comment(rawValue: p.serialize()))
            #expect(p["text"].asString == "CANDIDATE 586")
        }
        #expect(landed.count == 2, "reply_to:null — nothing is owed back to ai")
    }

    // ─── `await`: an honest wait or a refusal, never a silent stall ──
    // Mirrors python `test_done_and_nudge.py` (the two `await` cases).

    @Test func awaitWithNothingInFlightIsRefusedAndToldWhatToDo() async throws {
        // Nothing dispatched, nothing queued, nothing running: waiting would be
        // waiting forever on an empty inbox. The refusal is DELIVERED as a turn,
        // so the model acts on it instead of sitting on a return value nobody reads.
        //
        // ITS OWN AGENT ID (python's `nd_aw`), not the suite's `mock`: `inFlightTurns`
        // is keyed by the bare id across every kernel in the process, so a sibling
        // case's live batch for "mock" would count as THIS agent's in-flight work
        // and turn the refusal into an honoured wait (measured 2026-09-13: passes
        // alone, fails under parallel execution).
        let script = ProviderScript([["waiting <call name=\"await\"></call>"], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["nd_aw": script]), backendIds: ["nd_aw"])
        _ = await kernel.send([Item(ids["nd_aw"]!, .object([
                "type": .string("send"), "text": .string("hi"), "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the refusal never woke the model")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 2, "and `done` ends it: \(script.calls) passes")
        let turn2 = script.requestText(1)
        #expect(turn2.contains("[await refused]"), Comment(rawValue: turn2.suffix(400).description))
        #expect(turn2.contains("nothing of yours is in flight"), Comment(rawValue: turn2.suffix(400).description))
    }

    @Test func awaitWithAMessageQueuedStopsQuietlyAndTheQueueWakesIt() async throws {
        // The honest wait: something is queued for the agent, so `await` ends the
        // turn with no nudge and no refusal; the queued message is the next turn.
        //
        // A slow first pass (300ms per chunk), so the second send queues behind
        // it under the FIFO — python's `_Script(delay=0.3)`.
        let script = ProviderScript([["waiting <call name=\"await\"></call>"], [doneEnvelope]])
        var cfg = scriptedConfig(["nd_aw2": script])
        cfg.makeProvider = { _, _, _ in
            .provider(MockProvider(script: script, chunkDelayNanos: 300_000_000))
        }
        // Its own id (python's `nd_aw2`) for the same reason as the case above:
        // `pending` must be THIS agent's queue, not a sibling "mock"'s batch.
        let (kernel, ids, _) = await bootMockKernel(config: cfg, backendIds: ["nd_aw2"])
        let id = ids["nd_aw2"]!
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }
        let first = Task {
            await kernel.send([Item(id, .object([
                    "type": .string("send"), "text": .string("one"), "client_id": .string("c"),
                ]))])[0].result
        }
        try await Task.sleep(nanoseconds: 100_000_000)
        let second = Task {
            await kernel.send([Item(id, .object([
                    "type": .string("send"), "text": .string("two"), "client_id": .string("c"),
                ]))])[0].result
        }
        _ = await first.value
        _ = await second.value
        try await Task.sleep(nanoseconds: 400_000_000)
        #expect(script.calls == 2, "expected exactly the two sends, got \(script.calls) passes")
        for i in 0..<script.calls {
            #expect(!script.requestText(i).contains("[await refused]"), "pass \(i + 1) was refused")
            #expect(!script.requestText(i).contains("[no call was dispatched]"), "pass \(i + 1) was nudged")
        }
        let last = script.request(1)?.last?["content"].asString ?? ""
        #expect(last.hasSuffix("two"), "the queued message must be the next turn: \(last.suffix(200))")
        // B2.4: the honest wait is an ordinary status event from the SHARED
        // emitter — `reason, pending`, then the auto-filled fields, `queue_depth`
        // last — not an envelope built by hand with none of them.
        let awaiting = try #require(
            sink.snapshot().first { $0["detail"]["reason"].asString == "awaiting" })
        let keys = awaiting["detail"].asObject.map { Array($0.keys) } ?? []
        #expect(Array(keys.prefix(2)) == ["reason", "pending"], "\(keys)")
        #expect(keys.last == "queue_depth", "\(keys)")
        #expect(awaiting["client_id"].asString == "c")
    }
}

// MARK: - python's `_Kernel` stub, as bundles on a bare kernel

/// The `{type:"ping"}` payload every stub call carries — python's default
/// `reflect`, swapped for a verb the kernel does not compose itself, so an echo
/// reply is exactly what the bundle returned.
private let stubPing: JSON = .object(["type": .string("ping")])

/// python's `_call(target, reply_to)`.
private func stubCall(_ target: String, replyTo: String?? = .some("self")) -> String {
    callEnvelope(target: target, payload: stubPing, replyTo: replyTo)
}

/// The stub's "one outcome per item" half: answers any verb `{ok:<id>}` after
/// `pauseNanos`, and records `(agent, payload)` for every verb it answered —
/// python's `batches`, read as what actually ran.
private final class EchoBundle: AgentBundle, @unchecked Sendable {
    let name = "echo"
    private let pauseNanos: UInt64
    private let lock = NSLock()
    private var seen: [(String, JSON)] = []

    init(pauseNanos: UInt64 = 0) { self.pauseNanos = pauseNanos }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            lock.withLock { seen.append((agentId.value, payload)) }
            if pauseNanos > 0 { try? await Task.sleep(nanoseconds: pauseNanos) }
            return .object(["ok": .string(agentId.value)])
        }
    }

    func answered() -> [(String, JSON)] { lock.withLock { seen } }
}

/// The stub's `sent` half: records every delivered `(agent, payload)`. Unlike
/// `PayloadRecorder` it keeps the AGENT, so two destinations can be told apart.
private final class DeliveryRecorder: AgentBundle, @unchecked Sendable {
    let name = "delivery"
    private let lock = NSLock()
    private var seen: [(String, JSON)] = []
    /// A slow destination, so a test can observe that the router did NOT wait.
    let pauseNanos: UInt64

    init(pauseNanos: UInt64 = 0) { self.pauseNanos = pauseNanos }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            if pauseNanos > 0 { try? await Task.sleep(nanoseconds: pauseNanos) }
            lock.withLock { seen.append((agentId.value, payload)) }
            return .object(["recorded": .bool(true)])
        }
    }

    /// Every delivered message as `(destination, payload)`, in arrival order.
    func messages() -> [(String, JSON)] { lock.withLock { seen } }

    /// Wait for `count` deliveries to land. A delivery is a SPAWNED kernel task
    /// (`verb:"deliver_reply"`) and the router does not await it — python's
    /// `_deliver_soon` — so a caller that read `messages()` the instant
    /// `routeReply` returned was reading a race, not a result.
    func settled(_ count: Int, within seconds: Double = 2) async -> [(String, JSON)] {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            let seen = messages()
            if seen.count >= count { return seen }
            try? await Task.sleep(nanoseconds: 2_000_000)
        }
        return messages()
    }
}

/// A bare in-memory kernel with the two stub bundles registered (`echo.tools`,
/// `hung.tools` — an echo with its own pause — and `delivery.tools`). No
/// backend: these cases drive `routeReply` directly, as python drives
/// `_route_reply`.
private func bootStubKernel(
    echoPauseNanos: UInt64 = 0, hungPauseNanos: UInt64 = 0, deliveryPauseNanos: UInt64 = 0
) async -> (Kernel, EchoBundle, DeliveryRecorder) {
    let registry = BundleRegistry()
    let echo = EchoBundle(pauseNanos: echoPauseNanos)
    registry.register("echo.tools", echo)
    registry.register("hung.tools", EchoBundle(pauseNanos: hungPauseNanos))
    let sent = DeliveryRecorder(pauseNanos: deliveryPauseNanos)
    registry.register("delivery.tools", sent)
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return (kernel, echo, sent)
}

private func seedStub(_ kernel: Kernel, _ id: String, _ module: String) async {
    _ = await kernel.send([Item(AgentId("kernel"), .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "id": .string(id), "handler_module": .string(module),
        ]))])[0].result
}

extension String {
    /// `[reply to <pid> · <rest>` at the START of a block — python's
    /// `startswith(f"[reply to {agent}:pid · …")`, with the pid an int here.
    fileprivate func hasReplyTag(_ rest: String) -> Bool {
        range(
            of: "^\\[reply to [0-9]+ \u{b7} " + NSRegularExpression.escapedPattern(for: rest),
            options: .regularExpression) != nil
    }

    // ── round-2 edges (B1.11, B1.26, B5.18, B5.19) ───────────────

    // B1.11: kept RAW, refused only when it is not a string or is blank after a
    // FULL python strip.
    @Test func sendToIsKeptRawAndDroppedOnlyWhenBlank() async throws {
        // A `done` turn is the one that still takes the caller's hand-off.
        func handOff(_ sendTo: JSON) async -> [JSON] {
            let script = ProviderScript([["finished. " + doneEnvelope]])
            let (k, ids, recorder) = await bootMockKernel(
                config: scriptedConfig(["mock": script]), backendIds: ["mock"])
            _ = await k.send([
                Item(
                    ids["mock"]!,
                    .object([
                        "type": .string("send"), "text": .string("go"),
                        "send_to": sendTo, "client_id": .string("c"),
                    ]))
            ])[0].result
            _ = await recorder.waitForPayloads(1, timeout: 1.0)
            return recorder.payloads()
        }
        // `probe` reaches the recorder; ` probe ` is a DIFFERENT id and reaches
        // nothing — the value is kept raw, spaces and all.
        #expect(await handOff(.string("probe")).count == 1)
        #expect(await handOff(.string(" probe ")).isEmpty, "a padded id is not `probe`")
        // `\n\t` is blank under python's strip (`.whitespaces` alone is not), and
        // a non-string is no target at all — neither makes a hand-off.
        #expect(await handOff(.string("\n\t")).isEmpty)
        #expect(await handOff(.integer(7)).isEmpty)
    }

    // B1.26: the reply block renders `result` with NO injected `target`.
    @Test func aRaisedCallReplyBlockRendersTheResultUnchanged() async throws {
        let script = ProviderScript([
            [callEnvelope(target: "raiser", payload: .object(["type": .string("reflect")]))],
            ["saw it"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([
            Item(
                ids["mock"]!,
                .object([
                    "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                ]))
        ])[0].result
        #expect(await script.waitForCalls(2))
        let turn2 = script.requestText(1)
        // The body is `{error, raised}` rendered as-is. `<target>` was swift's
        // own addition and python never writes it.
        #expect(!turn2.contains("<target>"), "\(turn2.suffix(400))")
    }

    // B5.18: `_clean_title` on CODE POINTS, with python's strip.
    @Test func cleanTitleMatchesPython() {
        // A CRLF is ONE Character and TWO code points: python turns the run into
        // a single space, swift's Character scan left it whole.
        #expect(cleanTitle(.string("a\r\nb")) == "a b")
        #expect(cleanTitle(.string("a]]]b")) == "a b")
        // U+001C is whitespace to python's `strip()` and not to `.whitespaces`.
        #expect(cleanTitle(.string("a \u{1c}")) == "a")
        #expect(cleanTitle(.string("]]]")) == nil)
        #expect(cleanTitle(.string("   ")) == nil)
        #expect(cleanTitle(.integer(7)) == nil)
        #expect(cleanTitle(nil) == nil)
        // 140 CODE POINTS: a family emoji is 7 scalars and one Character.
        let family = String(repeating: "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", count: 30)
        #expect(cleanTitle(.string(family))?.unicodeScalars.count == 140)
    }

    // B5.19: a non-string `<type>` still names the reply — `?` means ABSENT.
    @Test func aNonStringTypeTagsTheReplyWithPythonStr() async throws {
        let script = ProviderScript([
            [
                renderCall(
                    "send",
                    [
                        "target_id": .string("probe"), "reply_to": .string("self"),
                        "payload": .object(["type": .integer(42)]),
                    ])
            ],
            ["saw it"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([
            Item(
                ids["mock"]!,
                .object([
                    "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                ]))
        ])[0].result
        #expect(await script.waitForCalls(2))
        #expect(
            script.requestText(1).contains("\u{b7} probe 42 \u{b7}"),
            "\(script.requestText(1).prefix(300))")
    }

    // B5.12: `<to>42</to>` is a CALL python dispatches; the no-target reply comes
    // from routing, and the echo shows what the model wrote.
    @Test func aTruthyNonListToReachesTheNoTargetReply() async throws {
        let script = ProviderScript([
            ["<call name=\"send\"><to>42</to><type>reflect</type></call>"],
            ["saw it"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([
            Item(
                ids["mock"]!,
                .object([
                    "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                ]))
        ])[0].result
        #expect(await script.waitForCalls(2))
        let turn2 = script.requestText(1)
        #expect(turn2.contains("no target_id, so nothing was dispatched"), "\(turn2.suffix(400))")
        #expect(turn2.contains("<to>42</to>"), "\(turn2.suffix(400))")
        #expect(!turn2.contains("call NOT dispatched"), "it parses; it is not malformed")
    }
}
