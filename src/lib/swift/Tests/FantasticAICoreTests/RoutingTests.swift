// Reply routing on the AI agent — the model decides where its output goes.
//
// Mirrors the canonical python suite
// `bundled_agents/ai/ollama/ollama_backend/tests/test_send_to.py`.
//
// After ONE provider pass the output is routed. Two paths, and the FIRST is the
// model's:
//
//   its own `<tool_call>`s — each names a target, a payload and `reply_to`. Every
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
import Testing

@testable import FantasticAICore

/// The envelope exactly as the prompt teaches it. `replyTo` is `.some(nil)` for an
/// explicit `null` and `nil` for the key being ABSENT — the two mean opposite
/// things, so the helper has to be able to express both.
private func callEnvelope(
    target: String, payload: JSON, replyTo: String?? = .some("self")
) -> String {
    var args: [(String, JSON)] = [("target_id", .string(target)), ("payload", payload)]
    if let outer = replyTo {
        args.append(("reply_to", outer.map(JSON.string) ?? .null))
    }
    var obj: JSON = .object([:])
    for (k, v) in args { obj[k] = v }
    let body = JSON.object(["name": .string("send"), "arguments": obj]).serialize()
    return "<tool_call>\(body)</tool_call>"
}

/// A cheap reflect: `tree:"none"` keeps the reply small, so a 3-hop chain's
/// prompts stay well inside the window and a failure is about routing.
private let cheapReflect: JSON = .object([
    "type": .string("reflect"), "tree": .string("none"),
])

@Suite("AI reply routing: the model decides where its output goes")
struct RoutingTests {

    // ─── stop is the default ────────────────────────────────────

