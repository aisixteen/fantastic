// Stage B batch 9 — what a caller SEES when a turn does not go well, and what
// the events it does see are shaped like.
//
// Every expectation here is python's (`ai_core/core.py`), pinned as text or as a
// key list. The two halves that matter:
//
//   • a provider failure RAISES — `{error:"<Type>: <msg>", raised:true}`, never
//     dressed up as a `{error, client_id}` refusal;
//   • `status(phase:'done')` IS the completion. There is no `done` event, the
//     token event carries `text`, and every detail key is in python's order.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// A provider that fails the way a real one does: it throws BEFORE the first
/// token, carrying a python-shaped error.
private final class FailingProvider: AIProvider, @unchecked Sendable {
    let model = "mock"
    let error: Error
    init(_ error: Error) { self.error = error }
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let error = error
        return AsyncThrowingStream { $0.finish(throwing: error) }
    }
}

/// A provider that never answers — the stalled upstream the ceiling exists for.
private final class StalledProvider: AIProvider, @unchecked Sendable {
    let model = "mock"
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        AsyncThrowingStream { continuation in
            let t = Task {
                try? await Task.sleep(nanoseconds: 60_000_000_000)
                continuation.finish()
            }
            continuation.onTermination = { _ in t.cancel() }
        }
    }
}

@Suite("AI events and refusals")
struct EventsAndRefusalsTests {

    // ── B2.1 / B2.16: a provider failure raises ──────────────────────────

