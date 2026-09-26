// The memory-store era of ai_core: calibrated compaction trigger, repeat-read
// dedupe, fold summaries with a budget and a mechanical index, context by grant
// (`fork_from`, `request_share`), the head cache that never diverges from the
// store, and the row event (`watch`).
//
// Twin of python `ai_core/tests/test_memory_features.py`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticRamStore

/// A provider that REPORTS a prompt size, like the real endpoints do.
private final class ReportingProvider: AIProvider, @unchecked Sendable {
    let model = "reporting"
    let reported: Int
    let usage: AIUsage? = AIUsage()
    init(_ reported: Int) { self.reported = reported }
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let usage = usage
        let n = reported
        return AsyncThrowingStream { c in
            usage?.lastPromptTokens = n
            c.yield(.token("ok " + doneEnvelope))
            c.finish()
        }
    }
}

/// A provider whose pass writes a row MID-TURN (as a fold or a view would).
private final class MidTurnWriter: AIProvider, @unchecked Sendable {
    let model = "mid"
    let kernel: Kernel
    let agent: String
    init(_ kernel: Kernel, _ agent: String) {
        self.kernel = kernel
        self.agent = agent
    }
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let kernel = kernel
        let agent = agent
        return AsyncThrowingStream { c in
            Task {
                try? await saveHistory(
                    agent, kernel, [.object(["role": .string("user"), "content": .string("mid")])])
                c.yield(.token("ok " + doneEnvelope))
                c.finish()
            }
        }
    }
}

private func send(_ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil) async
    -> JSON
{
    let item = Item(AgentId(target), payload)
    if let sender {
        return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
            await k.send([item])[0].result
        }
    }
    return await k.send([item])[0].result
}

/// A summarizer that counts and scripts its answers — python `_CutThenFit` and
/// friends.
private final class ScriptedSummarizer: @unchecked Sendable {
    private let lock = NSLock()
    private var asked: [Int?] = []
    private var prompts: [String] = []
    let answer: @Sendable (Int) -> (String, Bool)
    init(_ answer: @escaping @Sendable (Int) -> (String, Bool)) { self.answer = answer }
    var calls: [Int?] { lock.withLock { asked } }
    var texts: [String] { lock.withLock { prompts } }
    var fn: AISummarizer {
        { [self] msgs, maxTokens in
            let n: Int = lock.withLock {
                asked.append(maxTokens)
                prompts.append(msgs.first?["content"].asString ?? "")
                return asked.count
            }
            return answer(n)
        }
    }
}

/// python `_rows()`: the goal, then six big assistant rows.
private func bigRows(_ n: Int = 6, size: Int = 2000) -> [JSON] {
    var rows: [JSON] = [
        .object(["role": .string("user"), "id": .integer(1), "content": .string("the goal: write report.md")])
    ]
    for i in 0..<n {
        rows.append(
            .object([
                "role": .string("assistant"), "id": .integer(Int64(10 + i)),
                "content": .string(String(repeating: "x", count: size)),
            ]))
    }
    return rows
}

private func sctx(_ s: @escaping AISummarizer) -> ProjectionCtx {
    ProjectionCtx(budget: 4000, recentN: 4, summarize: s, selfId: AgentId("ai"), kernel: nil)
}

@Suite("ai_core in the memory-store era", .serialized)
struct MemoryFeaturesTests {

    // ── the trigger, calibrated by provider-reported usage ──

