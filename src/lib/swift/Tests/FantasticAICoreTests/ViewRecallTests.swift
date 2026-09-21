// The view array is the agent's state; `context_recall` rebuilds it; every
// self-read verb answers under the inline cap. Twins of python
// `ai_core/tests/test_view_recall.py`, case for case.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// One agent id per suite: the stripped set and the incarnation table are
/// process-global, keyed by agent, and suites run in parallel.
private let AI = "ai_v"

private let BIG = FOLD_BIG

/// A booted kernel with the `ai_v` backend, its fixture, and a backend instance
/// to drive `assemble`/`contextRecall`/`projectContext` directly — python drives
/// `core._assemble` and `_context_recall` as functions the same way.
private func boot(
    config: AIBackendConfig = baseConfig { .provider(MockProvider()) },
    contextWindow: Int64 = 12000,
    meta: OrderedDictionary<String, JSON> = ["output_reserve": .integer(0)]
) async -> (Kernel, Agent, FoldFixture, AIBackend) {
    let (kernel, ids, _) = await bootMockKernel(
        config: config, backendIds: [AI], contextWindow: contextWindow, meta: meta)
    let agent = kernel.agent(ids[AI]!)!
    return (kernel, agent, FoldFixture(kernel: kernel, agent: AI), buildAIBackend(config))
}

private func dry() -> JSON { .object(["dry": .bool(true)]) }

/// A model-call counter a `@Sendable` provider closure may bump.
private final class AskCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func bump() {
        lock.lock()
        n += 1
        lock.unlock()
    }
    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return n
    }
}

@Suite("the view array is the state", .serialized)
struct ViewRecallTests {
    init() { AIStripStrategy.forget(AI) }

    // ── the cap invariant ────────────────────────────────────────────────

