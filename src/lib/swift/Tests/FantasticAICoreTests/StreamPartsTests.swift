// STREAMING PARTS (owner, 2026-10-04, `tmp/streaming_parts.md`): a model's
// output streams to its addressees as parts — animation, never action; a
// receiver can stop it; a cut pass leaves a marker on top of the context, never a
// half answer.
//
// Case for case the canonical python suite
// `bundled_agents/ai/ai_core/tests/test_stream_parts.py`.

import FantasticJSON
@testable import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

private let CALL =
    "<call name=\"send\">\n<target_id>notes</target_id>\n<type>reflect</type>\n"
    + "<bytes>a </call> inside</bytes>\n</call>"
private let DONE = "<call name=\"done\"></call>"

/// `text[i : i + n]` for every i — code points, as python slices.
private func chunks(_ text: String, _ n: Int) -> [String] {
    let s = Array(text.unicodeScalars)
    return stride(from: 0, to: s.count, by: n).map { i in
        var v = String.UnicodeScalarView()
        v.append(contentsOf: s[i..<min(i + n, s.count)])
        return String(v)
    }
}

// ─── the splitter ────────────────────────────────────────────────────────────

private func run(_ text: String, _ n: Int) -> ([PartEvent], PartSplitter) {
    let sp = PartSplitter(source: "ai")
    var ev: [PartEvent] = []
    for c in chunks(text, n) { ev += sp.feed(c) }
    return (ev + sp.finish(), sp)
}

/// `(e[0], e[1]["kind"])`.
private func shape(_ e: PartEvent) -> String {
    switch e {
    case .start(let p): return "start \(p.kind)"
    case .delta(let p, _): return "delta \(p.kind)"
    case .end(let p, _): return "end \(p.kind)"
    case .abort(let p, _): return "abort \(p.kind)"
    }
}

private func isDelta(_ e: PartEvent) -> Bool {
    if case .delta = e { return true }
    return false
}

// ─── through an AI agent ─────────────────────────────────────────────────────

/// A provider that streams each script entry 4 chars at a time, `step` apart —
/// python's `_Slow`. The step is read as each pass's provider is built, so a
/// test can speed the next pass up.
private final class Slow: @unchecked Sendable {
    let script: ProviderScript
    private let lock = NSLock()
    private var _step: UInt64

    init(_ scripts: [String], stepNanos: UInt64 = 0) {
        self.script = ProviderScript(scripts.map { chunks($0, 4) })
        self._step = stepNanos
    }

    var stepNanos: UInt64 {
        get { lock.withLock { _step } }
        set { lock.withLock { _step = newValue } }
    }
}

private func slowKernel(_ aid: String, _ slow: Slow) async -> (Kernel, AgentId) {
    var cfg = baseConfig { throw PyError("RuntimeError", "unreachable") }
    cfg.makeProvider = { _, _, _ in
        MockProvider(script: slow.script, chunkDelayNanos: slow.stepNanos)
    }
    let (k, ids, _) = await bootMockKernel(config: cfg, backendIds: [aid])
    return (k, ids[aid]!)
}

private func ramStore(_ k: Kernel, _ id: String) async {
    _ = await k.send([
        Item(
            AgentId("kernel_state"),
            .object([
                "type": .string("create_agent"),
                "parent_id": .string("kernel_state"),
                "handler_module": .string("ram_store.tools"),
                "id": .string(id),
            ]))
    ])[0].result
}

/// A registered inbox `w` mirroring `src`, drained into a sink.
private func watcher(_ k: Kernel, _ src: String, _ w: String) -> (EventSink, Task<Void, Never>) {
    let sink = EventSink()
    let inbox = k.ensureInbox(AgentId(w))
    let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
    k.watch(src: AgentId(src), watcher: AgentId(w))
    return (sink, pump)
}

private func partEvents(_ sink: EventSink) -> [JSON] {
    sink.snapshot().filter { ($0["type"].asString ?? "").hasPrefix("part_") }
}

private func sendText(_ k: Kernel, _ id: AgentId, _ text: String, client: String? = nil) async
    -> Outcome
{
    var p: OrderedDictionary<String, JSON> = ["type": .string("send"), "text": .string(text)]
    if let client { p["client_id"] = .string(client) }
    return await k.send([Item(id, .object(p))])[0]
}

