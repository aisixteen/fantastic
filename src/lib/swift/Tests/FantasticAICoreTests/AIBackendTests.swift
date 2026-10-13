// Shared AI-backend machinery tests — driven through a MOCK `AIProvider` so the
// verb surface, the single-pass streaming, history persistence, cancellation,
// context management, and the error-shape config flag are all
// exercised WITHOUT any live LLM. The harness (scripted provider, mock bundle,
// in-memory file_bridge, polling helpers) lives in `MockBackend.swift`; the
// DISPATCH path each pass ends in lives in `RoutingTests.swift`;
// provider-specific wire tests stay with each backend target.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticRamStore

/// The private reaction cursor, read through the same lock the backend uses.
/// SYNCHRONOUS on purpose: `NSLock.lock()` is unavailable from an async context.
private func readMark(_ backend: AIBackend, _ key: String) -> (Int, String)? {
    backend.projectionLock.lock()
    defer { backend.projectionLock.unlock() }
    return backend.compactionMark[key]
}

// MARK: - Tests

@Suite("AIBackend shared machinery")
struct AIBackendTests {

    // B1.23: PYTHON'S KEYS, IN PYTHON'S ORDER, then the backend's extras. No
    // `kind`, no `provider` — python's reflect has neither, and this line is
    // rendered into every prompt.
    @Test func reflectBaseMatchesPythonKeysAndOrder() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let r = await kernel.send([Item(id, .object(["type": .string("reflect")]))])[0].result
        // The kernel appends its own frame (can_call/callable_by/…) after the
        // bundle's body; the BODY is what this pins.
        #expect(
            (r.asObject?.keys.prefix(10)).map(Array.init) == [
                "id", "sentence", "model", "endpoint", "context_window", "context_strategy",
                "memory_store_id", "credentials", "generating", "extra",
            ], "\(r.asObject?.keys.map { $0 } ?? [])")
        #expect(r["memory_store_id"].asString == "memory")
        #expect(r["credentials"].isNull, "no key named: \(r)")
        #expect(r["id"].asString == id.value)
        #expect(r["kind"].isNull && r["provider"].isNull)
        // the record's own fields, or null — nothing is guessed
        #expect(r["model"].isNull && r["endpoint"].isNull, "\(r)")
        #expect(r["generating"] == .bool(false))
        #expect(r["extra"].asString == "yes")
        // No verb docs on a reflect (owner, 2026-10-05) — they are `docs`.
        for key in ["verbs", "cancel", "emits", "concurrency"] {
            #expect(r.asObject?[key] == nil, "\(key)")
        }
        // The verb table `docs` serves is python's `_build` table, in its order.
        #expect(
            AIBackend.VERBS.keys.elementsEqual([
                "reflect", "send", "stop", "rows", "inquiry", "history", "status", "context_status",
                "recall", "grep", "context_recall", "context_clear", "watch", "unwatch", "boot",
            ]))
        #expect(
            AIBackend.VERBS["reflect"]?.asString
                == "Identity + model + endpoint + generating flag + context posture. No args.")
        #expect(
            AIBackend.VERBS["send"]?.asString?.hasPrefix(
                "args: text:str (req), client_id:str? (default 'cli'), system_prompt:str?, send_to:str?.")
                == true)
    }

    // B1.23: THREE channels, python's wording. `done` is NOT one of them —
    // `status(phase='done')` IS the completion event.
    @Test func docsEmitsArePythonVerbatim() async throws {
        // Served by `docs` (tier detailed), never on a reflect.
        let emits = JSON.object(AIBackend.REFLECT_EMITS)
        #expect(emits.asObject?.keys.elementsEqual(["token", "status", "context"]) == true)
        #expect(
            emits["token"].asString
                == "{type:'token', text:str, source, client_id} — streaming chunk, routed ONLY to "
                + "the caller: cli (stdout) when client_id='cli', else this agent's own inbox "
                + "(a browser filters by client_id).")
        #expect(emits["done"].isNull)
    }

    // B1.2: the cancel doc is python's, TAG notation included — the model reads
    // it and copies the shape.
    @Test func cancelDocIsPythonVerbatim() {
        #expect(
            CANCEL_DOC
                == "No `interrupt` verb — an in-flight `send` IS a kernel task. Kill it with the "
                + "substrate's universal system verbs: send 'kernel' <type>ps</type> for the "
                + "live task table, find the row {agent:'<this agent>', verb:'send'}, then "
                + "send 'kernel' <type>cancel</type> <task_id>n</task_id>. The turn closes "
                + "with status(phase='done', reason='cancelled') + done, its HTTP stream and "
                + "FIFO lock are released, and the blocked caller's `send` resolves to "
                + "{cancelled:true}. Cancel kills the task's whole SUBTREE, so any work the "
                + "turn spawned dies with it.")
    }

    @Test func backendStateIsNotAVerb() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let r = await kernel.send([Item(id, .object(["type": .string("backend_state")]))])[0].result
        #expect(r["error"].asString == "mock_backend: unknown type 'backend_state'")
    }

    @Test func unknownVerbErrors() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let r = await kernel.send([Item(id, .object(["type": .string("nope")]))])[0].result
        #expect(r["error"].asString == "mock_backend: unknown type 'nope'")
    }

    /// `send` answers with the FINISHED text — python/rust parity. It used to
    /// ack `{queued, stream_id, message_id}` and run the pass on its own task,
    /// so a caller reading `response` got the text on two kernels and an empty
    /// ack on the third.
    @Test func sendReturnsTheCompletedText() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let r = await kernel.send([Item(id, .object(["type": .string("send"), "text": .string("hi")]))])[0].result
        #expect(r["response"].asString == "Hello")
        #expect(r["final"].asString == r["response"].asString)
        #expect(r["client_id"].asString == "cli")
        #expect(r["queued"].asBool == nil)
    }

    /// A MISSING `text` IS THE EMPTY STRING. Python (`payload.get("text", "")`) and
    /// rust (`unwrap_or("")`) both run the turn, so a caller-driven agent kicking
    /// off from its system prompt alone worked on two kernels and was refused on the
    /// third with a reason the others never produce.
    @Test func sendDefaultsMissingTextToEmpty() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let r = await kernel.send([Item(id, .object(["type": .string("send")]))])[0].result
        #expect(r["error"].asString == nil, "a missing `text` is not a refusal: \(r.serialize())")
        #expect(r["response"].asString == "Hello", "the turn RAN")
        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.first?["content"].asString == "", "and the user turn is the empty string")
    }

    /// `status.is_mine` is matched against the SANITISED client id, so the entry has
    /// to be STORED sanitised too. Swift kept the raw id in the queue/current entry
    /// while `statusVerb` compared `safeClient(requesting)`, so a browser client
    /// `tab 1/x` asking after its OWN work was told `is_mine:false, mine_pending:[],
    /// others_pending:1`. Python sanitizes both sides at entry.
    @Test func statusIsMineMatchesASanitisedClientId() async throws {
        let raw = "tab 1/x"
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                MockProvider(tokens: ["a", "b", "c"], chunkDelayNanos: 40_000_000)
            })
        let running = Task {
            _ = await kernel.send([Item(id, .object([
                    "type": .string("send"), "text": .string("hi there"),
                    "client_id": .string(raw),
                ]))])[0].result
        }
        var snap: JSON = .null
        for _ in 0..<50 {
            let s = await kernel.send([Item(id, .object([
                    "type": .string("status"), "client_id": .string(raw),
                ]))])[0].result
            if s["generating"].asBool == true {
                snap = s
                break
            }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        #expect(snap["generating"].asBool == true)
        #expect(snap["current"]["is_mine"].asBool == true, "\(snap.serialize())")
        #expect(snap["current"]["text"].asString == "hi there", "the owner sees its own text")
        #expect(snap["others_pending"].asInt == 0)
        _ = await running.value
        // The conversation is the AGENT's — one store, whatever id the client used.
        #expect(conversationRows(kernel: kernel, id: id).count == 2, "the turn landed in the one store")
    }

    @Test func streamPersistsUserAndAssistantHistory() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: ["Hel", "lo"]) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("hi"),
                "client_id": .string("c1"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)

        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 2)
        #expect(msgs.first?["role"].asString == "user")
        #expect(msgs[0]["content"].asString == "hi")
        // EVERY ROW HAS AN ADDRESS — an int from the kernel's one counter — and
        // `parent` is causality: the external message is a ROOT, the assistant
        // row answers it. Python `test_row_ids.py`.
        let userId = try #require(msgs[0]["id"].asInt)
        #expect(msgs[0]["parent"].isNull)
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[1]["content"].asString == "Hello")
        #expect(msgs[1]["id"].asInt == userId + 1, "the counter, in order")
        #expect(msgs[1]["parent"].asInt == userId)
        #expect(msgs[1]["tool_calls"].asArray == nil)
        #expect(msgs[0]["complete"].isNull, "python rows carry no `complete`")
    }

    /// `history` answers python's DIGEST page — {rows:[{id, parent, kind, role,
    /// head, bytes}], next, total} — never a body: bodies are behind `pull`, a
    /// row's text behind `recall`. `limit`/`after` page it by row id.
    @Test func historyIsADigestPageByRowId() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: ["x ", doneEnvelope]) })
        for t in ["one\nline two", "two"] {
            _ = await kernel.send([Item(id, .object(["type": .string("send"), "text": .string(t)]))])[0].result
        }
        try await waitForHistory(kernel: kernel, id: id, count: 4)
        let h = await kernel.send([Item(id, .object(["type": .string("history")]))])[0].result
        let rows = h["rows"].asArray ?? []
        #expect(rows.count == 4 && h["total"].asInt == 4 && h["next"].isNull, "\(h.serialize())")
        #expect(rows[0]["head"].asString == "one line two", "120 chars, newlines flattened")
        #expect(rows[0]["bytes"].asInt == 12)
        #expect(rows[0]["kind"].asString == "user", "`kind` falls back to `role`")
        #expect(rows[0]["content"].isNull, "a digest, never a body")
        #expect(rows[1]["parent"].asInt == rows[0]["id"].asInt)
        let page = await kernel.send([Item(id, .object(["type": .string("history"), "limit": .integer(2)]))])[0].result
        #expect((page["rows"].asArray ?? []).count == 2 && page["next"].asInt == rows[1]["id"].asInt)
        let rest = await kernel.send([Item(id, .object(["type": .string("history"), "after": page["next"]]))])[0].result
        #expect((rest["rows"].asArray ?? []).map { $0["id"].asInt } == rows[2...].map { $0["id"].asInt })
        #expect(h["client_id"].isNull && h["messages"].isNull, "python's shape, nothing beside it")
    }

    @Test func aCallerSystemPromptIsKeptForEveryHopOfTheChain() async throws {
        // INVERTED by the dispatch phase, and kept for what survived. This test
        // used to assert that an envelope streamed through UNDISPATCHED — "the
        // tool loop is the caller's, this backend runs one pass and parses
        // nothing". That is no longer true: the substrate reads every completion
        // and dispatches the calls in it, so the envelope below really runs and
        // its reply arrives as turn 2.
        //
        // What a caller-supplied `system_prompt` still does is REPLACE the default
        // block — and it has to keep doing so for every hop, or a chain the caller
        // opened in its own convention silently reverts to the substrate's on turn
        // 2 and the model is told a different set of rules mid-task. That is why
        // `deliverTurn` carries the prompt along.
        let envelope =
            #"<call name="send"><target_id>core</target_id><type>reflect</type><tree>none</tree></call>"#
        let script = ProviderScript([[envelope], ["seen it " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"),
                "system_prompt": .string(
                    "CALLER BLOCK: you drive tools via <call name=…> envelopes."),
                "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the dispatched call's reply never came back")
        // Wait for ALL FOUR rows the assertion below reads.
        try await waitForHistory(kernel: kernel, id: id, count: 4)

        // Pass 2's system message is the CALLER's, still — not the substrate's
        // default block (whose `SEND_TO_HOWTO` heading would be in there).
        let turn2 = script.requestText(1)
        #expect(turn2.contains("CALLER BLOCK"))
        #expect(!turn2.contains("Reaching other agents"))
        #expect(turn2.contains(" · core "))

        // Four rows: the envelope turn and the reply turn it caused
        // (user+assistant each). The reply's done-signalled answer ends the chain.
        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 4)
        #expect(msgs[1]["content"].asString == envelope)  // persisted verbatim
        #expect(msgs[1]["tool_calls"].asArray == nil)  // and never as a native block
        #expect(msgs[2]["content"].asString?.contains(" · core ") == true)
    }

    @Test func thinkingIsStreamedRawAndPersistedStripped() async throws {
        // A reasoning model's turn arrives as ONE text stream with its
        // deliberation wrapped in `<think>…</think>` (the providers do the
        // wrapping; see `ThinkingWrap`). Two uses, two texts:
        //   • the `token` events carry the RAW completion — every `text` the
        //     client received, concatenated, is exactly what the model wrote;
        //   • the DURABLE assistant turn is STRIPPED, because re-feeding a
        //     reasoning model its own prior thought is against every vendor's
        //     guidance and compounds turn over turn.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                
                    MockProvider(tokens: ["<think>", "weigh ", "options", "</think>", "42"])
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("what is it"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        try await waitUntil("status(done)") { sink.statusDone() != nil }

        // RAW on the wire — the `text` of every token event, in order.
        let raw = "<think>weigh options</think>42"
        let deltas = sink.snapshot().filter { $0["type"].asString == "token" }
            .compactMap { $0["text"].asString }.joined()
        #expect(deltas == raw)
        // B2.15: `status(phase:'done')` IS the completion. There is no `done`
        // event at all — two events for one transition is two truths.
        #expect(sink.first(type: "done") == nil)

        // STRIPPED in the durable store.
        let msgs =
            conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 2)
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[1]["content"].asString == "42")
    }

    @Test func aThinkingOnlyTurnStreamsItsThoughtAndIsDiscardedAsADeadTurn() async throws {
        // The measured failure this mechanism exists for (gemma4:12b, 2026-08-02):
        // the model spent its ENTIRE turn reasoning. Reading one channel made that
        // an empty completion that silently ended the chain, indistinguishable
        // from "the model said nothing" — so the reasoning text must still reach
        // the caller (and a reader's parser) as tokens.
        //
        // What such a turn IS changed on 2026-09-30 (owner's decision, python
        // `_run`): a turn with no answer once the thinking is stripped is a DEAD
        // TURN — its empty answer discarded (the incoming row is kept), and
        // rerolled by the no-call reminder. It used to complete with an empty assistant row, and the
        // chain went on as if it had been answered (measured on vLLM: an 8-token
        // thinking budget, then a clean `stop` with no answer).
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                MockProvider(tokens: ["<think>", "all of my turn", "</think>"])
            })
        // This provider thinks forever; a cap of 1 gives up at the FIRST silent
        // turn, so the test observes exactly ONE pass (no rerolls streaming more
        // tokens into the same inbox).
        AISilentStreaks.setCap(kernel, 1)
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        let out = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))])[0]
        #expect(out.result["dead_turn"].asBool == true, "\(out.result)")
        #expect(out.result["response"].asString == "<think>all of my turn</think>")
        #expect(out.result["message_id"] == .null, "no row was minted for it")
        try await waitUntil("status(done)") { sink.statusDone() != nil }

        // The reasoning REACHED the caller as tokens, even though the turn was
        // discarded.
        let streamed = sink.snapshot().filter { $0["type"].asString == "token" }
            .compactMap { $0["text"].asString }.joined()
        #expect(streamed == "<think>all of my turn</think>")
        #expect(sink.statusDone()?["detail"]["reason"].asString == "dead_turn")
        #expect(sink.first(type: "done") == nil)

        // The INCOMING row is kept — only the empty answer is dropped (python
        // `_run`: a dead turn answering a reply batch must not lose the batch).
        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 1, "\(msgs)")
        #expect(msgs.first?["role"].asString == "user")
        #expect(msgs.first?["content"].asString == "go")
    }

    @Test func anErroredStreamPersistsNEITHERHalfOfTheTurn() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(throwsError: true) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("hi"),
                "client_id": .string("c"),
            ]))])[0].result
        // A turn is persisted as a UNIT or not at all. This used to leave the
        // user row behind, because the backend wrote it up-front and the error
        // path returned before the assistant row — so a failed exchange read
        // back as a question nobody answered. Python and rust never produced
        // that: both have ONE save, after the stream, carrying both turns.
        try await Task.sleep(nanoseconds: 60_000_000)
        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.isEmpty, "a failed turn left a half-exchange behind: \(msgs)")
    }

    @Test func cancellingAnInFlightSendClosesTheTurnOut() async throws {
        // There is NO `interrupt` verb: an in-flight `send` IS a kernel task. A
        // caller kills a turn with the SUBSTRATE's universal verbs — `ps` to find
        // the generation's row, `cancel` to kill it — and the backend's
        // `on_cancel` hook closes the turn out (final `status` + `done`) so the
        // client is never left hanging, with no phantom generation behind.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                
                    MockProvider(
                        tokens: Array(repeating: "x", count: 200),
                        chunkDelayNanos: 10_000_000)
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        // The pass runs INLINE (py/rust parity), so the send is launched on its
        // own task here and the kill is driven from the test body.
        let running = Task {
            await kernel.send([Item(id, .object([
                    "type": .string("send"), "text": .string("hi"),
                    "client_id": .string("c"),
                ]))])[0].result
        }

        // The `send` DISPATCH is the generation, so it stands in the table as
        // its own row — that is what makes it killable at all. (It used to be a
        // separate task that outlived an already-retired dispatch.)
        let taskId = try await waitForTask(kernel: kernel, agent: id, verb: "send")
        #expect(taskId > 0)  // a pid: an int from the one counter
        // MID-STREAM, as python's test kills it (inside the provider stream): a
        // kill that lands while the turn still assembles fails the turn's own
        // kernel calls (the subtree is marked killed before `onCancel` runs).
        try await waitUntil("the first token") { sink.first(type: "token") != nil }
        let before = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("c")]))])[0].result
        #expect(before["generating"].asBool == true)

        let c = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .integer(taskId)]))])[0].result
        #expect(c["cancelled"].asBool == true)
        #expect((c["killed"].asArray ?? []).contains(.integer(taskId)))

        // The client's turn is CLOSED OUT — the terminal `status(done)` lands
        // even though the generation was killed mid-stream (aborting a task
        // tells nobody). B2.3: its detail carries `started_at`, the only thing
        // that says how long the killed turn ran, and the keys are in python's
        // order.
        try await waitUntil("status(done)") { sink.statusDone() != nil }
        let status = try #require(sink.statusDone())
        #expect(status["client_id"].asString == "c")
        #expect(status["detail"]["reason"].asString == "cancelled")
        #expect(status["detail"]["started_at"].asDouble != nil)
        #expect(
            status["detail"].asObject.map { Array($0.keys) }
                == ["reason", "send_id", "started_at", "queue_depth"])
        #expect(sink.first(type: "done") == nil, "there is no `done` event")

        // No PHANTOM generation: the in-flight entry is gone, so `status` agrees
        // the agent is idle and the row has left the table.
        let after = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("c")]))])[0].result
        #expect(after["generating"].asBool == false)
        #expect(after["current"].isNull)
        let stillListed = await psRows(kernel).contains(where: { $0["id"].asInt == taskId })
        #expect(!stillListed)

        // THE CUT (owner, 2026-10-04): the killed turn keeps the incoming message
        // and a MARKER — never the half answer. A kernel `cancel` names no
        // stopper, so the marker says `cancel`.
        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.map { $0["role"].asString ?? "" } == ["user", "assistant"], "\(msgs)")
        #expect(msgs.first?["content"].asString == "hi")
        #expect(msgs.last?["content"].asString?.hasPrefix("[stopped by cancel]") == true, "\(msgs)")

        // The FIFO lock was released as the killed generation unwound: the agent
        // still answers a fresh send (a stranded lock would hang here forever).
        let next = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("again"),
                "client_id": .string("c2"),
            ]))])[0].result
        // That it RETURNED AT ALL is the proof: an inline send cannot answer
        // until it has taken the FIFO, so a lock stranded by the killed
        // generation would hang here rather than fail.
        #expect(next["response"].asString != nil)

        // The killed send's own dispatch unwinds as cancelled.
        _ = await running.value
    }

    @Test func historyIsPerAgentNotPerClient() async throws {
        // AGENT ID IS CONVERSATION ID. A second client on the same agent reads
        // the same thread — a second conversation is a second agent (python).
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: ["z"]) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("a"),
                "client_id": .string("c1"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        let h2 = await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c2")]))])[0].result
        #expect((h2["rows"].asArray ?? []).count == 2, "\(h2.serialize())")
        #expect(h2["client_id"].isNull, "no client axis on the store")
    }

    @Test func historyPersistsAcrossTurnsThroughTheRowStore() async throws {
        // Two sends on one agent: turn 2's persisted history must carry
        // turn 1 (the load→append round-trip through the kernel row store).
        let (kernel, id) = try await freshMockKernel(
            // DONE-TERMINATED: a call-less turn is nudged now, and the nudge is
            // itself a turn — so a fixture that just streams prose grows the
            // history under the assertion.
            config: baseConfig { MockProvider(tokens: ["x ", doneEnvelope]) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("one"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("two"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 4)
        let msgs =
            conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 4)
        #expect(msgs[0]["content"].asString == "one")
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[2]["content"].asString == "two")
        #expect(msgs[3]["role"].asString == "assistant")

        // THE ROW STORE IS THE RECORD. Read it directly: same four turns, in
        // order, each a row under its own id.
        let stored = conversationRows(kernel: kernel, id: id)
        #expect(stored.map { $0["role"].asString } == ["user", "assistant", "user", "assistant"])
        #expect(stored.map { $0["content"].asString ?? "" } == ["one", "x " + doneEnvelope, "two", "x " + doneEnvelope])
    }

    @Test func historyNeedsNoFileLegButAMemoryStore() async throws {
        // The conversation lives in the agent's MEMORY STORE — no file leg,
        // no sidecar, no chat agent. A backend keeps its turns in the store it
        // names; with NO store named, the
        // send fails loud before any turn (python `_send`'s `_store_of`).
        let registry = BundleRegistry()
        registry.register(
            "mock_backend.tools", MockBundle(config: baseConfig { MockProvider(tokens: ["ok ", doneEnvelope]) }))
        registry.register("ram_store.tools", RamStoreBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(
            id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
            meta: ["guid": .string(MOCK_GUID)])
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("ram_store.tools"), "id": .string("memory"),
                "callable_by": .object(["naked": .string("*")]),
            ]))])
        let r = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("kernel_state"),
                "handler_module": .string("mock_backend.tools"), "id": .string("naked"),
            ]))])[0].result
        let id = AgentId(r["id"].asString ?? "naked")
        let refused = await kernel.send([Item(id, .object(["type": .string("send"), "text": .string("hi")]))])[0].result
        #expect(refused["reason"].asString == "missing_arg", "\(refused.serialize())")
        #expect((refused["error"].asString ?? "").contains("memory_store_id"))
        kernel.update(id, [
            "memory_store_id": .string("memory"),
            "can_call": .object(["memory": .string("*"), "cli": .string("*")]),
        ])
        _ = await kernel.send([Item(id, .object(["type": .string("send"), "text": .string("hi")]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        let rows = conversationRows(kernel: kernel, id: id)
        #expect(rows.map { $0["role"].asString } == ["user", "assistant"])
        #expect(kernel.agent(AgentId("naked_chat")) == nil, "no chat agent, no sidecar")
        #expect(kernel.agent(id)?.childIds().isEmpty == true)
    }

    @Test func statusIdleShapeWhenNothingInFlight() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let s = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("c")]))])[0].result
        #expect(s["source"].asString == id.value)
        #expect(s["generating"].asBool == false)
        #expect(s["current"].isNull)
        #expect((s["mine_pending"].asArray ?? []).isEmpty)
        #expect(s["others_pending"].asInt == 0)
    }

    @Test func statusReflectsInFlightForOwner() async throws {
        // A slow stream stays in-flight long enough to observe `status`.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                
                    MockProvider(tokens: ["a", "b", "c"], chunkDelayNanos: 40_000_000)
            })
        // CONCURRENT send: the pass now runs INLINE (py/rust parity), so the
        // dispatch does not return until the generation is done. Observing it
        // in flight means launching it on another task and polling from here.
        let running = Task {
            _ = await kernel.send([Item(id, .object([
                    "type": .string("send"), "text": .string("hi there"),
                    "client_id": .string("c"),
                ]))])[0].result
        }
        // Poll until the generation is in-flight.
        var snap: JSON = .null
        for _ in 0..<50 {
            let s = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("c")]))])[0].result
            if s["generating"].asBool == true {
                snap = s
                break
            }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        #expect(snap["generating"].asBool == true)
        let cur = snap["current"]
        #expect(cur["is_mine"].asBool == true)
        #expect(cur["text"].asString == "hi there")  // owner sees own text
        #expect(["thinking", "streaming", "tool_calling"].contains(cur["phase"].asString ?? ""))
        _ = await running.value
    }

    @Test func statusRedactsOtherClientsText() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                MockProvider(tokens: ["a", "b"], chunkDelayNanos: 40_000_000)
            })
        // CONCURRENT send: the pass now runs INLINE (py/rust parity), so the
        // dispatch does not return until the generation is done. Observing it
        // in flight means launching it on another task and polling from here.
        let running = Task {
            _ = await kernel.send([Item(id, .object([
                    "type": .string("send"), "text": .string("secret"),
                    "client_id": .string("owner"),
                ]))])[0].result
        }
        // A DIFFERENT client polls — must not see the owner's text.
        var cur: JSON = .null
        for _ in 0..<50 {
            let s = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("nosy")]))])[0].result
            if s["generating"].asBool == true {
                cur = s["current"]
                break
            }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        #expect(cur["is_mine"].asBool == false)
        #expect(cur["text"].asString == nil)  // redacted
        #expect(cur["phase"].asString != nil)  // phase still visible
        _ = await running.value
    }

    @Test func fifoSerializesConcurrentSendsKeepingHistoryIntact() async throws {
        // Two overlapping sends on one client. FIFO serialization keeps
        // the load→save of chat history race-free: both turns survive.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                MockProvider(tokens: ["x"], chunkDelayNanos: 20_000_000)
            })
        async let a: JSON = kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("one"), "client_id": .string("c"),
            ]))])[0].result
        async let b: JSON = kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("two"), "client_id": .string("c"),
            ]))])[0].result
        _ = await (a, b)
        // Both generations: 2 turns each (user + assistant) = 4 rows.
        try await waitForHistory(kernel: kernel, id: id, count: 4)
        let msgs =
            conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 4)
        let texts = msgs.filter { $0["role"].asString == "user" }.compactMap {
            $0["content"].asString
        }
        #expect(Set(texts) == ["one", "two"])  // neither turn was clobbered
    }

    // MARK: - Context Protocol

    @Test func contextEstimateAndStrategies() async throws {
        func turn(_ role: String, _ content: String) -> JSON {
            .object(["role": .string(role), "content": .string(content)])
        }
        let body = (0..<10).map {
            turn(
                $0 % 2 == 0 ? "user" : "assistant", "turn\($0)-\(String(repeating: "x", count: 80))"
            )
        }
        #expect(AIContext.estimateOne(turn("user", String(repeating: "x", count: 400))) >= 100)
        #expect(AIContext.estimateTokens(body) > AIContext.estimateTokens(Array(body.prefix(2))))

        func ctx(_ summarize: AISummarizer?) -> ProjectionCtx {
            ProjectionCtx(
                budget: 1_000_000, recentN: 4, summarize: summarize, selfId: AgentId("ai"),
                kernel: nil)
        }
        // truncate: keeps first + recent, omittedMarker, NO summary.
        let tr = try await AIStrategies.truncate(body, [], .object([:]), ctx(nil))
        #expect(tr.omittedMarker && tr.summary == nil)
        #expect(tr.body.first?["content"].asString == body.first?["content"].asString)
        #expect(tr.body.last?["content"].asString == body.last?["content"].asString)

        // compact: recent verbatim + a summary artifact (via the mock provider).
        let cp = try await AIStrategies.compact(
            body, [], .object([:]),
            ctx(AIStrategies.makeSummarizer(MockProvider(tokens: ["SUMMARY"]))))
        #expect(cp.summary == "SUMMARY")
        // recent verbatim PLUS the pinned first turn (the task framing, which the
        // overflow split would otherwise take first).
        #expect(cp.body.count == 5)
        #expect(cp.body.first == body.first)
    }

    @Test func contextNoticeShapes() async throws {
        let backend = buildAIBackend(baseConfig { MockProvider() })
        let n = backend.contextNotice(
            strategy: "compact", summary: "MY SUMMARY", omittedMarker: false, droppedN: 7)
        #expect(n["role"].asString == "user")
        #expect(n["tool_calls"].asArray == nil)
        let c = n["content"].asString ?? ""
        #expect(c.contains("[context-notice]") && c.contains("MY SUMMARY"))
        #expect(c.contains("recall") && c.contains("memory agent") && c.contains("7 earlier turn"))
        let t = backend.contextNotice(
            strategy: "truncate", summary: nil, omittedMarker: true, droppedN: 3)
        let tc = t["content"].asString ?? ""
        #expect(tc.contains("omitted in place") && !tc.contains("Summary of the dropped span"))
    }

    @Test func recallPagesStoreAndContextStatusShape() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: ["noted"]) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"),
                "text": .string("my project codename is HALCYON"),
                "client_id": .string("chat"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)

        // recall finds the planted fact by substring, full durable history.
        let r = await kernel.send([Item(id, .object([
                "type": .string("recall"), "query": .string("codename"),
                "client_id": .string("chat"),
            ]))])[0].result
        let msgs = r["messages"].asArray ?? []
        #expect(
            msgs.contains { ($0["content"].asString ?? "").lowercased().contains("halcyon") })

        // context_status shape — default strategy, projection recorded.
        let st = await kernel.send([Item(id, .object(["type": .string("context_status")]))])[0].result
        #expect(st["context_window"].asInt != nil)
        #expect(st["budget"].asInt != nil)
        #expect(st["strategy"].asString == "compact")
        #expect(st["last_projection"]["fired"].asBool == false)  // under-budget send
        #expect(st["last_reaction"] == .null)  // no compaction fired
    }

    // ─── the queued event, the whole recall, the store cursor ───

    /// ONE event on contention, and it is the `status` one. Rust/swift emitted a
    /// bare `{type:'queued'}` ahead of it until 2026-09-02 — an undocumented type
    /// python never sends, which broke any per-submission event count and every
    /// cross-runtime event-stream diff.
    @Test func contendedSendsEmitOneQueuedStatusAndNoBareQueuedEvent() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                // DONE-TERMINATED: a call-less turn is nudged, and the nudge is a
                // second submission that contends for the same FIFO lock — a
                // second `queued` status the count would blame on this test.
                
                    MockProvider(
                        tokens: ["a", "b", "c ", doneEnvelope], chunkDelayNanos: 40_000_000)
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        let first = Task {
            _ = await kernel.send([Item(id, .object([
                    "type": .string("send"), "text": .string("one"), "client_id": .string("c"),
                ]))])[0].result
        }
        try await waitUntil("the first generation to be in flight") {
            (await kernel.send([Item(id, .object([
                "type": .string("status"), "client_id": .string("c"),
            ]))])[0].result)["generating"].asBool == true
        }
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("two"), "client_id": .string("c"),
            ]))])[0].result
        _ = await first.value

        #expect(
            sink.first(type: "queued") == nil,
            "the bare `queued` event is not part of the contract")
        let queued = sink.snapshot().filter {
            $0["type"].asString == "status" && $0["phase"].asString == "queued"
        }
        #expect(queued.count == 1, "one contended submission, one queued status")
        #expect(queued.first?["detail"]["ahead"].asInt != nil)
        // B2.17: python's `**detail` order — the caller's `send_id, ahead`, then
        // what `_emit_status` auto-fills. The in-flight entry is the FIRST turn,
        // so `started_at` is ITS start (python reads `_current` whoever holds it).
        #expect(
            queued.first?["detail"].asObject.map { Array($0.keys) }
                == ["send_id", "ahead", "started_at", "queue_depth"])
    }

    /// `recall` answers the turn WHOLE. Rust/swift capped each recalled turn at 2000
    /// chars while python returned it verbatim — same verb, same store, two different
    /// replies, and the cut half is exactly what a model pages back FOR.
    @Test func recallReturnsTheTurnWhole() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let long = "HEAD " + String(repeating: "x", count: 6000) + " TAIL"
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string(long), "client_id": .string("chat"),
            ]))])[0].result
        let r = await kernel.send([Item(id, .object([
                "type": .string("recall"), "query": .string("HEAD"),
                "client_id": .string("chat"),
            ]))])[0].result
        let content = (r["messages"].asArray ?? []).first?["content"].asString ?? ""
        #expect(content.count == long.count, "capped at \(content.count) of \(long.count)")
        #expect(content.hasSuffix("TAIL"), "the tail is what was being paged back")
    }

    /// The reaction cursor is the DURABLE STORE's length, not the model view's. The
    /// view is deduped/trimmed, so counting it puts the cursor short by exactly the
    /// turns the trimmers dropped — and `deriveReaction` then scans assistant turns
    /// written BEFORE the notice and scores a reaction that never happened.
    @Test func theCompactionCursorIsTheStoreLengthNotTheView() async throws {
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": ProviderScript([["x"]])]), backendIds: ["mock"])
        let agent = kernel.agent(ids["mock"]!)!

        // ① the store keeps every turn; the VIEW the prompt carries is shorter,
        //    because the superseded reflect deduped away.
        let store: [JSON] = [
            .object([
                "role": .string("user"),
                "content": .string("[reply to ks:a1 · core reflect]\nOLD TREE"),
            ]),
            .object(["role": .string("assistant"), "content": .string("noted")]),
            .object([
                "role": .string("user"),
                "content": .string("[reply to ks:a2 · core reflect]\nNEW TREE"),
            ]),
            .object(["role": .string("assistant"), "content": .string("noted again")]),
        ]
        let view = dedupeStaleReads(store)
        #expect(view.count < store.count, "the view must be shorter than the store")
        #expect(
            !view.map { $0["content"].asString ?? "" }.joined().contains("OLD TREE"),
            "the superseded reflect must be gone from the VIEW")

        // ② and the mark records the STORE's length, not the view's.
        let backend = buildAIBackend(baseConfig { MockProvider() })
        var fat: [JSON] = [.object(["role": .string("system"), "content": .string("sys")])]
        for i in 0..<60 {
            fat.append(
                .object([
                    "role": .string(i % 2 == 0 ? "user" : "assistant"),
                    "content": .string("turn\(i)-" + String(repeating: "x", count: 8000)),
                ]))
        }
        let outcome = try await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "probe", messages: fat,
            storeLen: store.count, kernel: kernel)
        guard case .projected = outcome else {
            Issue.record("the projection must fire, not failfast")
            return
        }
        let mark = readMark(backend, agent.id.value)
        #expect(
            mark?.0 == store.count,
            "the cursor is the store length, got \(String(describing: mark))")
        #expect(mark?.1 == "probe")
    }

    // ── the prompt the model reads (B1.1, B1.4, B1.9, B1.22) ─────

    // B1.1: NO JSON on the model path. A list of scalars is comma-joined, and
    // anything nested is the REPLY notation collapsed onto the line.
    @Test func renderReflectMatchesPythonOne() {
        let r: JSON = .object([
            "sentence": .string("S"),
            "a": .null,
            "b": .array([.string("x"), .string("y")]),
            "c": .object(["$self": .string("*")]),
        ])
        #expect(renderReflect(r) == "S  a=  b=x,y  c=<$self>*</$self>")
        // A list of scalars uses python `str`, so a bool is `True`.
        #expect(
            renderReflect(
                .object(["sentence": .string("S"), "t": .array([.bool(true), .integer(1)])]))
                == "S  t=True,1")
    }

    // B1.4: the estimate counts `json.dumps(…)` in CODE POINTS — an em dash
    // costs its six `\u2014` characters, not three.
    @Test func estimateOneCountsPythonJsonDumps() {
        #expect(
            AIContext.estimateOne(.object(["role": .string("user"), "content": .string("a — b")]))
                == 11)
    }

    // B5.6: the numeric context knobs parse EXACTLY as python's do.
    @Test func budgetMetaParsesLikePython() async throws {
        func agent(_ meta: OrderedCollections.OrderedDictionary<String, JSON>) async throws
            -> Agent
        {
            let (kernel, ids, _) = await bootMockKernel(
                config: baseConfig { MockProvider() }, backendIds: ["mock"],
                contextWindow: 131_072, meta: meta)
            return kernel.agent(ids["mock"]!)!
        }
        // `int(0.5)` is 0, `"-5"` is not digits so the reserve is 1024, and
        // `1000.0` clamps to 50.
        let a = try await agent([
            "context_window": .double(0.5), "output_reserve": .string("-5"),
            "recent_n": .double(1000.0),
        ])
        #expect(AIContext.resolveContextWindow(a, modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == 0)
        #expect(AIContext.outputReserve(a) == 1024)
        #expect(AIContext.budget(a, modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == 256)
        #expect(AIContext.recentN(a) == 50)
        // `"+12"` is not `isdigit()`, so the window falls through to the default;
        // `1000.0` is not an int, so the reserve is the default; `true` is 1.
        let b = try await agent([
            "context_window": .string("+12"), "output_reserve": .double(1000.0),
            "recent_n": .bool(true),
        ])
        #expect(AIContext.resolveContextWindow(b, modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == 131_072)
        #expect(AIContext.outputReserve(b) == 1024)
        #expect(AIContext.budget(b, modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == 130_048)
        #expect(AIContext.recentN(b) == 1)
    }

    // B1.9 + B1.22: the opt-in blocks read python TRUTHINESS, and the primer
    // reflect goes to the reserved `kernel` address (the root id differs per runtime).
    @Test func primerReflectTargetsKernelAliasAndFlagsArePythonTruthy() async throws {
        let cfg = baseConfig { MockProvider() }
        let (kernel, ids, _) = await bootMockKernel(
            config: cfg, backendIds: ["mock"], meta: ["context_titles": .integer(1)])
        let backend = buildAIBackend(cfg)
        let agent = kernel.agent(ids["mock"]!)!
        let prompt = await backend.assembleSystemPrompt(
            agent: agent, systemPrompt: nil, kernel: kernel)
        // The `kernel` address resolved to the ROOT: its reflect is the first block. An
        // unresolved target would have rendered an error here instead.
        #expect(prompt.contains("id=kernel_state"), "\(prompt.prefix(200))")
        #expect(!prompt.contains("unknown agent"))
        // `context_titles: 1` is truthy in python; `asBool` read only `true`.
        #expect(prompt.contains("## Titling your calls"))
    }

    // ── the turn text (B1.5, B1.6, B1.10, B1.20, B1.21, B5.7) ────

    // B1.21: a numeric `text` runs as its digits.
    @Test func aNumericTextIsSentAsItsDigits() async throws {
        let script = ProviderScript([["ok " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([
            Item(
                ids["mock"]!,
                .object([
                    "type": .string("send"), "text": .integer(586), "client_id": .string("c"),
                ]))
        ])[0].result
        #expect(await script.waitForCalls(1))
        #expect(try #require(script.request(0)).last?["content"].asString == "586")
        // A bool is NOT a number here — python's `isinstance` excludes it.
        let s2 = ProviderScript([["ok " + doneEnvelope]])
        let (k2, ids2, _) = await bootMockKernel(
            config: scriptedConfig(["mock": s2]), backendIds: ["mock"])
        _ = await k2.send([
            Item(
                ids2["mock"]!,
                .object([
                    "type": .string("send"), "text": .bool(true), "client_id": .string("c"),
                ]))
        ])[0].result
        #expect(await s2.waitForCalls(1))
        let live = try #require(s2.request(0)).last?["content"].asString
        #expect(live == "", "a bool stays the empty turn")
    }

    // B1.20: a message from ANOTHER agent opens with the header the KERNEL
    // vouches for — the sender is the dispatch's context, never its prose.
    @Test func aTurnFromAnotherAgentCarriesTheVouchedHeader() async throws {
        let a = ProviderScript([
            [
                renderCall(
                    "send",
                    [
                        "target_id": .string("mock2"), "reply_to": .string("null"),
                        "payload": .object([
                            "type": .string("send"), "text": .string("hello peer"),
                        ]),
                    ])
            ]
        ])
        let b = ProviderScript([["ok " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": a, "mock2": b]), backendIds: ["mock", "mock2"])
        _ = await kernel.send([
            Item(
                ids["mock"]!,
                .object([
                    "type": .string("send"), "text": .string("go"), "client_id": .string("c"),
                ]))
        ])[0].result
        #expect(await b.waitForCalls(1), "the peer never ran")
        let seen = b.requestText(0)
        #expect(seen.contains("[from mock \u{b7} msg "), "\(seen.suffix(300))")
        #expect(seen.contains("hello peer"))
    }

    // The `messages` context-array parameter of `send` is REMOVED: the
    // conversation is ALWAYS the agent's own (its memory store). A supplied list
    // is ignored — python's `_send` no longer reads it.
    @Test func aSuppliedMessagesListIsNotAConversation() async throws {
        let script = ProviderScript([["ok " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("live"),
                    "client_id": .string("c"),
                    "messages": .array([
                        .object(["role": .string("user"), "content": .string("supplied A")])
                    ]),
                ]))
        ])[0].result
        #expect(await script.waitForCalls(1))
        let msgs = try #require(script.request(0))
        #expect(msgs.count == 2, "[system] + the live turn")
        #expect(!JSON.array(msgs).serialize().contains("supplied A"))
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        #expect(conversationRows(kernel: kernel, id: id).map { $0["content"] }.first == .string("live"))
    }

    // B1.6: `options` via python `str`, `action` via `json.dumps`.
    @Test func inquiryTurnTextRendersOptionsAndActionLikePython() async throws {
        let script = ProviderScript([
            ["<call name=\"inquiry_answer\"><answer>b</answer></call>"]
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        _ = await kernel.send([
            Item(
                ids["mock"]!,
                .object([
                    "type": .string("inquiry"), "text": .string("pick"),
                    "options": .array([.bool(true), .string("b")]),
                    "action": .object(["a": .integer(1), "b": .string("\u{2014}")]),
                    "client_id": .string("c"),
                ]))
        ])[0].result
        #expect(await script.waitForCalls(1))
        let turn = try #require(script.request(0)).last?["content"].asString ?? ""
        #expect(turn.contains("Options: True, b"), "\(turn)")
        // `json.dumps(act)` — python separators, ensure_ascii, so the em dash is
        // escaped. (The plan's illustration shows it raw; the SOURCE escapes.)
        #expect(turn.contains(#"Proposed action: {"a": 1, "b": "\u2014"}"#), "\(turn)")
    }
}