    /// The handler RAISES. The kernel settles `failed` with
    /// `{error:"<Type>: <msg>", raised:true}` — a crash the caller can tell from
    /// a decision — while the status event carries the bare `str(e)`.
    @Test func aProviderErrorSettlesRaised() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                FailingProvider(PyError("RuntimeError", "upstream said no"))
            })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        let out = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("hi"), "client_id": .string("tab"),
                ]))
        ])[0]
        #expect(out.status == .failed)
        #expect(out.result["error"].asString == "RuntimeError: upstream said no")
        #expect(out.result["raised"].asBool == true)
        #expect(out.result["client_id"] == .null, "a raise carries no client_id")

        try await waitUntil("status(done)") { sink.statusDone() != nil }
        let detail = try #require(sink.statusDone()?["detail"])
        #expect(detail["reason"].asString == "error")
        // `str(e)` — the message alone, NOT the `<Type>: ` prefixed rendering
        // the settle carries.
        #expect(detail["error"].asString == "upstream said no")
        // B2.16 + B2.17: the detail keys, in python's order.
        #expect(
            detail.asObject.map { Array($0.keys) }
                == ["reason", "error", "send_id", "started_at", "queue_depth"])
    }

    /// A backend builds its provider INSIDE the turn (python `_run`): a build
    /// that raises (responses' sampling
    /// naming an owned key, a missing `model`) is seen as `thinking` then
    /// `done(reason:error)`, and the handler raises.
    @Test func aProviderBuildFailureEndsTheTurnWithDoneError() async throws {
        var cfg = baseConfig { throw PyError("RuntimeError", "unreachable") }
        cfg.makeProvider = { _, _, _ in
            throw PyError("ValueError", "sampling sets an owned key: 'model'")
        }
        let (kernel, id) = try await freshMockKernel(config: cfg)
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }
        let out = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("hi"), "client_id": .string("t"),
                ]))
        ])[0]
        #expect(out.status == .failed, "\(out.result.serialize())")
        #expect(out.result["error"].asString == "ValueError: sampling sets an owned key: 'model'")
        try await waitUntil("status(done)") { sink.statusDone() != nil }
        let phases = sink.snapshot().filter { $0["type"].asString == "status" }
            .compactMap { $0["phase"].asString }
        #expect(phases == ["thinking", "done"], "\(phases)")
        #expect(sink.statusDone()?["detail"]["reason"].asString == "error")
        #expect(
            sink.statusDone()?["detail"]["error"].asString == "sampling sets an owned key: 'model'")
    }

    // ── B2.2: a projection refusal is the reply, verbatim ────────────────

    /// The too_small failsafe means the MODEL WAS NEVER CALLED: python returns
    /// the refusal body exactly as the seam built it — no `client_id` bolted on
    /// — and emits no `status(done)`, because no generation ever started. The
    /// `context:too_small` event is the one thing that fires.
    @Test func aProjectionRefusalIsReturnedVerbatimWithNoDoneStatus() async throws {
        let (kernel, ids, _) = await bootMockKernel(
            config: baseConfig { MockProvider() },
            backendIds: ["mock"],
            // Smaller than the system block alone: the seam fails the turn.
            contextWindow: 64)
        let id = ids["mock"]!
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        let r = await kernel.send([
            Item(
                id,
                .object(["type": .string("send"), "text": .string("hi"), "client_id": .string("c")])
            )
        ])[0].result
        #expect(r["error"].asString?.hasPrefix("mock_backend: context_insufficient — ") == true)
        #expect(
            r.asObject.map { Array($0.keys) } == ["error"],
            "the refusal is returned VERBATIM: \(r.serialize())")

        try await waitUntil("context:too_small") {
            sink.snapshot().contains { $0["phase"].asString == "too_small" }
        }
        let statuses = sink.snapshot().filter { $0["type"].asString == "status" }
        #expect(
            !statuses.contains { $0["phase"].asString == "done" },
            "no generation started, so nothing announces one ending")
    }

    // ── B2.6: the send id ────────────────────────────────────────────────

    /// python `secrets.token_urlsafe(8)`: ELEVEN base64url characters, no
    /// prefix. Swift minted `snd_<8 hex>`.
    @Test func sendIdIsElevenCharsOfBase64Url() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }
        _ = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("hi"), "client_id": .string("tab"),
                ]))
        ])[0].result
        try await waitUntil("status(done)") { sink.statusDone() != nil }
        let sendId = try #require(sink.statusDone()?["detail"]["send_id"].asString)
        #expect(sendId.count == 11, Comment(rawValue: sendId))
        #expect(
            sendId.range(of: "^[A-Za-z0-9_-]{11}$", options: .regularExpression) != nil,
            Comment(rawValue: sendId))
    }

    // ── B2.10: `_safe_client` is per CODE POINT ──────────────────────────

    @Test func safeClientMapsCodePointsLikePython() {
        let backend = buildAIBackend(baseConfig { MockProvider() })
        // A DECOMPOSED `é` is TWO code points to python: the letter survives, the
        // combining accent does not. Swift mapped Characters, so it kept both.
        #expect(backend.safeClient("e\u{301}x") == "e_x")
        // …and the PRECOMPOSED one is a single alphanumeric code point.
        #expect(backend.safeClient("\u{e9}x") == "\u{e9}x")
        // A zero-width joiner is not alphanumeric.
        #expect(backend.safeClient("a\u{200d}b") == "a_b")
        // python's `strip` removes U+001C..U+001F; `.whitespacesAndNewlines` does not.
        #expect(backend.safeClient("\u{1f} p \u{1f}") == "p")
        // Empty (or all-whitespace) falls back to the default client id.
        #expect(backend.safeClient("   ") == "cli")
        // NO LENGTH CAP — python truncates nothing.
        #expect(backend.safeClient(String(repeating: "\u{e9}", count: 70)).count == 70)
        // Unicode digits are alphanumeric to python (`'²'.isnumeric()`).
        #expect(backend.safeClient("v\u{b2}") == "v\u{b2}")
        #expect(backend.safeClient("a/b c.d-e_f") == "a_b_c.d-e_f")
    }

    // ── B2.14 / B2.15: the token event, and the absence of `done` ────────

    @Test func tokenEventIsTypeTextSourceClientId() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: ["ab"]) })
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }
        _ = await kernel.send([
            Item(
                id,
                .object(["type": .string("send"), "text": .string("hi"), "client_id": .string("c")])
            )
        ])[0].result
        try await waitUntil("a token event") { sink.first(type: "token") != nil }
        let tok = try #require(sink.first(type: "token"))
        #expect(tok.asObject.map { Array($0.keys) } == ["type", "text", "source", "client_id"])
        #expect(tok["text"].asString == "ab")
        #expect(tok["source"].asString == id.value)
        #expect(tok["client_id"].asString == "c")
        #expect(sink.first(type: "done") == nil, "`status(done)` IS the completion")
    }

    // ── B2.18: the ceiling covers the WHOLE generation ───────────────────

    /// A provider that never yields a first token used to run forever: swift
    /// only compared a deadline BETWEEN chunks, so a stall before chunk one was
    /// unbounded. python wraps the whole of `_run` in `asyncio.timeout`.
    @Test func aStalledProviderTimesOutAtTheCeiling() async throws {
        var cfg = baseConfig { StalledProvider() }
        cfg.sendTimeout = 0.1
        let (kernel, id) = try await freshMockKernel(config: cfg)
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        let r = await kernel.send([
            Item(
                id,
                .object(["type": .string("send"), "text": .string("hi"), "client_id": .string("c")])
            )
        ])[0].result
        // The float is rendered python's way — `0.1`, never `0`.
        #expect(r["error"].asString == "send: timeout after 0.1s")
        #expect(r["client_id"].asString == "c")
        try await waitUntil("status(done)") { sink.statusDone() != nil }
        #expect(sink.statusDone()?["detail"]["reason"].asString == "timeout")

        // AND THE AGENT IS NOT STRANDED: the FIFO guard was released, so the
        // next turn runs.
        var next = cfg
        next.makeProvider = { _, _, _ in MockProvider(tokens: ["ok"]) }
        _ = next
        let s = await kernel.send([
            Item(id, .object(["type": .string("status"), "client_id": .string("c")]))
        ])[0].result
        #expect(s["generating"].asBool == false)
    }

    // ── B2.19: the completion is announced BEFORE the turn is persisted ──

    @Test func statusDoneOkIsEmittedBeforeTheTurnIsPersisted() async throws {
        // The turn ENDS THE CHAIN (`done`), so the only status events are this
        // turn's three: a nudge would open another turn whose own events land
        // after these rows and make "the last status" mean something else.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: ["ok " + doneEnvelope]) })
        let rowsAtStatus = RowCountProbe()
        // A SYNCHRONOUS subscriber: it runs inside `emit`, so what it reads is
        // the store as it stood at that instant — an async inbox reader could
        // only ever see the state after both writes.
        let token = kernel.subscribe { ev, _ in
            guard ev["payload"]["type"].asString == "status" else { return }
            // CONVERSATION rows only: the dispatch's own `call`/`task`
            // bookkeeping lands in the same table and says nothing about when
            // the turn was persisted.
            rowsAtStatus.record(conversationRows(kernel: kernel, id: id).count)
        }
        defer { kernel.unsubscribe(token) }

        let before = conversationRows(kernel: kernel, id: id).count
        // A browser-style caller, so the events are EMITS on the inbox (a `cli`
        // caller's would be sends to the cli agent).
        _ = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("hi"), "client_id": .string("tab"),
                ]))
        ])[0].result
        try await waitForHistory(kernel: kernel, id: id, count: 2)

        #expect(rowsAtStatus.last() == before, "the last status ran before the two turn rows")
        #expect(conversationRows(kernel: kernel, id: id).count == before + 2)
    }

    // ── B2.7: ONE caller route ───────────────────────────────────────────

    /// `client_id == 'cli'` → every stream event is a SEND to the `cli` agent
    /// (whose handler prints it), and NOTHING lands on the backend's inbox.
    /// python `test_cli_caller_routes_to_cli_only`.
    @Test func cliCallerRoutesToCliOnly() async throws {
        let (kernel, ids, recorder) = await bootMockKernel(
            config: baseConfig { MockProvider(tokens: ["hel", "lo ", doneEnvelope]) },
            backendIds: ["mock"])
        let id = ids["mock"]!
        // The cli agent the backend streams to — granted both ways, like the
        // daemon wires it.
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("mock_recorder.tools"), "id": .string("cli"),
                    "callable_by": .object([id.value: .string("*")]),
                ]))
        ])[0].result
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        _ = await kernel.send([
            Item(id, .object(["type": .string("send"), "text": .string("hi")]))
        ])[0].result
        try await waitUntil("status(done) at the cli") {
            recorder.payloads().contains {
                $0["type"].asString == "status" && $0["phase"].asString == "done"
            }
        }
        let atCli = recorder.payloads()
        let tokens = atCli.filter { $0["type"].asString == "token" }
        #expect(tokens.compactMap { $0["text"].asString }.joined().hasPrefix("hello "))
        #expect(atCli.allSatisfy { $0["client_id"].asString == "cli" })
        #expect(
            Set(atCli.compactMap { $0["type"].asString }).isSubset(of: ["token", "status"]),
            "\(atCli.map { $0["type"].asString ?? "?" })")
        try await Task.sleep(nanoseconds: 50_000_000)
        // The backend's OWN inbox carries the dispatch that was sent TO it — C76,
        // python's `_fanout` at dispatch start — and NOTHING of the stream: no
        // token, no `say`, no status. Python asserts exactly this (its `emits`
        // filter over `token`/`say`/`status(done)` on the backend's own id).
        let leaked = sink.snapshot().filter {
            let t = $0["type"].asString
            return t == "token" || t == "say"
                || (t == "status" && $0["phase"].asString == "done")
        }
        #expect(leaked.isEmpty, "unexpected leak to own inbox: \(leaked.map { $0.serialize() })")
    }

    /// Any other client id → the backend's OWN inbox, and nothing is sent to
    /// the cli. python `test_browser_caller_routes_to_own_inbox_only`.
    @Test func browserCallerRoutesToOwnInboxOnly() async throws {
        let (kernel, ids, recorder) = await bootMockKernel(
            config: baseConfig { MockProvider(tokens: ["hi ", doneEnvelope]) },
            backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("mock_recorder.tools"), "id": .string("cli"),
                    "callable_by": .object([id.value: .string("*")]),
                ]))
        ])[0].result
        let sink = EventSink()
        let inbox = kernel.ensureInbox(id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }

        _ = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("hi"), "client_id": .string("tab-7"),
                ]))
        ])[0].result
        try await waitUntil("status(done) on the inbox") { sink.statusDone() != nil }
        #expect(sink.first(type: "token")?["client_id"].asString == "tab-7")
        #expect(
            !recorder.payloads().contains {
                ["token", "status"].contains($0["type"].asString ?? "")
            },
            "a browser's stream never reaches the cli")
    }

    // ── B3.1: the unknown-type refusal ───────────────────────────────────

    @Test func unknownTypeNamesTheBundleAndReprsTheVerb() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let garbage = await kernel.send([
            Item(id, .object(["type": .string("garbage")]))
        ])[0].result
        #expect(garbage["error"].asString == "mock_backend: unknown type 'garbage'")
        // `shutdown` IS UNKNOWN: python's verb map has no such entry, so the
        // lifecycle no-op swift used to answer was a reply python never sends.
        let shutdown = await kernel.send([
            Item(id, .object(["type": .string("shutdown")]))
        ])[0].result
        #expect(shutdown["error"].asString == "mock_backend: unknown type 'shutdown'")
        // A NON-STRING `type` never reaches the bundle on ANY runtime — the
        // kernel refuses the send first — so python's `{t!r}` of a number is a
        // branch nothing can observe.
        let numeric = await kernel.send([Item(id, .object(["type": .integer(5)]))])[0].result
        // C30: python's tail names a non-string by its TYPE, never its repr
        // (audit 2026-10-03, W-11).
        #expect(numeric["error"].asString == "send: payload.type must be a non-empty string, got int")
    }

    // ── B3.2: the inquiry placeholder ────────────────────────────────────

    @Test func theInquiryPlaceholderRefusalRendersPythonRepr() async throws {
        // `'...'` is the shape the refusal itself teaches, copied back verbatim.
        #expect(
            try await inquiryError(answer: #"<answer>...</answer>"#)
                == "that is the PLACEHOLDER, not an answer — `answer` came back as '...'. "
                + "Say what you actually concluded.")
        // A NON-STRING is refused whatever it says — python checks the type
        // first — and `%r` renders the number bare.
        #expect(
            try await inquiryError(answer: "<answer>42</answer>")
                == "that is the PLACEHOLDER, not an answer — `answer` came back as 42. "
                + "Say what you actually concluded.")
        // TWO STEPS, EACH ONE ONLY AT THE ENDS: `' . . '` strips to `'. .'` and
        // then to `' '` — not empty, so python ACCEPTS it. A single "strip all
        // dots and spaces" pass would have refused it.
        #expect(try await inquiryError(answer: "<answer> . . </answer>") == nil)
        // `'. x'` survives the same way: the leading dot comes off, `x` remains.
        #expect(try await inquiryError(answer: "<answer>. x</answer>") == nil)
    }

    /// Run one `inquiry` whose turn emits `answer` and return the refusal text
    /// (or nil when the answer was accepted).
    private func inquiryError(answer: String) async throws -> String? {
        let turn = "<call name=\"inquiry_answer\">\(answer)</call>"
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: [turn]) })
        let r = await kernel.send([
            Item(id, .object(["type": .string("inquiry"), "text": .string("well?")]))
        ])[0].result
        return r["error"].asString
    }

    // ── B3.3: the two `status.current` shapes ────────────────────────────

    @Test func statusCurrentHasTwoShapes() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                MockProvider(tokens: ["a", "b"], chunkDelayNanos: 80_000_000)
            })
        let running = Task {
            await kernel.send([
                Item(
                    id,
                    .object([
                        "type": .string("send"), "text": .string("hi"), "client_id": .string("c"),
                    ]))
            ])[0].result
        }
        try await waitUntil("a generation in flight") {
            let s = await kernel.send([
                Item(id, .object(["type": .string("status"), "client_id": .string("c")]))
            ])[0].result
            return s["generating"].asBool == true
        }
        let mine = await kernel.send([
            Item(id, .object(["type": .string("status"), "client_id": .string("c")]))
        ])[0].result
        // WITH a caller: the composed shape, text only for the owner.
        #expect(
            mine["current"].asObject.map { Array($0.keys) }
                == ["phase", "send_id", "started_at", "elapsed", "is_mine", "text", "text_so_far"])
        #expect(mine["current"]["is_mine"].asBool == true)

        // WITHOUT one: `_redact_text(cur)` — the ENTRY's keys, in the entry's
        // order, minus the text-bearing ones and `task_id`, plus elapsed/is_mine.
        let redacted = await kernel.send([Item(id, .object(["type": .string("status")]))])[0].result
        #expect(redacted["client_id"] == .null)
        #expect(
            redacted["current"].asObject.map { Array($0.keys) }
                == [
                    "client_id", "send_id", "queued_at", "started_at", "phase", "elapsed",
                    "is_mine",
                ])
        #expect(redacted["current"]["is_mine"].asBool == false)

        // An EMPTY client_id is falsy: echoed as-is, and it asks as nobody.
        let empty = await kernel.send([
            Item(id, .object(["type": .string("status"), "client_id": .string("")]))
        ])[0].result
        #expect(empty["client_id"].asString == "")
        #expect(empty["current"]["is_mine"].asBool == false)
        #expect(empty["current"]["text"] == .null, "no caller, no text")
        _ = await running.value
    }

    // ── B3.10: the recursion guard, BEFORE the lock ──────────────────────

    @Test func sendRefusesACycleAndOverDepth() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { MockProvider() })
        let cycle = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("hi"),
                    "_call_stack": .array([.string("peer"), .string(id.value)]),
                ]))
        ])[0].result
        #expect(cycle["error"].asString == "mock_backend: cycle detected")
        #expect(cycle["response"].asString == "")
        #expect(
            cycle["cycle"] == .array([.string("peer"), .string(id.value), .string(id.value)]))
        #expect(cycle["client_id"].asString == "cli")

        let deep = await kernel.send([
            Item(
                id,
                .object([
                    "type": .string("send"), "text": .string("hi"),
                    "client_id": .string("c"),
                    "_call_stack": .array((1...8).map { .string("a\($0)") }),
                ]))
        ])[0].result
        #expect(deep["error"].asString == "mock_backend: max call depth 8 reached")
        #expect(deep["response"].asString == "")
        #expect(deep["cycle"] == .null)
        #expect(deep["client_id"].asString == "c")
    }
}

/// Row counts sampled synchronously inside `emit`.
private final class RowCountProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var counts: [Int] = []
    func record(_ n: Int) {
        lock.lock()
        counts.append(n)
        lock.unlock()
    }
    func last() -> Int? {
        lock.lock()
        defer { lock.unlock() }
        return counts.last
    }
}