    @Test func theEndpointsCountCalibratesTheEstimate() async throws {
        let (kernel, ids, _) = await bootMockKernel(
            config: baseConfig { .provider(MockProvider()) }, backendIds: ["mf_cal"],
            contextWindow: 12000, meta: ["output_reserve": .integer(0)])
        let agent = kernel.agent(ids["mf_cal"]!)!
        let backend = buildAIBackend(baseConfig { .provider(MockProvider()) })
        let msgs: [JSON] = [
            .object(["role": .string("system"), "content": .string("s")]),
            .object(["role": .string("user"), "content": .string(String(repeating: "x", count: 4000))]),
        ]
        AICalibration.set(kernel, "mf_cal", nil)
        // under the trigger by our own estimate (~1000 of a ~12k budget)
        var out = try await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "cli", messages: msgs, storeLen: 1,
            kernel: kernel)
        guard case .projected(let same) = out else {
            Issue.record("must not failfast")
            return
        }
        #expect(same == msgs && backend.getProjection("mf_cal")["fired"] == .bool(false))
        // the endpoint said the same prompt was TEN times bigger — the budget shrinks
        AICalibration.set(kernel, "mf_cal", 10.0)
        out = try await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "cli", messages: msgs, storeLen: 1,
            kernel: kernel)
        guard case .projected = out else {
            Issue.record("must not failfast")
            return
        }
        #expect(backend.getProjection("mf_cal")["fired"] == .bool(true))
        #expect(backend.getProjection("mf_cal")["calibration"] == .double(10.0))
        AICalibration.set(kernel, "mf_cal", nil)
    }

    @Test func aTurnRecordsWhatTheEndpointReported() async throws {
        let (kernel, ids, _) = await bootMockKernel(
            config: baseConfig { .provider(ReportingProvider(777)) }, backendIds: ["mf_rep"],
            contextWindow: 32000, meta: ["output_reserve": .integer(0)])
        let id = ids["mf_rep"]!
        _ = await send(kernel, id.value, .object(["type": .string("send"), "text": .string("hello")]))
        #expect(AICalibration.reportedTokens(kernel, id.value) == 777)
        #expect(AICalibration.calibration(kernel, id.value) > 0)
        let st = await send(kernel, id.value, .object(["type": .string("context_status")]))
        #expect(st["reported_prompt_tokens"].asInt == 777 && st["trigger"] == .double(0.8))
    }

    // ── the same bytes twice: one copy in view ──

    @Test func aRepeatedBodyIsAStubCitingTheFirst() {
        let body = String(repeating: "line\n", count: 200)
        let hist: [JSON] = [
            .object(["role": .string("user"), "id": .integer(1), "content": .string("[reply to 10 · notes read]\n\(body)")]),
            .object(["role": .string("assistant"), "id": .integer(2), "content": .string("again")]),
            .object(["role": .string("user"), "id": .integer(3), "content": .string("[reply to 11 · notes read]\n\(body)")]),
            .object(["role": .string("user"), "id": .integer(4), "content": .string("[reply to 12 · notes read]\nsmall")]),
        ]
        let out = dedupeRepeatBodies(hist)
        #expect(out[0] == hist[0], "the first copy stays whole")
        #expect((out[2]["content"].asString ?? "").contains("identical to the reply in row 1"))
        #expect(out[2]["id"].asInt == 3 && out[3] == hist[3])
    }

    // ── the fold summary: a budget, one retry, then refuse ──

    @Test func anOversizedSummaryIsResummarisedOnceThenRefused() async {
        let s = ScriptedSummarizer { _ in ("TITLE\n\n" + String(repeating: "word ", count: 5000), false) }
        let out = await AIFoldStrategy.summarize(sctx(s.fn), bigRows(), Array(1...5))
        #expect(out == nil, "still over budget after the retry — nothing is folded")
        #expect(s.texts.count == 2 && s.texts[1].contains("Rewrite it to fit"))
        #expect(s.texts[0].contains("AT MOST ~") && s.texts[0].contains("ERRORS:"), "fixed sections, a target")
    }

    @Test func aRetryThatFitsIsTheSummary() async throws {
        let s = ScriptedSummarizer { n in
            ("TITLE\n\n" + (n == 1 ? String(repeating: "word ", count: 5000) : "short summary"), false)
        }
        let out = try #require(await AIFoldStrategy.summarize(sctx(s.fn), bigRows(), Array(1...5)))
        #expect(out.0 == "TITLE" && out.1 == "short summary" && out.2 == false)
    }

    @Test func theMechanicalIndexKeepsFilesIdsAndTheUsersWords() {
        let rows: [JSON] = [
            .object(["role": .string("user"), "id": .integer(1), "content": .string("please check filler_3.txt for deadbeef99")]),
            .object(["role": .string("assistant"), "id": .integer(2), "content": .string("reading notes/a.md")]),
            .object(["role": .string("user"), "id": .integer(3), "content": .string("[reply to 9 · notes read]\nbody")]),
        ]
        let idx = AIFoldStrategy.mechanicalIndex(rows, [0, 1, 2])
        #expect(idx.contains("filler_3.txt") && idx.contains("notes/a.md"))
        #expect(idx.contains("deadbeef99"))
        #expect(idx.contains("user said: please check filler_3.txt for deadbeef99"))
        #expect(!idx.contains("[reply to"), "runtime lines are not the user's words")
    }

    @Test func aSummaryCutAtItsBudgetIsResummarisedOnce() async throws {
        let s = ScriptedSummarizer { n in ("TITLE\n\n" + (n <= 1 ? "partial" : "whole summary"), n <= 1) }
        let c = sctx(s.fn)
        let rows = bigRows()
        let out = try #require(await AIFoldStrategy.summarize(c, rows, Array(1...5)))
        #expect(out.1 == "whole summary" && s.calls.count == 2)
        let want = AIFoldStrategy.summaryBudget(c, rows, Array(1...5))
        let bound = Int(want + AIFoldStrategy.THINK_ALLOWANCE)  // the thought counts against max_tokens
        #expect(s.calls == [bound, bound], "the call is bounded by budget + thinking")
    }

    @Test func aSummaryCutTwiceIsRefusedNothingFolded() async {
        let s = ScriptedSummarizer { n in ("TITLE\n\n" + (n <= 2 ? "partial" : "whole summary"), n <= 2) }
        #expect(await AIFoldStrategy.summarize(sctx(s.fn), bigRows(), Array(1...5)) == nil)
    }

    // ── context by grant: fork_from, request_share ──

    private func pair() async -> (Kernel, ProviderScript, ProviderScript) {
        let a = ProviderScript([[doneEnvelope], [doneEnvelope]])
        let b = ProviderScript([[doneEnvelope], [doneEnvelope]])
        let (kernel, _, _) = await bootMockKernel(
            config: scriptedConfig(["alice": a, "bob": b]), backendIds: ["alice", "bob"])
        return (kernel, a, b)
    }

    private func shareHead(_ k: Kernel) async -> JSON {
        await send(
            k, "memory",
            .object([
                "type": .string("share"), "with": .string("bob"),
                "scope": .object(["head_until": .integer(1_000_000_000)]),
            ]), as: "alice")
    }

    @Test func forkFromReadsTheOwnersRowsUnderTheGrant() async throws {
        let (kernel, _, b) = await pair()
        try await saveHistory(
            "alice", kernel, [.object(["role": .string("user"), "content": .string("the vault code is 4711")])])
        let g = await shareHead(kernel)
        let out = await send(
            kernel, "bob",
            .object([
                "type": .string("fork_from"), "owner": .string("alice"), "grant": g["grant"],
                "message": .string("what is the code?"),
            ]))
        #expect(out["forked"].asBool == true && out["rows"].asInt == 1, "\(out.serialize())")
        let hist = try await loadHistory("bob", kernel)
        let fork = try #require(hist.first { $0["kind"].asString == "fork" })
        #expect((fork["content"].asString ?? "").contains("[shared from alice \u{b7} read-only"))
        #expect((fork["content"].asString ?? "").contains("4711"), "the owner's row, read back under the grant")
        // the kickstart: the fork's turn starts at once with the framing + message
        // (its first model call is the recall a boot owes — create_agent booted
        // it — then the turn itself)
        #expect(await b.waitForCalls(2), "the fork never started")
        let turn = b.requestText(1)
        #expect(turn.contains("you are a new agent, not alice"))
        #expect(turn.contains("what is the code?"))
        // the owner revokes — the reader's next read fails fast
        _ = await send(
            kernel, "memory", .object(["type": .string("unshare"), "grant": g["grant"]]), as: "alice")
        await #expect(throws: MemoryStoreError.self) { try await loadHistory("bob", kernel) }
    }

    @Test func forkFromRefusesAnOwnerTheGrantDoesNotName() async throws {
        let (kernel, _, _) = await pair()
        try await saveHistory(
            "alice", kernel, [.object(["role": .string("user"), "content": .string("alice's row")])])
        let g = await shareHead(kernel)
        var out = await send(
            kernel, "bob",
            .object([
                "type": .string("fork_from"), "owner": .string("mallory"), "grant": g["grant"],
                "message": .string("go"),
            ]))
        #expect(out["reason"].asString == "bad_arg" && (out["error"].asString ?? "").contains("given by alice"))
        out = await send(
            kernel, "bob",
            .object([
                "type": .string("fork_from"), "owner": .string("alice"), "grant": .integer(99999),
                "message": .string("go"),
            ]))
        #expect(out["reason"].asString == "not_found")
    }

    @Test func requestShareReachesTheOwnersModel() async throws {
        let (kernel, a, _) = await pair()
        let out = await send(
            kernel, "alice",
            .object(["type": .string("request_share"), "row": .integer(42), "why": .string("need it")]),
            as: "bob")
        #expect(out == .object(["requested": .bool(true), "owner": .string("alice"), "row": .integer(42)]))
        #expect(await a.waitForCalls(1), "the owner's model never heard it")
        let turn = a.requestText(0)
        #expect(turn.contains("[request_share from bob \u{b7} row 42] need it"))
        #expect(turn.contains("<type>share</type>"))
    }

    @Test func aSharedScopeShowsAThirdOwnersRowAsAStub() {
        let rows: [JSON] = [
            .object(["id": .integer(1), "role": .string("user"), "content": .string("mine")]),
            .object([
                "id": .integer(2), "role": .string("user"), "kind": .string("fork"),
                "owner": .string("carol"), "as_of": .integer(57), "content": .string("[fork of carol]"),
            ]),
        ]
        let text = renderShared(owner: "alice", asOf: .integer(2), rows: rows)
        #expect(text.contains("(user \u{b7} row 1) mine"))
        // carol's OWN row id — the one `request_share` to carol can name
        #expect(text.contains("[row 57 \u{b7} owner carol \u{b7} not shared with you]"))
    }

    // ── the head cache never diverges from the store ──

    @Test func aRowAnotherWriterAppendsReachesTheNextHeadRead() async throws {
        let (kernel, _, _) = await pair()
        try await saveHistory("alice", kernel, [.object(["role": .string("user"), "content": .string("first")])])
        #expect(try await loadHistory("alice", kernel).map { $0["content"] } == [.string("first")])
        let rid = kernel.mintId()
        let out = await send(
            kernel, "memory",
            .object([
                "type": .string("append"), "owner": .string("alice"),
                "rows": .array([.object(["id": .integer(rid), "role": .string("user"), "content": .string("planted")])]),
            ]))
        #expect(out["ids"] == .array([.integer(rid)]), "\(out.serialize())")
        let hist = try await loadHistory("alice", kernel)
        #expect(hist.map { $0["content"] } == [.string("first"), .string("planted")])
        // and read again: merged once, not twice
        #expect(try await loadHistory("alice", kernel).count == 2)
    }

    @Test func aNoteTheAgentAppendsItselfIsMemoryNotATurn() async throws {
        let (kernel, _, _) = await pair()
        try await saveHistory("alice", kernel, [.object(["role": .string("user"), "content": .string("hi")])])
        _ = try await loadHistory("alice", kernel)  // the cache is warm
        let out = await send(
            kernel, "memory",
            .object([
                "type": .string("append"),
                "rows": .array([.object(["kind": .string("note"), "content": .string("vault code 4711")])]),
            ]), as: "alice")
        #expect(out["appended"].asInt == 1, "\(out.serialize())")
        let hist = try await loadHistory("alice", kernel)
        #expect(hist.map { $0["content"] } == [.string("hi")])
        let behind = try await loadHistory("alice", kernel, behind: true)
        #expect(behind.allSatisfy { $0["role"] != .null })
    }

    @Test func aTurnsRowsAreMintedWhenAppendedSoIdOrderIsAppendOrder() async throws {
        let box = KernelBox()
        var cfg = baseConfig { .refused(.object(["error": .string("unset")])) }
        cfg.makeProvider = { agent, _, kernel in .provider(MidTurnWriter(kernel, agent.id.value)) }
        let (kernel, ids, _) = await bootMockKernel(
            config: cfg, backendIds: ["mf_mint"], contextWindow: 32000,
            meta: ["output_reserve": .integer(0)])
        box.kernel = kernel
        let id = ids["mf_mint"]!
        _ = await send(kernel, id.value, .object(["type": .string("send"), "text": .string("hello")]))
        let hist = try await loadHistory(id.value, kernel)
        let rowIds = hist.compactMap { $0["id"].asInt }
        #expect(rowIds == rowIds.sorted(), "append order diverged from id order: \(rowIds)")
        #expect(Array(hist.map { $0["content"] }.prefix(2)) == [.string("mid"), .string("hello")])
    }

    // ── the row event: an agent announces its own rows to its watchers ──

    @Test func watchAnnouncesDigestsAndBodiesOnlyUnderALiveGrant() async throws {
        let (kernel, _, _) = await pair()
        let inbox = kernel.ensureInbox(AgentId("bob"))
        var w = await send(
            kernel, "alice", .object(["type": .string("watch"), "body": .bool(true)]), as: "bob")
        #expect(w == .object(["watching": .bool(true), "watcher": .string("bob"), "body": .bool(true)]))
        try await saveHistory("alice", kernel, [.object(["role": .string("user"), "content": .string("secret one")])])
        var events: [JSON] = []
        while let item = inbox.tryTake() { events.append(item.payload) }
        var row = try #require(events.last { $0["type"].asString == "row" })
        #expect(row["source"].asString == "alice" && row["row"]["role"].asString == "user")
        #expect(row["row"].asObject?["content"] == nil, "no grant, no body")
        // alice shares LIVE with bob — its model's share refreshes the grant cache
        _ = await send(
            kernel, "memory",
            .object([
                "type": .string("share"), "with": .string("bob"),
                "scope": .object(["head_until": .integer(1_000_000_000)]), "mode": .string("live"),
            ]), as: "alice")
        forgetGrants("alice", kernel)
        try await myGrants("alice", kernel)
        try await saveHistory("alice", kernel, [.object(["role": .string("user"), "content": .string("secret two")])])
        events = []
        while let item = inbox.tryTake() { events.append(item.payload) }
        row = try #require(events.last { $0["type"].asString == "row" })
        #expect(row["row"]["content"].asString == "secret two", "\(row.serialize())")
        w = await send(kernel, "alice", .object(["type": .string("unwatch")]), as: "bob")
        #expect(w["watching"] == .bool(false))
        // an agent watches for ITSELF
        let other = await send(
            kernel, "alice", .object(["type": .string("watch"), "watcher": .string("carol")]),
            as: "bob")
        #expect(other["reason"].asString == "unauthorized")
    }
}

/// Holds a kernel for a provider closure built before the kernel exists.
private final class KernelBox: @unchecked Sendable {
    var kernel: Kernel?
}
