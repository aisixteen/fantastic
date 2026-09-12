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
import Testing

@testable import FantasticAICore

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

    @Test func reflectMergesExtraAndKeepsOrder() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send([Item(id, .object(["type": .string("reflect")]))])[0].result
        #expect(r["id"].asString == id.value)
        #expect(r["kind"].asString == "mock_backend")
        #expect(r["provider"].asString == "mock")
        #expect(r["extra"].asString == "yes")
        // The verb map is python's `_build` table, first doc line each, in
        // its order — no per-backend wording, no `backend_state`, and `cancel`
        // is a top-level field beside it, as python spells it.
        #expect(
            r["verbs"].asObject?.keys.elementsEqual([
                "reflect", "send", "inquiry", "history", "status", "context_status", "recall",
                "pull", "grep", "context_recall", "boot",
            ]) == true, "\(r["verbs"])")
        #expect(
            r["verbs"]["reflect"].asString
                == "Identity + model + endpoint + generating flag + context posture. No args.")
        #expect(r["verbs"]["send"].asString?.hasPrefix("args: text:str (req), client_id:str? (default 'cli').") == true)
        #expect(r["verbs"]["backend_state"].isNull && r["verbs"]["cancel"].isNull)
        #expect(r["cancel"].asString == CANCEL_DOC)
    }

    @Test func backendStateIsNotAVerb() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send([Item(id, .object(["type": .string("backend_state")]))])[0].result
        #expect(r["error"].asString == "mock_backend: unknown type 'backend_state'")
    }

    @Test func unknownVerbErrors() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send([Item(id, .object(["type": .string("nope")]))])[0].result
        #expect(r["error"].asString == "mock_backend: unknown type 'nope'")
    }

    /// `send` answers with the FINISHED text — python/rust parity. It used to
    /// ack `{queued, stream_id, message_id}` and run the pass on its own task,
    /// so a caller reading `response` got the text on two kernels and an empty
    /// ack on the third.
    @Test func sendReturnsTheCompletedText() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
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
            config: baseConfig { .provider(MockProvider()) })
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
                .provider(MockProvider(tokens: ["a", "b", "c"], chunkDelayNanos: 40_000_000))
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

    @Test func makeProviderRefusalReturnedVerbatim() async throws {
        let refusal: JSON = .object([
            "error": .string("no_key"), "reason": .string("missing"),
        ])
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .refused(refusal) })
        let r = await kernel.send([Item(id, .object(["type": .string("send"), "text": .string("hi")]))])[0].result
        #expect(r["error"].asString == "no_key")
        #expect(r["reason"].asString == "missing")
        #expect(r["queued"].asBool == nil)
    }

    @Test func streamPersistsUserAndAssistantHistory() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["Hel", "lo"])) })
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
            config: baseConfig { .provider(MockProvider(tokens: ["x ", doneEnvelope])) })
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
        //   • the token/done events carry the RAW completion — `accumulated` is
        //     contractually the concatenation of the deltas the client received;
        //   • the DURABLE assistant turn is STRIPPED, because re-feeding a
        //     reasoning model its own prior thought is against every vendor's
        //     guidance and compounds turn over turn.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(
                    MockProvider(tokens: ["<think>", "weigh ", "options", "</think>", "42"]))
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev) } }
        defer { pump.cancel() }

        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("what is it"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        try await waitUntil("a terminal done event") { sink.first(type: "done") != nil }

        // RAW on the wire — and equal to the deltas actually emitted.
        let raw = "<think>weigh options</think>42"
        #expect(sink.first(type: "done")?["accumulated"].asString == raw)
        let deltas = sink.snapshot().filter { $0["type"].asString == "token" }
            .compactMap { $0["delta"].asString }.joined()
        #expect(deltas == raw)
        let lastToken = sink.snapshot().last { $0["type"].asString == "token" }
        #expect(lastToken?["accumulated"].asString == raw)

        // STRIPPED in the durable store.
        let msgs =
            conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 2)
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[1]["content"].asString == "42")
    }

    @Test func aThinkingOnlyTurnIsNotAnEmptyCompletion() async throws {
        // The measured failure this whole mechanism exists for (gemma4:12b,
        // 2026-08-02): the model spent its ENTIRE turn reasoning. Reading one
        // channel made that an empty completion that silently ended the chain,
        // indistinguishable from "the model said nothing". The reasoning text must
        // reach the caller (and a reader's parser) even though it is never
        // persisted as conversation.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(MockProvider(tokens: ["<think>", "all of my turn", "</think>"]))
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev) } }
        defer { pump.cancel() }

        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        try await waitUntil("a terminal done event") { sink.first(type: "done") != nil }

        let done = sink.first(type: "done")
        #expect(done?["error"].asString == nil)
        #expect(done?["accumulated"].asString == "<think>all of my turn</think>")
        #expect(done?["accumulated"].asString?.isEmpty == false)

        // The conversation keeps no deliberation — the turn persists empty
        // content, and that is the honest record of an answer-free turn.
        let msgs =
            conversationRows(kernel: kernel, id: id)
        #expect(msgs.count == 2)
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[1]["content"].asString == "")
    }

    @Test func anErroredStreamPersistsNEITHERHalfOfTheTurn() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(throwsError: true)) })
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
                .provider(
                    MockProvider(
                        tokens: Array(repeating: "x", count: 200),
                        chunkDelayNanos: 10_000_000))
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev) } }
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
        let before = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("c")]))])[0].result
        #expect(before["generating"].asBool == true)

        let c = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .integer(taskId)]))])[0].result
        #expect(c["cancelled"].asBool == true)
        #expect((c["killed"].asArray ?? []).contains(.integer(taskId)))

        // The client's turn is CLOSED OUT — a terminal `done` lands even though
        // the generation was killed mid-stream (aborting a task tells nobody).
        try await waitUntil("a terminal done event") { sink.first(type: "done") != nil }
        let done = sink.first(type: "done")
        #expect(done?["client_id"].asString == "c")
        #expect(done?["error"].asString == "cancelled")
        let status = sink.snapshot().last { $0["type"].asString == "status" }
        #expect(status?["phase"].asString == "done")
        #expect(status?["detail"]["reason"].asString == "cancelled")

        // No PHANTOM generation: the in-flight entry is gone, so `status` agrees
        // the agent is idle and the row has left the table.
        let after = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("c")]))])[0].result
        #expect(after["generating"].asBool == false)
        #expect(after["current"].isNull)
        let stillListed = await psRows(kernel).contains(where: { $0["id"].asInt == taskId })
        #expect(!stillListed)

        // The killed turn persisted NOTHING — not the partial assistant row, and
        // not the user row either. The exchange never completed, so no half of
        // it is a record. This matches python (`_save_history`, one call site
        // after the stream) and rust (`agent_loop.rs`, likewise): a turn is
        // persisted as a UNIT or not at all.
        let msgs = conversationRows(kernel: kernel, id: id)
        #expect(msgs.isEmpty, "an interrupted turn left a half-exchange behind: \(msgs)")

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
            config: baseConfig { .provider(MockProvider(tokens: ["z"])) })
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
            config: baseConfig { .provider(MockProvider(tokens: ["x ", doneEnvelope])) })
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

    @Test func historyNeedsNoFileLeg() async throws {
        // The conversation lives in the KERNEL ROW STORE — no `file_bridge_id`,
        // no sidecar, no chat agent. A backend with no leg at all still keeps
        // its turns; persistence of the store is the kernel's concern.
        let registry = BundleRegistry()
        registry.register(
            "mock_backend.tools", MockBundle(config: baseConfig { .provider(MockProvider(tokens: ["ok ", doneEnvelope])) }))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let r = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("mock_backend.tools"), "id": .string("naked"),
            ]))])[0].result
        let id = AgentId(r["id"].asString ?? "naked")
        _ = await kernel.send([Item(id, .object(["type": .string("send"), "text": .string("hi")]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        let rows = conversationRows(kernel: kernel, id: id)
        #expect(rows.map { $0["role"].asString } == ["user", "assistant"])
        #expect(kernel.agent(AgentId("naked_chat")) == nil, "no chat agent, no sidecar")
        #expect(kernel.agent(id)?.childIds().isEmpty == true)
    }

    @Test func statusIdleShapeWhenNothingInFlight() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
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
                .provider(
                    MockProvider(tokens: ["a", "b", "c"], chunkDelayNanos: 40_000_000))
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
                .provider(MockProvider(tokens: ["a", "b"], chunkDelayNanos: 40_000_000))
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
                .provider(MockProvider(tokens: ["x"], chunkDelayNanos: 20_000_000))
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

        let kernel = Kernel()
        func ctx(_ summarize: (@Sendable ([JSON]) async throws -> String)?) -> ProjectionCtx {
            ProjectionCtx(
                budget: 1_000_000, recentN: 4, summarize: summarize, selfId: AgentId("ai"),
                kernel: kernel)
        }
        // truncate: keeps first + recent, omittedMarker, NO summary.
        let tr = await AIStrategies.truncate(body, [], .object([:]), ctx(nil))
        #expect(tr.omittedMarker && tr.summary == nil)
        #expect(tr.body.first?["content"].asString == body.first?["content"].asString)
        #expect(tr.body.last?["content"].asString == body.last?["content"].asString)

        // compact: recent verbatim + a summary artifact (via the mock provider).
        let cp = await AIStrategies.compact(
            body, [], .object([:]),
            ctx(AIStrategies.makeSummarizer(MockProvider(tokens: ["SUMMARY"]))))
        #expect(cp.summary == "SUMMARY")
        // recent verbatim PLUS the pinned first turn (the task framing, which the
        // overflow split would otherwise take first).
        #expect(cp.body.count == 5)
        #expect(cp.body.first == body.first)
    }

    @Test func contextNoticeShapes() async throws {
        let backend = buildAIBackend(baseConfig { .provider(MockProvider()) })
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
            config: baseConfig { .provider(MockProvider(tokens: ["noted"])) })
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
                .provider(
                    MockProvider(
                        tokens: ["a", "b", "c ", doneEnvelope], chunkDelayNanos: 40_000_000))
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev) } }
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
    }

    /// `recall` answers the turn WHOLE. Rust/swift capped each recalled turn at 2000
    /// chars while python returned it verbatim — same verb, same store, two different
    /// replies, and the cut half is exactly what a model pages back FOR.
    @Test func recallReturnsTheTurnWhole() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
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
        let view = dropAnsweredReflects(dedupeStaleReads(store))
        #expect(view.count < store.count, "the view must be shorter than the store")
        #expect(
            !view.map { $0["content"].asString ?? "" }.joined().contains("OLD TREE"),
            "the superseded reflect must be gone from the VIEW")

        // ② and the mark records the STORE's length, not the view's.
        let backend = buildAIBackend(baseConfig { .provider(MockProvider()) })
        var fat: [JSON] = [.object(["role": .string("system"), "content": .string("sys")])]
        for i in 0..<60 {
            fat.append(
                .object([
                    "role": .string(i % 2 == 0 ? "user" : "assistant"),
                    "content": .string("turn\(i)-" + String(repeating: "x", count: 8000)),
                ]))
        }
        let outcome = await backend.projectContext(
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
}