    @Test func noCallMeansOnePass() async throws {
        // THE point of model-owned routing: a one-pass task costs ONE pass. No
        // `<tool_call>` means the model is finished — nothing schedules another
        // turn. The second scripted pass exists only so an over-eager chain has
        // something to consume and fails the count instead of stalling.
        let script = ProviderScript([["the answer"], ["must never run"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("hi"), "client_id": .string("c"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 1, "no call must end the turn, got \(script.calls) passes")
        let msgs = await historyRows(kernel: kernel, id: id, client: "c")
        #expect(msgs.count == 2)
        #expect(msgs[1]["content"].asString == "the answer")
    }

    // ─── the model acts, and sees the result ────────────────────

    @Test func modelCallIsDispatchedAndTheResultReturns() async throws {
        // The model emits a send; it really runs; its REPLY comes back as the next
        // turn, tagged so the model can tell it from a human's message.
        let script = ProviderScript([
            [callEnvelope(target: "core", payload: .object(["type": .string("list_agents")]))],
            ["done looking"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("what is here?"),
                "client_id": .string("c"),
            ]))
        #expect(await script.waitForCalls(2), "the call's reply never came back")
        try await Task.sleep(nanoseconds: 200_000_000)
        #expect(script.calls == 2, "one call = one follow-up turn, then it stopped")
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
            ["enough"],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
        #expect(await script.waitForCalls(3), "chain stalled at \(script.calls) passes")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 3, "expected 3 passes, got \(script.calls)")
        let turn3 = script.requestText(2)
        #expect(turn3.contains("PASS-ONE-SPEAKING"), "pass 3 read a stale history")
        #expect(turn3.contains("PASS-TWO-SPEAKING"))
        // 3 passes = 3 user turns + 3 assistant turns.
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
            _ = await kernel.send(
                ids["mock"]!,
                .object([
                    "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                ]))
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
        // A `<tool_call>` with no target tells the model so, instead of vanishing.
        let bad = #"<tool_call>{"name":"send","arguments":{"reply_to":"self"}}</tool_call>"#
        let script = ProviderScript([[bad], ["ok"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
        #expect(await script.waitForCalls(2), "malformed call was swallowed")
        #expect(script.requestText(1).contains("no target_id"))
    }

    @Test func aSendWithNoPayloadNamesWhatIsMissing() async throws {
        // Measured 2026-08-06: a payload-less send reached the handler as `None`
        // and came back `'NoneType' object has no attribute 'get'` — a Python
        // error about our internals, naming neither the omitted verb nor the
        // agent. It must say what was missing instead, and must never dispatch.
        let bad = #"<tool_call>{"name":"send","arguments":{"target_id":"probe"}}</tool_call>"#
        let script = ProviderScript([[bad], ["ok"]])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
        let broken =
            #"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":"#
            + #"{"type":"create_agent","code":"x = \"unclosed}}</tool_call>"#
        let script = ProviderScript([[broken], ["ok"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
            #"<tool_call>{"name":"send","arguments":{"target_id":"core","#
            + #""code":"x = \"oops}}</tool_call>"#
        let script = ProviderScript([[broken + "\n" + good], ["ok"], ["ok again"]])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
        // The sibling really dispatched…
        #expect(await recorder.waitForPayloads(1), "the parsed sibling was dropped")
        #expect(recorder.payloads()[0]["type"].asString == "noted")
        // …and the broken one still produced a report the model can read. Two
        // deliveries land — the report and the sibling's reply — so exactly two
        // follow-up passes happen. Which of the two lands first is NOT asserted:
        // each delivery is its own kernel task racing for the FIFO, so pinning the
        // order would pin a scheduler, not a contract. What matters is that
        // neither report nor reply went missing.
        #expect(await script.waitForCalls(3), "a delivery went missing")
        try await Task.sleep(nanoseconds: 300_000_000)
        #expect(script.calls == 3, "expected 3 passes, got \(script.calls)")
        let followUps = script.requestText(1) + script.requestText(2)
        #expect(followUps.contains("NOT dispatched"), "the malformed sibling was swallowed")
        #expect(followUps.contains(" · probe "), "the parsed sibling's reply was lost")
    }

    @Test func theErrorPromptsAreVerbatim() async throws {
        // These strings are PROMPTS, not diagnostics — the only thing a model has
        // to go on when a call it wrote did not run. Pinned as literal text so a
        // "clearer" rewording in one runtime shows up as a failing test rather
        // than as cross-runtime drift the model pays for.
        #expect(
            NO_TARGET_ERROR == """
                no target_id, so nothing was dispatched — a send must name the AGENT it \
                goes to. Shape: {"name":"send","arguments":{"target_id":"<agent>",\
                "payload":{"type":"<verb>"}}}. Send `{type:'reflect'}` to the kernel for \
                the list of agent ids.
                """)
        #expect(
            noPayloadError(target: "totals") == """
                send to 'totals' carried no payload, so nothing was dispatched. A payload \
                is an object whose `type` names the verb, e.g. {"type":"reflect"}.
                """)
        // The malformed report is composed in `Malformed.swift`; its opening
        // sentence is the part the model keys on.
        let report = malformedCalls(#"<tool_call>{"name":"send","arguments":{"#)
        #expect(report.count == 1)
        #expect(
            report[0]["error"].asString?.hasPrefix("tool_call NOT dispatched: invalid JSON — ")
                == true)
        #expect(report[0]["error"].asString?.contains("Nothing ran.") == true)
    }

    // ─── translation, not correction ────────────────────────────

    @Test func aVerbNamedToolReachesTheAgent() async throws {
        // The other half of the same fix: `{"name":"reflect","arguments":
        // {"target_id":…}}` is a real instruction and must DISPATCH, not earn an
        // error.
        let call = #"<tool_call>{"name":"reflect","arguments":{"target_id":"probe"}}</tool_call>"#
        let script = ProviderScript([[call], ["ok"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
        #expect(await script.waitForCalls(2), "the reflect never came back")
        let turn2 = script.requestText(1)
        #expect(turn2.contains(" · probe "), "\(turn2.suffix(300))")
        #expect(!turn2.contains("no payload"))
        // The translated verb really arrived as the payload's `type` — the reply
        // carries `probe`'s own reflect body, which only a real `reflect` produces.
        #expect(turn2.contains("payload recorder"), "\(turn2.suffix(300))")
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
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
        let a = ProviderScript([["the answer from a"], ["a must not run again"]])
        let b = ProviderScript([["b replied"], ["b must not run again"]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": a, "peer": b]), backendIds: ["mock", "peer"])
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("probe_client"), "send_to": .string("peer"),
            ]))
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                "send_to": .string("peer"),
            ]))
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("probe_client"),
            ]))
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
            ]))
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
        _ = await kernel.send(
            ids["mock"]!,
            .object([
                "type": .string("send"), "text": .string("hi"), "client_id": .string("c"),
            ]))
        #expect(await script.waitForCalls(1))
        let messages = script.request(0) ?? []
        #expect(messages.first?["role"].asString == "system")
        let sysBlock = (messages.first?["content"].asString ?? "").lowercased()

        // the mechanism IS stated
        #expect(sysBlock.contains("reply_to"))
        #expect(sysBlock.contains("reflect"))
        #expect(sysBlock.contains("<tool_call>"))

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
}