    @Test func fitReplyNeverExceedsTheCapAndSaysSo() {
        let items: [JSON] = (0..<200).map {
            .object(["id": .string("r:\($0)"), "text": .string(String(repeating: "y", count: 300))])
        }
        let (kept, cut) = AIContext.fitReply(items, capTokens: 500)
        #expect(cut && 0 < kept.count && kept.count < 200)
        #expect(
            AIContext.estimateOne(
                .object(["role": .string("user"), "content": .string(JSON.array(kept).serialize())])
            )
                <= 500)
        let (kept2, cut2) = AIContext.fitReply(Array(items.prefix(2)), capTokens: 10_000)
        #expect(kept2 == Array(items.prefix(2)) && !cut2)
    }

    @Test func grepAnswersUnderTheInlineCapForABroadQuery() async throws {
        let (kernel, agent, f, _) = await boot()
        // every row matches, on distinct lines (identical lines are reported once)
        var rows: [JSON] = (0..<40).map { i in
            f.row("user", (0..<80).map { "marker \(i).\($0)" }.joined(separator: "\n"), "u:\(i)")
        }
        var pids: [Int64] = []
        for i in 0..<10 {
            let pid = kernel.rows.mintId()
            pids.append(pid)
            try kernel.rows.bodyPut(
                agent.id, pid: pid,
                text: (0..<500).map { "marker line \(i).\($0)" }.joined(separator: "\n"))
        }
        rows.append(
            f.row(
                "user", pids.map { "[reply to \($0) · n r]\nx" }.joined(separator: "\n\n"), "u:tags"
            ))
        f.seed(rows)
        let r = await kernel.send([
            Item(agent.id, .object(["type": .string("grep"), "query": .string("marker")]))
        ])[0].result
        let cap = AIContext.inlineCap(agent)
        #expect(
            AIContext.estimateOne(
                .object(["role": .string("user"), "content": .string(r.serialize())])) <= cap + 64)
        #expect(r["truncated"].asBool == true && r["next"]["id"] != .null)
    }

    @Test func historyIsDigestResolutionAndPaged() async throws {
        let (kernel, agent, f, _) = await boot()
        f.seed((0..<30).map { f.row("user", BIG, "u:\($0)") })
        let r = await kernel.send([Item(agent.id, .object(["type": .string("history")]))])[0].result
        let rows = r["rows"].asArray ?? []
        #expect(r["total"].asInt == 30 && !rows.isEmpty)
        #expect(rows.allSatisfy { $0.asObject?["head"] != nil && $0.asObject?["bytes"] != nil })
        #expect(
            rows.allSatisfy { ($0["head"].asString ?? "").count <= 120 }, "no bodies in a digest")
        if r["next"] != .null {
            let r2 = await kernel.send([
                Item(agent.id, .object(["type": .string("history"), "after": r["next"]]))
            ])[0].result
            #expect((r2["rows"].asArray ?? []).first?["id"] != rows.first?["id"])
        }
    }

    // ── the view array ───────────────────────────────────────────────────

    @Test func viewRoundTripsAndAssembleBuildsFromIt() async throws {
        let (kernel, agent, f, backend) = await boot()
        let a = f.chain("a", 3)
        f.seed(a)
        let ids: [JSON] = [
            .object(["id": a[0]["id"], "res": .string("full")]),
            .object(["id": a[2]["id"], "res": .string("stub")]),
            .object(["id": a[4]["id"], "res": .string("full")]),
        ]
        saveView(agent.id, kernel: kernel, ids: ids, budget: 1000, why: "test")
        let v = try #require(loadView(agent.id, kernel: kernel))
        #expect(v["ids"] == .array(ids) && v["why"]?.asString == "test")

        let (msgs, n) = await backend.assemble(
            agent: agent, userText: "live", systemPrompt: "sys", kernel: kernel)
        #expect(n == a.count)
        let body = Array(msgs[1..<(msgs.count - 1)])
        // the array, in order, then every row appended after its newest member
        #expect(body.map { $0["id"] } == ids.map { $0["id"] } + a[5...].map { $0["id"] })
        let stubbed = body[1]["content"].asString ?? ""
        #expect(stubbed.contains("(stripped:") && !stubbed.contains(BIG))
        #expect(backend.getViewSource(AI) == "array")
    }

    @Test func aStaleArrayFallsBackToTheStoreAndSaysSo() async throws {
        let (kernel, agent, f, backend) = await boot()
        let a = f.chain("a", 1, big: false)
        f.seed(a)
        saveView(
            agent.id, kernel: kernel,
            ids: [.object(["id": .string("gone:0000"), "res": .string("full")])], budget: 1,
            why: "x")
        let (msgs, _) = await backend.assemble(
            agent: agent, userText: "live", systemPrompt: "sys", kernel: kernel)
        #expect(msgs.count == 1 + a.count + 1)
        #expect(backend.getViewSource(AI)?.hasPrefix("store") == true)
    }

    @Test func resOfClassifiesRows() {
        let f = FoldFixture(agent: AI)
        #expect(resOf(f.fold([f.id("x:1"), f.id("x:2")], "t", "b", false)) == "fold")
        #expect(
            resOf(
                .object([
                    "role": .string("user"),
                    "content": .string(
                        "[reply to n:1 · n r]\n(stripped: 10 bytes — recall by pid)"),
                ])) == "stub")
        #expect(
            resOf(
                .object(["role": .string("user"), "content": .string("[reply to n:1 · n r]\nbody")])
            ) == "full")
    }

    // ── context_recall ───────────────────────────────────────────────────

    @Test func recallMechanicalFitIsUnderBudgetAndPersistsAViewAndARow() async throws {
        let (kernel, agent, f, backend) = await boot()
        let a = f.chain("a", 20)
        f.seed([f.row("user", "GOAL", "user:goal")] + a)
        let r = try await backend.contextRecall(
            agent: agent, payload: dry(), kernel: kernel, provider: nil)
        #expect(r["degraded"].asBool == true && (r["kept"].asInt ?? 0) >= 1)
        let v = try #require(loadView(agent.id, kernel: kernel))
        #expect(
            v["ids"]?.asArray?.first?["id"].asInt == f.id("user:goal"), "the goal turn is always in"
        )
        let rows = backend.loadHistory(agent: agent, kernel: kernel)
        let rec = rows.filter { $0["kind"].asString == "recall" }
        #expect(rec.count == 1 && (rec.first?["content"].asString ?? "").contains("[recall "))
        // a second recall is deterministic on the same store (modulo its own new row)
        let r2 = try await backend.contextRecall(
            agent: agent, payload: dry(), kernel: kernel, provider: nil)
        #expect((r2["kept"].asInt ?? 0) >= (r["kept"].asInt ?? 0) - 1)
    }

    @Test func recallAsksTheModelAndExpandsOnlyWhatFits() async throws {
        let (kernel, agent, f, backend) = await boot()
        let a = f.chain("a", 12)
        f.seed([f.row("user", "GOAL", "user:goal")] + a)
        let replyIds = Set(
            a.filter { ($0["content"].asString ?? "").hasPrefix("[reply to ") }.compactMap(rowId))
        // python's `_P`: read the head ids off the prompt, ask for the last two
        // replies in full plus a ghost id.
        let provider = ComputedProvider { messages in
            let headIds: [Int64] = (messages[0]["content"].asString ?? "").split(separator: "\n")
                .compactMap { ln in
                    guard ln.contains(" · ") else { return nil }
                    return Int64(ln.components(separatedBy: " · ")[0])
                }
            let want = headIds.filter { replyIds.contains($0) }.suffix(2) + [999_999_999]
            return JSON.object([
                "expand": .array(want.map(JSON.integer)),
                "why": .string("the last two replies matter"),
            ]).serialize()
        }
        let r = try await backend.contextRecall(
            agent: agent, payload: .object([:]), kernel: kernel, provider: provider)
        #expect(r["degraded"].asBool == false && r["why"].asString == "the last two replies matter")
        let v = try #require(loadView(agent.id, kernel: kernel))
        let fullReplies = (v["ids"]?.asArray ?? []).filter {
            $0["res"].asString == "full" && $0["id"].asInt.map { replyIds.contains($0) } ?? false
        }
        #expect(
            1 <= fullReplies.count && fullReplies.count <= 2,
            "expansions are the model's, capped by budget; a ghost id is ignored")
    }

    @Test func recallClearsRamScratchButNeverTheStore() async throws {
        let (kernel, agent, f, backend) = await boot()
        f.seed([f.row("user", "GOAL", "user:goal")] + f.chain("a", 8))
        backend.setProjection(AI, .object(["fired": .bool(true)]))
        backend.setMark(AI, index: 3, client: "cli")
        AIStripStrategy.set(AI, ["n:1"])
        let before = backend.loadHistory(agent: agent, kernel: kernel).count
        _ = try await backend.contextRecall(
            agent: agent, payload: dry(), kernel: kernel, provider: nil)
        #expect(backend.getProjection(AI) == .null)
        #expect(markOf(backend, AI) == nil)
        #expect(!AIStripStrategy.has(AI))
        #expect(
            backend.loadHistory(agent: agent, kernel: kernel).count == before + 1,
            "one recall row appended, nothing removed")
    }

    @Test func bootRecallsOnlyWhenThereIsAStore() async throws {
        // python's `_cfg()` builds no provider: the recall stays at the mechanical fit.
        let (kernel, agent, f, _) = await boot(
            config: baseConfig { .refused(.object(["error": .string("no provider")])) })
        let bootVerb: JSON = .object(["type": .string("boot")])
        #expect(await kernel.send([Item(agent.id, bootVerb)])[0].result == .null)
        f.seed([f.row("user", "GOAL", "user:goal")] + f.chain("a", 8))
        let r = await kernel.send([Item(agent.id, bootVerb)])[0].result
        #expect((r["kept"].asInt ?? 0) >= 1 && loadView(agent.id, kernel: kernel) != nil)
    }

    @Test func aFoldBatchMakesRecallDueOnTheNextTurn() async throws {
        let (kernel, agent, f, backend) = await boot(
            contextWindow: 2500,
            meta: ["output_reserve": .integer(0), "context_strategy": .string("fold")])
        let body =
            [plainTurn("system", "sys"), f.row("user", "GOAL", "user:goal")] + f.chain("a", 12)
            + [f.row("user", "now?", nil)]
        let out = await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "cli", messages: body,
            storeLen: body.count - 1, kernel: kernel)
        guard case .projected = out else {
            Issue.record("the projection must fire, not failfast")
            return
        }
        #expect(backend.isRecallDue(AI))
        #expect(loadView(agent.id, kernel: kernel) != nil, "every projection writes the view")
    }

    /// THE LIVE PATH, end to end: a send that folds a chain marks a recall due,
    /// and the NEXT send runs it before assembling — python `_recall_due` is
    /// consumed at the top of `_send`, and the decision lands as a `kind:recall`
    /// row. Measured 2026-09-13 (recall_after_fold:swift): 6 folds, 0 recalls.
    @Test func anOwedRecallRunsAtTheStartOfTheNextLiveTurn() async throws {
        let (kernel, agent, f, _) = await boot(
            config: baseConfig { .provider(MockProvider(tokens: ["ok ", doneEnvelope])) },
            // Room for the real system block (~5.1k tok) plus a head, not the chain.
            contextWindow: 8000,
            meta: ["output_reserve": .integer(0), "context_strategy": .string("fold")])
        f.seed([f.row("user", "GOAL", "user:goal")] + f.chain("a", 12))
        let r1 = await kernel.send([
            Item(agent.id, .object(["type": .string("send"), "text": .string("now?")]))
        ])[0].result
        let cs = await kernel.send([Item(agent.id, .object(["type": .string("context_status")]))])[
            0
        ].result
        let folds = f.foldsOf().count
        #expect(
            folds >= 3,
            "the first send folds the chain: \(folds) send=\(r1.serialize().prefix(300)) status=\(cs.serialize().prefix(400))"
        )
        _ = await kernel.send([
            Item(agent.id, .object(["type": .string("send"), "text": .string("again")]))
        ])[0].result
        let kinds = f.rowsOf().map { $0["kind"].asString ?? $0["role"].asString ?? "?" }
        #expect(
            kinds.contains("recall"), "the owed recall runs at the start of the next turn: \(kinds)"
        )
    }

    /// Measured 2026-09-06 (python): the rebooted agent was woken by answers to
    /// calls its previous incarnation made, and finished THAT work instead of the
    /// new instruction.
    @Test func aReplyInFlightAcrossARebootIsRecordedNotActedOn() async throws {
        // Its own agent id: the incarnation table is process-global and other
        // suites boot `mock` agents while this runs.
        let id = "reboot_ai"
        // Pass 1 answers boot's recall prompt (no JSON — the mechanical fit stands);
        // pass 2 is the ONE turn a live reply may run. A third would be a runaway.
        let script = ProviderScript([[doneEnvelope], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig([id: script]), backendIds: [id])
        let agentId = ids[id]!
        let f = FoldFixture(kernel: kernel, agent: id)
        f.seed([f.row("user", "GOAL", "user:goal")])
        AIIncarnation.set(kernel, id, 1)
        // reboot → incarnation 2
        _ = await kernel.send([Item(agentId, .object(["type": .string("boot")]))])[0].result
        let afterBoot = script.calls
        await deliverTurn(
            kernel: kernel, target: id, text: "[reply to n:1 · n r]\nlate", clientId: "c",
            systemPrompt: nil, born: 1)
        #expect(script.calls == afterBoot, "no turn ran for the late reply")
        let rows = conversationRows(kernel: kernel, id: agentId)
        #expect(
            rows.last?["kind"].asString == "late"
                && (rows.last?["content"].asString ?? "").contains("late"))
        await deliverTurn(
            kernel: kernel, target: id, text: "[reply to n:2 · n r]\nfresh", clientId: "c",
            systemPrompt: nil, born: AIIncarnation.of(kernel, id))
        #expect(script.calls == afterBoot + 1, "a reply from THIS incarnation runs a turn")
    }

    /// The array is the head as of its write; the log keeps growing. Measured
    /// (python): served the array alone, the model restarted its survey every turn.
    @Test func rowsAppendedAfterTheArrayRideBehindIt() async throws {
        let (kernel, agent, f, backend) = await boot()
        let a = f.chain("a", 4, big: false)
        f.seed(a)
        saveView(
            agent.id, kernel: kernel,
            ids: [
                .object(["id": a[0]["id"], "res": .string("full")]),
                .object(["id": a[2]["id"], "res": .string("full")]),
            ], budget: 1000, why: "t")
        let later = [
            f.row("assistant", "I just read filler_9", "ai:new1", parentId: rowId(a[a.count - 1])),
            f.row("user", "[reply to notes:new · notes read]\nfiller_9 body", "ai:new2", "ai:new1"),
        ]
        f.seed(later)
        let (msgs, _) = await backend.assemble(
            agent: agent, userText: "live", systemPrompt: "sys", kernel: kernel)
        let ids = msgs[1..<(msgs.count - 1)].map { $0["id"] }
        #expect(Array(ids.prefix(2)) == [a[0]["id"], a[2]["id"]], "the array first")
        #expect(
            Array(ids.suffix(2)) == [.integer(f.id("ai:new1")), .integer(f.id("ai:new2"))],
            "then everything appended since")
        #expect(backend.getViewSource(AI) == "array")
    }

    @Test func theRecallRowNamesTheNewestRoot() async throws {
        let (kernel, agent, f, backend) = await boot()
        f.seed(
            [f.row("user", "TASK 0: first", "user:t0")] + f.chain("a", 2)
                + [f.row("user", "TASK 1: latest work", "user:t1")])
        _ = try await backend.contextRecall(
            agent: agent, payload: dry(), kernel: kernel, provider: nil)
        let rec = try #require(
            backend.loadHistory(agent: agent, kernel: kernel).last {
                $0["kind"].asString == "recall"
            })
        let content = rec["content"].asString ?? ""
        #expect(
            content.contains("live task: \(f.id("user:t1")) \"TASK 1: latest work\""),
            Comment(rawValue: content))
    }

    @Test func theLiveRootIsFoundEvenWhenAFoldCollapsedIt() async throws {
        let (kernel, agent, f, backend) = await boot()
        let b = f.chain("b", 2)
        let rows =
            [f.row("user", "TASK 0: first", "user:t0")] + f.chain("a", 2)
            + [f.row("user", "TASK 1: latest work", "user:t1")] + b
        f.seed(rows)
        f.seed([f.fold([f.id("user:t1")] + f.ids(b), "task 1 folded", "body", false)])
        _ = try await backend.contextRecall(
            agent: agent, payload: dry(), kernel: kernel, provider: nil)
        let rec = try #require(
            backend.loadHistory(agent: agent, kernel: kernel).last {
                $0["kind"].asString == "recall"
            })
        let content = rec["content"].asString ?? ""
        // chain b's own root ("task b") is the newest root and the fold collapsed it too
        #expect(content.contains("live task: \(f.id("user:b"))"), Comment(rawValue: content))
    }

    // ── round-2 self-verb edges (B3.5, B5.2 … B5.11) ─────────────────────

    // B3.5 r2: `limit or None` — only ZERO means "no limit"; a NEGATIVE limit
    // returns an empty page.
    @Test func historyNegativeLimitIsAnEmptyPage() async throws {
        let (kernel, agent, f, _) = await boot()
        f.seed(f.chain("a", 1, big: false))
        let r = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("history"), "limit": .integer(-1),
                ]))
        ])[0].result
        #expect(r["rows"].asArray?.isEmpty == true, "\(r.serialize())")
        #expect(r["next"] == .null)
        #expect((r["total"].asInt ?? 0) > 0)
    }

    // B5.7: the digest of a CALLER-WRITTEN row — `kind or role` on python
    // truthiness, `head`/`bytes` from `str(content)`.
    @Test func historyDigestsACallerWrittenRowLikePython() async throws {
        let (kernel, agent, f, _) = await boot()
        f.seed([
            .object([
                "role": .string("user"), "kind": .string(""),
                "content": .object(["a": .integer(1)]),
                "id": .integer(kernel.rows.mintId()), "parent": .null,
            ])
        ])
        let r = await kernel.send([Item(agent.id, .object(["type": .string("history")]))])[0].result
        let row = try #require(r["rows"].asArray?.first)
        #expect(row["kind"].asString == "user", "an EMPTY kind falls back to role")
        #expect(row["head"].asString == "{'a': 1}")
        #expect(row["bytes"].asInt == 8)
    }

    // B5.2: `splitlines()` is python's boundary set — the INFORMATION
    // SEPARATORS split lines too, and swift's `isNewline` misses them.
    @Test func grepSplitsOnInformationSeparators() async throws {
        let (kernel, agent, f, _) = await boot()
        f.seed([f.row("user", "[from rest]\nneedle one\u{0c}needle two\rneedle three", "u:1")])
        let r = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("grep"), "query": .string("needle"),
                ]))
        ])[0].result
        #expect(r["total"].asInt == 3, "\(r.serialize())")
        let lines = (r["matches"].asArray ?? []).map { $0["line"].asInt }
        #expect(lines == [2, 3, 4])
        // `x\u{1c}needle` is two lines to python, one to `Character.isNewline`.
        let (k2, a2, f2, _) = await boot()
        f2.seed([f2.row("user", "x\u{1c}needle here", "u:1")])
        let r2 = await k2.send([
            Item(
                a2.id,
                .object([
                    "type": .string("grep"), "query": .string("needle"),
                ]))
        ])[0].result
        #expect(r2["matches"].asArray?.first?["line"].asInt == 2, "\(r2.serialize())")
    }

    // B5.8: a non-string content is `json.dumps(content, default=str)` — every
    // non-ASCII scalar ESCAPED. The model greps what it was shown.
    @Test func nonStringContentIsPythonsEscapedDump() async throws {
        let (kernel, agent, f, _) = await boot()
        f.seed([
            .object([
                "role": .string("user"), "content": .object(["k": .string("\u{e9}")]),
                "id": .integer(kernel.rows.mintId()), "parent": .null,
            ])
        ])
        let rec = await kernel.send([Item(agent.id, .object(["type": .string("recall")]))])[0]
            .result
        #expect(
            rec["messages"].asArray?.first?["content"].asString == #"{"k": "\u00e9"}"#,
            "\(rec.serialize())")
        let hit = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("grep"), "query": .string("u00e9"),
                ]))
        ])[0].result
        #expect(hit["total"].asInt == 1, "\(hit.serialize())")
        let miss = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("grep"), "query": .string("\u{e9}"),
                ]))
        ])[0].result
        #expect(miss["total"].asInt == 0, "\(miss.serialize())")
    }

    // B5.9: the AI's own `_as_id` — a digit string is STRIPPED first and `'0'`
    // IS an id, so `pull pid:'0'` reaches `not_found`, not `bad_arg`.
    @Test func pullPidPaddedStringPagesLikePython() async throws {
        let (kernel, agent, f, _) = await boot()
        f.seed(f.chain("a", 1, big: false))
        let zero = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("pull"), "pid": .string("0"),
                ]))
        ])[0].result
        #expect(zero["reason"].asString == "not_found", "\(zero.serialize())")
        #expect(zero["error"].asString?.contains("no stored body for pid 0") == true)
        // `'+5'` is not all digits — that one IS a bad arg.
        let plus = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("pull"), "pid": .string("+5"),
                ]))
        ])[0].result
        #expect(plus["reason"].asString == "bad_arg", "\(plus.serialize())")
        // A PADDED digit string resolves: `' 12 '` is 12.
        let body = "the body of the reply"
        let pid = kernel.rows.mintId()
        _ = try kernel.rows.bodyPut(agent.id, pid: pid, text: body)
        let padded = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("pull"), "pid": .string(" \(pid) "),
                ]))
        ])[0].result
        #expect(padded["text"].asString == body, "\(padded.serialize())")
    }

    // C70 (unblocks B5.1's stored-body twin): `kernel.rows.bodyGet` pages a
    // STORED body by CODE POINTS now, not graphemes — the fold branch was
    // already fixed in `AsyncRepliesTests.pullAndGrepWidthCountCodePoints`,
    // which this test's sibling comment named as waiting on C70.
    @Test func pullPagesACrlfBodyByCodePoints() async throws {
        let (kernel, agent, f, _) = await boot()
        f.seed(f.chain("a", 1, big: false))
        let pid = kernel.rows.mintId()
        _ = try kernel.rows.bodyPut(agent.id, pid: pid, text: String(repeating: "a\r\nb", count: 3))
        let r = await kernel.send([
            Item(
                agent.id,
                .object(["type": .string("pull"), "pid": .integer(pid), "length": .integer(5)]))
        ])[0].result
        #expect(r["text"].asString == "a\r\nba", "\(r.serialize())")
        #expect(r["next_offset"].asInt == 5)
        #expect(r["eof"] == .bool(false))
    }

    // B5.10: a scope is scanned in CPython's int-SET order, which decides which
    // row owns a line two bodies share.
    @Test func grepScopeScansBodiesInPythonSetOrder() {
        #expect(pySetOrder([1116, 1065, 1066, 1080]) == [1080, 1065, 1066, 1116])
        // The table grows when `fill*5 >= mask*3`, and the order changes with it.
        #expect(pySetOrder([1, 2, 3]) == [1, 2, 3])
        #expect(pySetOrder(Array(1...9)).count == 9)
    }

    // B5.11 + B5.1: pull refuses a negative length (python 2026-09-20) — it used
    // to slice from the END and hand back a negative `next_offset`.
    @Test func pullFoldNegativeLengthIsRefused() async throws {
        let (kernel, agent, f, _) = await boot()
        let fold = f.fold([], "t", "abcdefghij", false)
        f.seed([fold])
        let pid = try #require(rowId(fold))
        let r = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("pull"), "pid": .integer(pid), "length": .integer(-3),
                ]))
        ])[0].result
        #expect(r["error"].asString == "pull: length must not be negative", "\(r.serialize())")
        #expect(r["reason"].asString == "bad_arg")
    }

    // B5.3: `dry` is python-truthy — `1` is dry and makes NO model call.
    @Test func dryIsPythonTruthy() async throws {
        let (kernel, agent, f, backend) = await boot()
        f.seed([f.row("user", "GOAL", "user:goal")] + f.chain("a", 6, big: false))
        let asked = AskCounter()
        let provider = ComputedProvider { _ in
            asked.bump()
            return #"{"expand": [], "why": "asked"}"#
        }
        let r = try await backend.contextRecall(
            agent: agent, payload: .object(["dry": .integer(1)]), kernel: kernel,
            provider: provider)
        #expect(asked.count == 0, "dry:1 must make no model call")
        #expect(r["degraded"] == .bool(true) && r["why"].asString == "mechanical fit")
    }

    // B5.4: the model's answer is read with python's rules.
    @Test func recallModelAnswerFollowsPython() async throws {
        // An EMPTY answer is NO answer: degraded stays true.
        let (k1, a1, f1, b1) = await boot()
        f1.seed([f1.row("user", "GOAL", "user:goal")] + f1.chain("a", 6, big: false))
        let empty = try await b1.contextRecall(
            agent: a1, payload: .object([:]), kernel: k1,
            provider: ComputedProvider { _ in "{}" })
        #expect(empty["degraded"] == .bool(true) && empty["why"].asString == "mechanical fit")

        // `why: 0` is FALSY — it reads `chosen`, not `0`.
        let (k2, a2, f2, b2) = await boot()
        f2.seed([f2.row("user", "GOAL", "user:goal")] + f2.chain("a", 6, big: false))
        let zero = try await b2.contextRecall(
            agent: a2, payload: .object([:]), kernel: k2,
            provider: ComputedProvider { _ in #"{"expand": [], "why": 0}"# })
        #expect(zero["why"].asString == "chosen", "\(zero.serialize())")
        #expect(zero["degraded"] == .bool(false))

        // A non-string `why` is its python `str`.
        let (k3, a3, f3, b3) = await boot()
        f3.seed([f3.row("user", "GOAL", "user:goal")] + f3.chain("a", 6, big: false))
        let listy = try await b3.contextRecall(
            agent: a3, payload: .object([:]), kernel: k3,
            provider: ComputedProvider { _ in #"{"expand": [], "why": ["a"]}"# })
        #expect(listy["why"].asString == "['a']", "\(listy.serialize())")
    }

    // B5.4: `expand` is `_as_list`-ed as python does (2026-09-20) — a scalar is
    // ONE candidate, never iterated and never a raise.
    @Test func expandIsWrappedLikePython() async throws {
        let (k1, a1, f1, b1) = await boot()
        f1.seed([f1.row("user", "GOAL", "user:goal")] + f1.chain("a", 6, big: false))
        // A string is one candidate — "12" is no head id, so the answer still
        // counts (degraded false) and nothing is expanded.
        let chars = try await b1.contextRecall(
            agent: a1, payload: .object([:]), kernel: k1,
            provider: ComputedProvider { _ in #"{"expand": "12", "why": "w"}"# })
        #expect(chars["degraded"] == .bool(false) && chars["expanded"].asInt == 0)

        // A truthy int is wrapped: one candidate, and the recall completes.
        let (k2, a2, f2, b2) = await boot()
        f2.seed([f2.row("user", "GOAL", "user:goal")] + f2.chain("a", 6, big: false))
        let before = b2.loadHistory(agent: a2, kernel: k2).count
        let num = try await b2.contextRecall(
            agent: a2, payload: .object([:]), kernel: k2,
            provider: ComputedProvider { _ in #"{"expand": 5, "why": "w"}"# })
        #expect(num["degraded"] == .bool(false), "\(num.serialize())")
        #expect(b2.loadHistory(agent: a2, kernel: k2).count == before + 1, "the recall row is written")
    }

    // B5.5: `context_strategy` is read RAW — only an ABSENT key means compact.
    @Test func aNullContextStrategyIsReportedRawAndRefused() async throws {
        let (kernel, agent, f, backend) = await boot(
            contextWindow: 12000, meta: ["output_reserve": .integer(0), "context_strategy": .null])
        let st = await kernel.send([
            Item(
                agent.id,
                .object([
                    "type": .string("context_status")
                ]))
        ])[0].result
        #expect(st["strategy"] == .null, "\(st.serialize())")
        let r = await kernel.send([Item(agent.id, .object(["type": .string("reflect")]))])[0].result
        #expect(r["context_strategy"] == .null)
        // The projection REFUSES by name rather than silently running compact.
        f.seed([f.row("user", "GOAL", "user:goal")] + f.chain("a", 20))
        let big = (0..<40).map { plainTurn("user", "\(BIG) \($0)") }
        let outcome = await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "c",
            messages: [plainTurn("system", "sys")] + big, storeLen: big.count, kernel: kernel)
        guard case .failed(let err) = outcome else {
            Issue.record("the projection ran compact instead of refusing")
            return
        }
        #expect(
            err["error"].asString
                == "mock_backend: unknown context_strategy None "
                + "(valid: ['compact', 'truncate', 'strip', 'fold'])", "\(err.serialize())")
    }
}
