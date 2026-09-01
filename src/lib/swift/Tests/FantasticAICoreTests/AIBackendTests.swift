// Shared AI-backend machinery tests — driven through a MOCK `AIProvider` so the
// verb surface, the single-pass streaming, history persistence, cancellation,
// context management, and the stateless/error-shape config flags are all
// exercised WITHOUT any live LLM. The harness (scripted provider, mock bundle,
// in-memory file_bridge, polling helpers) lives in `MockBackend.swift`; the
// DISPATCH path each pass ends in lives in `RoutingTests.swift`;
// provider-specific wire tests stay with each backend target.

import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@testable import FantasticAICore

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
        #expect(r["verbs"]["send"].asString == "s")
    }

    @Test func backendStateMergesExtra() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send([Item(id, .object(["type": .string("backend_state")]))])[0].result
        #expect(r["provider"].asString == "mock")
        #expect(r["configured"].asBool == true)
    }

    @Test func unknownVerbErrors() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send([Item(id, .object(["type": .string("nope")]))])[0].result
        #expect(r["error"].asString == "unknown verb nope")
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

    @Test func sendWithoutTextErrors() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send([Item(id, .object(["type": .string("send")]))])[0].result
        #expect(r["error"].asString == "send requires text")
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
        try await waitForHistory(kernel: kernel, id: id, client: "c1", count: 2)

        let h = await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c1")]))])[0].result
        let msgs = h["messages"].asArray ?? []
        #expect(msgs.count == 2)
        #expect(msgs.first?["role"].asString == "user")
        #expect(msgs[0]["content"].asString == "hi")
        #expect(msgs[0]["complete"].asBool == true)
        #expect(msgs[0]["id"].asString == nil)  // non-stateless: no id
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[1]["content"].asString == "Hello")
        #expect(msgs[1]["tool_calls"].asArray == nil)
    }

    @Test func statelessRowsCarryIdAndDoNotFeedHistoryBack() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig(stateless: true) { .provider(MockProvider(tokens: ["x"])) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("one"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        let h = await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c")]))])[0].result
        let msgs = h["messages"].asArray ?? []
        #expect(msgs[0]["id"].asString != nil)  // stateless: id present
        #expect(msgs[1]["id"].asString != nil)
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
            #"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"reflect","tree":"none"}}}</tool_call>"#
        let script = ProviderScript([[envelope], ["seen it " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("go"),
                "system_prompt": .string(
                    "CALLER BLOCK: you drive tools via <tool_call> envelopes."),
                "client_id": .string("c"),
            ]))])[0].result
        #expect(await script.waitForCalls(2), "the dispatched call's reply never came back")
        // Wait for ALL FOUR rows the assertion below reads.
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)

        // Pass 2's system message is the CALLER's, still — not the substrate's
        // default block (whose `SEND_TO_HOWTO` heading would be in there).
        let turn2 = script.requestText(1)
        #expect(turn2.contains("CALLER BLOCK"))
        #expect(!turn2.contains("Reaching other agents"))
        #expect(turn2.contains(" · core "))

        // Four rows: the envelope turn and the reply turn it caused
        // (user+assistant each). The reply's done-signalled answer ends the chain.
        let msgs = await historyRows(kernel: kernel, id: id, client: "c")
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
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
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
            (await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c")]))])[0].result)[
                "messages"
            ].asArray ?? []
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
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        try await waitUntil("a terminal done event") { sink.first(type: "done") != nil }

        let done = sink.first(type: "done")
        #expect(done?["error"].asString == nil)
        #expect(done?["accumulated"].asString == "<think>all of my turn</think>")
        #expect(done?["accumulated"].asString?.isEmpty == false)

        // The conversation keeps no deliberation — the turn persists empty
        // content, and that is the honest record of an answer-free turn.
        let msgs =
            (await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c")]))])[0].result)[
                "messages"
            ].asArray ?? []
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
        let h = await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c")]))])[0].result
        let msgs = h["messages"].asArray ?? []
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
        #expect(taskId.hasPrefix("\(id.value):"))  // a pid: "<agent_id>:<n>"
        let before = await kernel.send([Item(id, .object(["type": .string("status"), "client_id": .string("c")]))])[0].result
        #expect(before["generating"].asBool == true)

        let c = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .string(taskId)]))])[0].result
        #expect(c["cancelled"].asBool == true)
        #expect((c["killed"].asArray ?? []).contains(.string(taskId)))

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
        let stillListed = await psRows(kernel).contains(where: { $0["id"].asString == taskId })
        #expect(!stillListed)

        // The killed turn persisted NOTHING — not the partial assistant row, and
        // not the user row either. The exchange never completed, so no half of
        // it is a record. This matches python (`_save_history`, one call site
        // after the stream) and rust (`agent_loop.rs`, likewise): a turn is
        // persisted as a UNIT or not at all.
        let h = await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c")]))])[0].result
        let msgs = h["messages"].asArray ?? []
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

    @Test func historyIsPerClient() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["z"])) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("a"),
                "client_id": .string("c1"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, client: "c1", count: 2)
        let h2 = await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c2")]))])[0].result
        #expect((h2["messages"].asArray ?? []).isEmpty)
        #expect(h2["client_id"].asString == "c2")
    }

    @Test func historyPersistsAcrossTurnsThroughTheChatStore() async throws {
        // Two sends on one client: turn 2's persisted history must carry
        // turn 1 (the load→append round-trip through the JSONL sidecar).
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["x"])) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("one"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("two"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)
        let msgs =
            (await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c")]))])[0].result)[
                "messages"
            ].asArray ?? []
        #expect(msgs.count == 4)
        #expect(msgs[0]["content"].asString == "one")
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[2]["content"].asString == "two")
        #expect(msgs[3]["role"].asString == "assistant")

        // THE SIDECAR IS THE RECORD. `history` is a read of the JSONL file
        // `agents/<self>/chat_<client>.jsonl` on the backend's own leg. Read
        // the leg directly: same four turns, in order, one line per turn.
        let stored = await chatStoreRows(kernel: kernel, id: id, client: "c")
        #expect(stored.map { $0["content"].asString } == ["one", "x", "two", "x"])
    }

    @Test func historyStaysEmptyWithoutFileBridge() async throws {
        // No file_bridge_id wired ⇒ no persistence (RAM-empty), NOT a
        // silent fallback: the JSONL sidecar has no leg to ride, so nothing
        // accumulates. The send still streams.
        let registry = BundleRegistry()
        registry.register(
            "mock_backend.tools", MockBundle(config: baseConfig { .provider(MockProvider()) }))
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
        // Give the stream a beat, then confirm history is still empty.
        try? await Task.sleep(nanoseconds: 60_000_000)
        let h = await kernel.send([Item(id, .object(["type": .string("history")]))])[0].result
        #expect((h["messages"].asArray ?? []).isEmpty)
        // And no chat AGENT exists — the sidecar is a file, not a child, and
        // with no leg there is simply no file anywhere.
        #expect(kernel.agent(AgentId("naked_chat")) == nil)
    }

    @Test func theChatSidecarIsAJSONLFileOnTheOwnLeg() async throws {
        // WHERE the conversation lives, asserted as structure: an append-only
        // JSONL file on the backend's OWN leg, one line per turn — NOT a chat
        // agent (durable_memory is gone; memory is plain files) and not a
        // loader blob sidecar.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["ok"])) })
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("hi"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)

        let rows = await chatStoreRows(kernel: kernel, id: id, client: "c")
        #expect(rows.count == 2)
        #expect(rows.first?["role"].asString == "user")
        #expect(rows.last?["role"].asString == "assistant")
        // No chat AGENT is mounted any more — a second turn appends to the
        // same file rather than minting anything.
        #expect(kernel.agent(AgentId("\(id.value)_chat")) == nil)
        _ = await kernel.send([Item(id, .object([
                "type": .string("send"), "text": .string("again"),
                "client_id": .string("c"),
            ]))])[0].result
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)
        #expect((await chatStoreRows(kernel: kernel, id: id, client: "c")).count == 4)
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
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)
        let msgs =
            (await kernel.send([Item(id, .object(["type": .string("history"), "client_id": .string("c")]))])[0].result)[
                "messages"
            ].asArray ?? []
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

        // truncate: keeps first + recent, omittedMarker, NO summary.
        let tr = AIStrategies.truncate(body: body, budget: 1_000_000)
        #expect(tr.omittedMarker && tr.summary == nil)
        #expect(tr.body.first?["content"].asString == body.first?["content"].asString)
        #expect(tr.body.last?["content"].asString == body.last?["content"].asString)

        // compact: recent verbatim + a summary artifact (via the mock provider).
        let cp = await AIStrategies.compact(
            body: body, recentN: 4, budget: 1_000_000, provider: MockProvider(tokens: ["SUMMARY"]))
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
        try await waitForHistory(kernel: kernel, id: id, client: "chat", count: 2)

        // recall finds the planted fact by substring, full durable history.
        let r = await kernel.send([Item(id, .object([
                "type": .string("recall"), "query": .string("codename"),
                "client_id": .string("chat"),
            ]))])[0].result
        let msgs = r["messages"].asArray ?? []
        #expect(
            msgs.contains { ($0["content"].asString ?? "").lowercased().contains("halcyon") })
        #expect(r["client_id"].asString == "chat")

        // context_status shape — default strategy, projection recorded.
        let st = await kernel.send([Item(id, .object(["type": .string("context_status")]))])[0].result
        #expect(st["context_window"].asInt != nil)
        #expect(st["budget"].asInt != nil)
        #expect(st["strategy"].asString == "compact")
        #expect(st["last_projection"]["fired"].asBool == false)  // under-budget send
        #expect(st["last_reaction"] == .null)  // no compaction fired
    }
}

// The `author` verb + its SELF-CORRECT loop were DELETED — python has no `author`
// subsystem (a coder AND its loop emerge from base abstractions + genesis readmes).