@Suite("streaming parts")
struct StreamPartsTests {

    @Test func partsAreTheSameAtEveryChunkSize() {
        let text = "hi <think>plan</think>ok \(CALL) after \(DONE)"
        for n in [1, 2, 5, text.unicodeScalars.count] {
            let (ev, sp) = run(text, n)
            #expect(
                ev.filter { !isDelta($0) }.map(shape) == [
                    "start text", "end text",
                    "start thinking", "end thinking",
                    "start text", "end text",
                    "start call", "end call",
                    "start text", "end text",
                    "start call", "end call",
                ], "chunk size \(n)")
            let call = sp.parts[3]
            #expect(call.name == "send" && call.target == "notes")
            let body = ev.compactMap { e -> String? in
                if case .delta(let p, let t) = e, p === call { return t }
                return nil
            }.joined()
            #expect(body.contains("a </call> inside"))  // verbatim bytes never close the call
        }
    }

    @Test func anOpenCallWhenThePassEndsAborts() {
        let (ev, _) = run(#"<call name="send"><target_id>x</target_id><type>wr"#, 3)
        #expect(ev.map(shape).contains("abort call"))
    }

    @Test func aCallInsideAThoughtIsACallPart() {
        let (ev, _) = run("<think>let me \(DONE) then</think>", 4)
        #expect(
            ev.filter { !isDelta($0) }.map(shape) == [
                "start thinking", "end thinking",
                "start call", "end call",
                "start thinking", "end thinking",
            ])
    }

    @Test func theCutMarkerIsAMarkerNotAHalfAnswer() {
        let m = cutMarker(
            #"said this <think>hidden</think><call name="send"><target_id>x</target_id>"#,
            "model stopped generating",
            [StreamPart(streamId: "s", kind: "call", name: "send", target: "x")])
        #expect(
            m.components(separatedBy: "\n") == [
                "[model stopped generating]",
                "said this",
                "[a send call to x was not sent]",
            ])
        #expect(cutMarker("<think>hmm", "stopped by rx", []) == "[stopped by rx while thinking]")
    }

    @Test func aCallPartStreamsToItsAddressee() async throws {
        let slow = Slow([CALL, DONE])
        let (k, ai) = await slowKernel("sp_ai", slow)
        await ramStore(k, "notes")
        wire(k, "sp_ai", "notes")
        let (sink, pump) = watcher(k, "notes", "w_notes")
        defer { pump.cancel() }
        _ = await sendText(k, ai, "go")
        #expect(await slow.script.waitForCalls(2))
        try await waitUntil("part_end") {
            partEvents(sink).contains { $0["type"].asString == "part_end" }
        }
        let evs = partEvents(sink)
        let types = evs.map { $0["type"].asString ?? "" }
        #expect(types.first == "part_start" && types.last == "part_end", "\(types)")
        #expect(evs.first?["name"].asString == "send" && evs.first?["target"].asString == "notes")
        #expect(Set(evs.map { $0["stream_id"].asString ?? "" }).count == 1)
        let body = evs.filter { $0["type"].asString == "part_delta" }
            .map { $0["text"].asString ?? "" }.joined()
        #expect(body.contains("a </call> inside"))
    }

    /// The model stops generating mid-call: the call is NOT dispatched, and the
    /// conversation's newest assistant row is the cut marker, which the next turn
    /// sees.
    @Test func aModelThatStopsWithABlockOpenLeavesTheMarkerOnTop() async throws {
        let openCall = #"<call name="send"><target_id>notes</target_id><type>refl"#
        let slow = Slow([openCall, DONE])
        let (k, ai) = await slowKernel("sp_cut", slow)
        await ramStore(k, "notes")
        wire(k, "sp_cut", "notes")
        let (sink, pump) = watcher(k, "notes", "w_cut")
        defer { pump.cancel() }
        _ = await sendText(k, ai, "go")
        #expect(await slow.script.waitForCalls(2))
        let nxt = try #require(slow.script.request(1))
        let marker = nxt.filter { $0["role"].asString == "assistant" }.last?["content"].asString
        #expect(
            marker?.components(separatedBy: "\n") == [
                "[model stopped generating]",
                "[a send call to notes was not sent]",
            ], "\(marker ?? "nil")")
        try await waitUntil("part_abort") {
            partEvents(sink).contains { $0["type"].asString == "part_abort" }
        }
        let seen = partEvents(sink).map { $0["type"].asString ?? "" }
        #expect(seen.first == "part_start" && seen.last == "part_abort", "\(seen)")
        // nothing was dispatched to notes: no reply came back for it
        let last = nxt.last?["content"].asString ?? ""
        #expect(!last.contains("reflect"), "\(last)")
    }

    @Test func aReceiverStopsTheStreamAndTheTurnKeepsAMarker() async throws {
        let long = String(repeating: "partial words ", count: 200)
        let slow = Slow([long, DONE], stepNanos: 10_000_000)
        let (k, ai) = await slowKernel("sp_stop", slow)
        await ramStore(k, "rx")
        wire(k, "rx", "sp_stop")
        let (sink, pump) = watcher(k, "sp_stop", "w_stop")
        defer { pump.cancel() }
        // the send dispatch lasts as long as the stream: run it, stop it mid-way
        let turn = Task { await sendText(k, ai, "go", client: "tab") }
        #expect(await slow.script.waitForCalls(1))
        try await Task.sleep(nanoseconds: 200_000_000)  // some text has streamed
        let out = await KernelTaskLocals.$sender.withValue(AgentId("rx")) {
            await k.send([Item(ai, .object(["type": .string("stop")]))])[0]
        }
        #expect(out.result == .object(["stopped": .bool(true)]), "\(out.result)")
        #expect(await turn.value.status == .cancelled)
        try await waitUntil("part_abort") {
            partEvents(sink).contains { $0["type"].asString == "part_abort" }
        }
        let seen = partEvents(sink).map { $0["type"].asString ?? "" }
        #expect(seen.last == "part_abort", "\(seen)")
        // the next turn sees what it was asked and the marker — never the half answer
        slow.stepNanos = 0
        _ = await sendText(k, ai, "again")
        #expect(await slow.script.waitForCalls(2))
        let nxt = try #require(slow.script.request(1))
        #expect(
            nxt.contains {
                $0["role"].asString == "user" && ($0["content"].asString ?? "").contains("go")
            }, "\(nxt)")
        let marker = nxt.filter { $0["role"].asString == "assistant" }.last?["content"].asString ?? ""
        #expect(marker.hasPrefix("[stopped by rx]\npartial words"), "\(marker.prefix(80))")
    }

    @Test func stopWithNothingInFlightSaysSo() async {
        let (k, ai) = await slowKernel("sp_idle", Slow([]))
        let out = await k.send([Item(ai, .object(["type": .string("stop")]))])[0]
        #expect(out.result == .object(["stopped": .bool(false)]))
    }

    // ─── the match: draft ↔ final message (owner, 2026-10-04) ────────────────

    /// AI → AI: the consumer watches `part_*` on its inbox, then receives the
    /// message; the message's header names the same stream id, `part_start` names
    /// the message the producer answers (`re`, the draft's anchor), `part_end`
    /// says ok.
    @Test func aMessageCarriesTheStreamItWasPreviewedAs() async throws {
        let msg =
            "<call name=\"send\">\n<target_id>sp_cons</target_id>\n<type>send</type>\n"
            + "<text>hello consumer</text>\n</call>"
        let prod = ProviderScript([chunks(msg, 4), chunks(DONE, 4)])
        let cons = ProviderScript([chunks(DONE, 4)])
        let (k, ids, _) = await bootMockKernel(
            config: scriptedConfig(["sp_prod": prod, "sp_cons": cons]),
            backendIds: ["sp_prod", "sp_cons"])
        wire(k, "sp_prod", "sp_cons")
        let (sink, pump) = watcher(k, "sp_cons", "w_cons")
        defer { pump.cancel() }
        _ = await sendText(k, ids["sp_prod"]!, "go")
        #expect(await cons.waitForCalls(1))
        try await waitUntil("part_end") {
            partEvents(sink).contains { $0["type"].asString == "part_end" }
        }
        let parts = partEvents(sink)
        let start = try #require(parts.first { $0["type"].asString == "part_start" })
        let end = try #require(parts.first { $0["type"].asString == "part_end" })
        #expect(start["target"].asString == "sp_cons" && start["re"].asInt != nil, "\(start)")
        #expect(end["ok"] == .bool(true) && end["stream_id"] == start["stream_id"], "\(end)")
        let delivered = try #require(cons.request(0)?.last?["content"].asString)
        let sid = start["stream_id"].asString ?? ""
        #expect(
            delivered.components(separatedBy: "\n")[0].contains("stream \(sid)"), "\(delivered)")
        #expect(delivered.contains("hello consumer"))
        #expect(prod.calls >= 1)
    }

    @Test func aMalformedClosedCallEndsNotOk() throws {
        let (ev, _) = run(
            #"<call name="send"><target_id>x</target_id><type>a<path>p</path></call>"#, 3)
        let info = try #require(
            ev.lazy.compactMap { e -> OrderedDictionary<String, JSON>? in
                if case .end(let p, let i) = e, p.kind == "call" { return i }
                return nil
            }.first)
        #expect(info == ["ok": .bool(false)])
    }

    /// The in-flight entry holds the live splitter: a `status` snapshot never
    /// carries it (it is bookkeeping, and not JSON).
    @Test func statusDuringAStreamIsJson() async throws {
        let slow = Slow([String(repeating: "words ", count: 300), DONE], stepNanos: 10_000_000)
        let (k, ai) = await slowKernel("sp_st", slow)
        let turn = Task { await sendText(k, ai, "go") }
        #expect(await slow.script.waitForCalls(1))
        try await Task.sleep(nanoseconds: 100_000_000)
        let out = await k.send([Item(ai, .object(["type": .string("status")]))])[0]
        // `json.dumps(out["result"])`: the snapshot serializes as JSON.
        _ = try JSON.parse(out.result.serialize())
        #expect(out.result["generating"] == .bool(true), "\(out.result)")
        #expect((out.result["current"].asObject ?? [:])["parts"] == nil, "\(out.result)")
        _ = await turn.value
    }

    /// `rows` is the agent's in-memory head, as held — no store read, no
    /// transform: nothing loaded → loaded:false; after a turn → its rows, paged by
    /// row id.
    @Test func rowsIsAViewOfWhatTheAgentHolds() async throws {
        let slow = Slow(["hello there", DONE])
        let (k, ai) = await slowKernel("sp_rows", slow)
        await ramStore(k, "viewer")
        wire(k, "viewer", "sp_rows", ["rows"])
        func rows(_ as: String, _ p: OrderedDictionary<String, JSON> = [:]) async -> Outcome {
            var payload = p
            payload["type"] = .string("rows")
            return await KernelTaskLocals.$sender.withValue(AgentId(`as`)) {
                await k.send([Item(ai, .object(payload))])[0]
            }
        }
        let cold = await rows("viewer").result
        #expect(
            cold
                == .object([
                    "rows": .array([]), "next": .null, "total": .integer(0),
                    "loaded": .bool(false),
                ]), "\(cold)")
        _ = await sendText(k, ai, "hi")
        #expect(await slow.script.waitForCalls(2))
        let page = await rows("viewer", ["limit": .integer(1)]).result
        let rest = await rows("viewer", ["after": page["next"]]).result
        let total = page["total"].asInt ?? 0
        #expect(page["loaded"] == .bool(true) && total >= 2, "\(page)")
        let first = page["rows"][0]
        #expect(
            first["role"].asString == "user"
                && (first["content"].asString ?? "").contains("hi"), "\(page)")
        #expect(
            (page["rows"].asArray?.count ?? 0) + (rest["rows"].asArray?.count ?? 0) == total
                && rest["next"] == .null, "\(page) \(rest)")
        // an ungranted caller is refused by the gate
        await ramStore(k, "stranger")
        let out = await rows("stranger")
        #expect(out.result["reason"].asString == "unauthorized", "\(out)")
    }
}
