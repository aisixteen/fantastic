// The Context-Protocol seam for the shared Swift AI backend: shape the
// assembled messages to fit the agent's token budget via its configured
// `context_strategy`, prepend the ONE canonical [context-notice], and emit a
// `context` event. The `too_small` failsafe is a failfast (model NOT called)
// that also emits a `context:too_small` event — NOT a fallback. The durable
// store is untouched (the notice lives only in the model view). Plus the
// `recall` + `context_status` verbs + the derived reaction read-model.
// Mirrors Python `ai_core/core.py` / Rust `projection.rs` + `verbs.rs`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Outcome of the projection seam: the projected model messages, or a refusal
/// JSON (the `too_small` failsafe / an unknown-strategy config error). A plain
/// enum because `JSON` is not an `Error` (so `Result` can't carry it).
public enum ProjectionOutcome: Sendable {
    case projected([JSON])
    case failed(JSON)
}

/// TOKEN CALIBRATION per agent: the prompt size the endpoint REPORTED for the last
/// pass divided by our own estimate of what we sent. The estimate is chars/4 over
/// the rendered messages; a real tokenizer disagrees with it, per model and per
/// language. The budget is measured in ESTIMATE units scaled by this ratio, so
/// what triggers a compaction is what the window really holds. 1.0 until an
/// endpoint reports usage. Python `_calib` / `_reported` — keyed by kernel here,
/// like every other AI side table.
enum AICalibration {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var calib: [UInt64: [String: Double]] = [:]
    nonisolated(unsafe) private static var reported: [UInt64: [String: Int]] = [:]

    static func calibration(_ kernel: Kernel, _ id: String) -> Double {
        lock.withLock { calib[kernel.serial]?[id] ?? 1.0 }
    }

    static func reportedTokens(_ kernel: Kernel, _ id: String) -> Int? {
        lock.withLock { reported[kernel.serial]?[id] }
    }

    static func record(_ kernel: Kernel, _ id: String, reported r: Int, estimate: Int64) {
        lock.withLock {
            calib[kernel.serial, default: [:]][id] = Double(r) / Double(max(1, estimate))
            reported[kernel.serial, default: [:]][id] = r
        }
    }

    /// TEST SEAM — python's `core._calib["ai"] = 10.0`.
    static func set(_ kernel: Kernel, _ id: String, _ c: Double?) {
        lock.withLock {
            if let c {
                calib[kernel.serial, default: [:]][id] = c
            } else {
                calib[kernel.serial]?.removeValue(forKey: id)
                reported[kernel.serial]?.removeValue(forKey: id)
            }
        }
    }

    static func forget(_ kernel: Kernel) {
        lock.withLock {
            calib.removeValue(forKey: kernel.serial)
            reported.removeValue(forKey: kernel.serial)
        }
    }

    /// Drop a deleted agent's ratio and report (`AIBackend.onDelete`).
    static func forget(_ kernel: Kernel, _ id: String) {
        lock.withLock {
            calib[kernel.serial]?.removeValue(forKey: id)
            reported[kernel.serial]?.removeValue(forKey: id)
        }
    }
}

/// `round(x, 3)` as python renders it — the calibration a status reports.
func round3(_ x: Double) -> Double { (x * 1000).rounded(.toNearestOrEven) / 1000 }

extension AIBackend {
    // MARK: - the canonical notice

    /// The ONE canonical inbound context-notice — composed at the SEAM from a
    /// strategy's `Projection` artifact (no strategy fabricates its own turn).
    /// A `role:user` turn (the role every backend reliably attends to). Carries
    /// the protocol affordances: `recall` to page dropped turns back, and
    /// persist-to-memory. Injected into the MODEL view ONLY — never the durable
    /// store. Python `_context_notice`, verbatim.
    public func contextNotice(
        strategy: String, summary: String?, omittedMarker: Bool, droppedN: Int,
        pinned: [String] = [], stripped: Int = 0, strippedBytes: Int = 0
    ) -> JSON {
        var lines = [
            "[context-notice] Your conversation exceeded the window and was compacted "
                + "(strategy=\(strategy), \(droppedN) earlier turn(s) dropped from THIS view)."
        ]
        if let s = summary {
            lines.append("Summary of the dropped span:\n" + s)
        } else if omittedMarker {
            lines.append("An earlier span was omitted in place.")
        }
        if stripped > 0 {
            lines.append(
                "\(stripped) reply bod(ies) were STRIPPED to their tag+title line "
                    + "(\(strippedBytes) bytes left your view); grep finds a line in a body held "
                    + "this run — after a restart, re-read it from its origin.")
        }
        if !pinned.isEmpty {
            // POINT AT THE BLOCK, DO NOT REPEAT IT. The facts live in their own stable
            // message ABOVE this notice, in the cached prefix. Embedding them here
            // would put the one append-only thing in the system inside the most
            // volatile one — the notice is rebuilt with a fresh `dropped_n` at every
            // cut, so a fact block carried in it would be rewritten every time and
            // take the whole cached prefix with it.
            lines.append(
                "Your \(pinned.count) checkpointed fact(s) are in the FACTS block above, "
                    + "unchanged — they still bind.")
        }
        lines.append(
            "The full transcript is intact in your memory store. To page dropped turns back, "
                + "send your OWN id `<type>recall</type>` (optional `<query>`, `<limit>`). If the dropped span holds "
                + "durable facts (names, decisions, preferences), persist them to your memory agent "
                + "now via the send tool — the earlier turns are leaving your live view."
        )
        return .object(["role": .string("user"), "content": .string(lines.joined(separator: "\n"))])
    }

    // MARK: - the seam

    /// Shape `messages` to fit the budget, prepend the notice, emit the
    /// `context` event. Returns `.projected(modelMessages)`, or
    /// `.failed(errorJSON)` for the `too_small` failsafe / unknown-strategy
    /// config error. Sets the public projection summary + private reaction
    /// cursor, writes the VIEW ARRAY, and books the fold-count debounce.
    /// Mirrors `_project_context`. Called ONCE per send, at turn entry.
    ///
    /// `storeLen` is the DURABLE store's length behind `messages` — the index the
    /// reaction cursor is recorded at. It is NOT derivable from `messages`, which is
    /// a deduped/trimmed view.
    func projectContext(
        provider: any AIProvider, agent: Agent, clientId: String, messages: [JSON],
        storeLen: Int, kernel: Kernel
    ) async throws -> ProjectionOutcome {
        let agentKey = agent.id.value
        // THE BUDGET IN ESTIMATE UNITS: the record's budget, divided by how much
        // the endpoint's own count exceeded our estimate last turn (calibration).
        let calib = AICalibration.calibration(kernel, agentKey)
        let b: Int64 =
            calib > 0
            ? max(AIConstants.BUDGET_FLOOR, Int64(Double(AIContext.budget(agent, modelWindow: config.modelWindow)) / calib))
            : AIContext.budget(agent, modelWindow: config.modelWindow)
        if AIContext.estimateTokens(messages) <= Int64(Double(b) * AIConstants.TRIGGER) {
            setProjection(
                agentKey,
                .object([
                    "fired": .bool(false), "trigger": .double(AIConstants.TRIGGER),
                    "calibration": .double(round3(calib)),
                ]))
            return .projected(messages)
        }
        let systemBlock = Array(messages.prefix(1))
        let body = Array(messages.dropFirst())
        if body.isEmpty {
            setProjection(agentKey, .object(["fired": .bool(false)]))
            return .projected(messages)
        }
        let sysTokens = AIContext.estimateTokens(systemBlock)
        let bodyBudget = b - sysTokens
        let lastCost = AIContext.estimateOne(body[body.count - 1])
        // The live user turn AND the prepended notice envelope are both
        // non-negotiable — if there's no room for BOTH, fail loud (so the trim
        // below can never drop the live turn).
        if bodyBudget < lastCost + AIConstants.NOTICE_ENVELOPE_RESERVE {
            let window = AIContext.resolveContextWindow(agent, modelWindow: config.modelWindow)
            let hint =
                "the system prompt (\(sysTokens) tok) leaves no room in the \(window)-token "
                + "window for even one turn; reduce agents/menu or raise context_window"
            setProjection(agentKey, .object(["fired": .bool(false), "too_small": .bool(true)]))
            clearMark(agentKey)
            await toCaller(
                kernel: kernel, agentId: agent.id, clientId: clientId,
                event: [
                    "type": .string("context"), "source": .string(agentKey),
                    "ts": .double(nowSecs()), "phase": .string("too_small"),
                    "detail": .object([
                        "context_window": .integer(window), "system_tokens": .integer(sysTokens),
                        // NAME THE KILLER. Measured 2026-08-26 (python, `live_subai_graph`,
                        // 131k window): this fired needing a single >126k-token turn and
                        // recorded nothing about it — the run's tempdir was gone before
                        // anyone could ask WHICH message was that big.
                        "last_message_tokens": .integer(lastCost),
                        "last_message_head": .string(
                            String(pyStr(body[body.count - 1]["content"]).unicodeScalars.prefix(200))),
                        "hint": .string(hint),
                    ]),
                ])
            return .failed(
                .object(["error": .string("\(config.kind): context_insufficient — \(hint)")]))
        }
        // ONLY A STRING NAMES A STRATEGY. A `context_strategy` of null or 5 is a
        // misconfiguration, and the refusal renders the value python's way
        // rather than quietly running compact.
        let stratValue = AIContext.strategy(agent)
        let stratName = stratValue.asString ?? ""
        guard case .string = stratValue, let strat = AIStrategies.getStrategy(stratName) else {
            // python: f"{cfg.name}: unknown context_strategy {strat_name!r} (valid: {strategy_names()})"
            let names = "[" + AIStrategies.strategyNames().map(pyRepr).joined(separator: ", ") + "]"
            return .failed(
                .object([
                    "error": .string(
                        "\(config.kind): unknown context_strategy \(pyRepr(stratValue)) (valid: \(names))"
                    )
                ]))
        }
        let ctx = ProjectionCtx(
            budget: bodyBudget, recentN: AIContext.recentN(agent),
            summarize: AIStrategies.makeSummarizer(provider),
            selfId: agent.id, kernel: kernel, clientId: clientId)
        let proj = try await strat(body, systemBlock, .object(agent.meta), ctx)
        let notice = contextNotice(
            strategy: stratName, summary: proj.summary, omittedMarker: proj.omittedMarker,
            droppedN: body.count - proj.body.count, pinned: proj.pinned,
            stripped: proj.stripped, strippedBytes: proj.strippedBytes)
        // THE STABLE PREFIX. System block, then the fact block, then everything
        // volatile. MEASURED on this stack (vLLM prefix cache, /metrics deltas): an
        // APPEND after the prefix reuses it in full (4752 of 6559 tokens, 72.3%),
        // while any edit inside it — or a cut taken from the front of it — reuses
        // nothing and recomputes everything. The block is its own message rather
        // than part of the notice for that reason.
        let factBlock: [JSON] =
            proj.pinned.isEmpty
            ? []
            : [
                .object([
                    "role": .string("user"),
                    "content": .string(
                        "FACTS (checkpointed by you; append-only, later "
                            + "lines supersede earlier):\n" + proj.pinned.joined(separator: "\n")),
                ])
            ]
        // Single budget authority: the strategy reserved the notice envelope, but
        // guard the final fit anyway — trim body turns (tool-pairing-safe). Never
        // drop the last (live) turn: the failsafe above guarantees room for
        // [notice + live turn], so the `count > 1` guard is a defensive backstop,
        // not a fallback.
        // Trim from the MIDDLE, not the front. `outBody[0]` is the goal turn every
        // strategy deliberately pins (`compact`/`truncate` both put it first); the
        // last entry is the live turn. Taking from the front silently undid the pin
        // — the exact regression the pin exists to prevent, re-introduced one layer
        // up. Measured 2026-08-28 (python): with a snug window the goal sentence was
        // absent from the projection entirely, in both strategies.
        var outBody = proj.body
        let stable = systemBlock + factBlock
        while outBody.count > 2 && AIContext.estimateTokens(stable + [notice] + outBody) > b {
            outBody = [outBody[0]] + Array(outBody[2...])
        }
        // Last resort, and only when nothing else is left to give: [goal, live]
        // still over budget. The live turn is what the model must answer, so the
        // goal is what goes — loudly, in the notice's own count, not silently.
        while outBody.count > 1 && AIContext.estimateTokens(stable + [notice] + outBody) > b {
            outBody = Array(outBody[1...])
        }
        let droppedN = max(0, body.count - outBody.count)
        let summarized = proj.summary != nil
        let viewIds = outBody.compactMap { m -> Int64? in
            guard let id = rowId(m), id != 0 else { return nil }
            return id
        }
        setProjection(
            agentKey,
            .object([
                "fired": .bool(true), "trigger": .double(AIConstants.TRIGGER),
                "calibration": .double(round3(calib)),
                "reported_prompt_tokens":
                    AICalibration.reportedTokens(kernel, agentKey).map { JSON.integer(Int64($0)) }
                    ?? .null,
                "strategy": .string(stratName),
                "kept_messages": .integer(Int64(outBody.count)),
                "dropped_messages": .integer(Int64(droppedN)),
                "summarized": .bool(summarized),
                "pinned_facts": .integer(Int64(proj.pinned.count)),
                "stripped": .integer(Int64(proj.stripped)),
                "stripped_bytes": .integer(Int64(proj.strippedBytes)),
                "folded": .integer(Int64(proj.folded)),
                // What the model actually sees this turn — so "after reload it sees
                // only the fold" is measured from the view, never self-reported.
                "view_ids": .array(viewIds.prefix(200).map(JSON.integer)),
                "view_ids_truncated": .bool(viewIds.count > 200),
            ]))
        // THE VIEW ARRAY, written by every projection: what the model sees, at
        // what resolution — so the next turn (and the next process) rebuilds the
        // same bytes.
        try await saveView(
            agent.id.value, kernel,
            ids: outBody.compactMap { m -> JSON? in
                guard let id = rowId(m), id != 0 else { return nil }
                return .object(["id": .integer(id), "res": .string(resOf(m))])
            },
            budget: b, why: "projection: \(stratName)")
        if proj.folded > 0 {
            // "When it feels it folded, it runs context_recall" — at the start of
            // the next turn, over the new head. DEBOUNCED: a recall is a model
            // call, and a recall after EVERY fold batch doubled the turn price
            // (measured 2026-09-06, python: 11 folds, 5 recalls in 60 turns). Every
            // RECALL_EVERY-th batch, or when a batch folded a lot at once.
            countFolds(agentKey, proj.folded)
        }
        // `storeLen`, NOT `body.count - 1`. `body` is the DEDUPED/trimmed model view
        // and `deriveReaction` slices the FULL durable store by this index; whenever
        // the trimmers dropped whole turns the index came up short by that many, so
        // the reader scanned an assistant turn written BEFORE the notice and reported
        // a reaction to a compaction the model had not yet seen.
        setMark(agentKey, index: storeLen, client: clientId)
        await toCaller(
            kernel: kernel, agentId: agent.id, clientId: clientId,
            event: [
                "type": .string("context"), "source": .string(agentKey),
                "ts": .double(nowSecs()), "phase": .string("compacted"),
                "detail": .object([
                    "strategy": .string(stratName),
                    "dropped_messages": .integer(Int64(droppedN)),
                    "kept_messages": .integer(Int64(outBody.count)),
                    "summarized": .bool(summarized),
                    "pinned_facts": .integer(Int64(proj.pinned.count)),
                    "stripped": .integer(Int64(proj.stripped)),
                    "stripped_bytes": .integer(Int64(proj.strippedBytes)),
                    "folded": .integer(Int64(proj.folded)),
                ]),
            ])
        return .projected(stable + [notice] + outBody)
    }

    // MARK: - state accessors

    func setProjection(_ key: String, _ value: JSON) {
        projectionLock.lock()
        projectionCache[key] = value
        projectionLock.unlock()
    }

    func getProjection(_ key: String) -> JSON {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        return projectionCache[key] ?? .null
    }

    func setMark(_ key: String, index: Int, client: String) {
        projectionLock.lock()
        compactionMark[key] = (index, client)
        projectionLock.unlock()
    }

    func clearMark(_ key: String) {
        projectionLock.lock()
        compactionMark[key] = nil
        projectionLock.unlock()
    }

    private func markFor(_ key: String) -> (Int, String)? {
        projectionLock.lock()
        defer { projectionLock.unlock() }
        return compactionMark[key]
    }

    // MARK: - derived reaction (the 'ack')

    /// Read-model over the durable transcript: AFTER the last compaction notice
    /// (its cursor), did the model react? Scans the same client's thread for
    /// `send` tool-calls — a `recall` to its OWN id, or a memory write.
    ///
    /// THE MEMORY VERBS are a file_bridge's MUTATING text verbs
    /// (`write`/`edit`/`append`) — a model persists what it must keep as plain
    /// files. `nil` if no compaction has fired.
    func deriveReaction(agent: Agent, kernel: Kernel) async throws -> JSON? {
        let key = agent.id.value
        let fired = getProjection(key)["fired"].asBool ?? false
        guard fired, let (idx, clientId) = markFor(key) else { return nil }
        _ = clientId
        let store = try await loadHistory(agent.id.value, kernel)
        var recalled = false
        var persisted = false
        var recallCount: Int64 = 0
        for m in store.dropFirst(idx) {
            guard m["role"].asString == "assistant" else { continue }
            // Tool calls live as envelope TEXT inside the assistant content —
            // extract them with the shared parser. It now reads all three
            // envelopes and accepts an unterminated-but-call-shaped block, so
            // this read-model SEES reactions it previously scored as absent (a
            // model that reacted in a shape the old scanner dropped was recorded
            // as not having reacted at all). Pinned by
            // `theReactionReaderSeesShapesItUsedToDrop`.
            for call in extractToolCalls(m["content"].asString ?? "") {
                let args = call.arguments
                let target = args["target_id"].asString
                let ptype = args["payload"]["type"].asString
                if target == key && ptype == "recall" {
                    recalled = true
                    recallCount += 1
                } else if ptype == "write" || ptype == "edit" || ptype == "append" {
                    persisted = true
                }
            }
        }
        return .object([
            "recalled": .bool(recalled), "persisted": .bool(persisted),
            "recall_count": .integer(recallCount),
        ])
    }

    // MARK: - verbs

    /// Render ONE stored turn for a recall reply — WHOLE, never truncated.
    ///
    /// Turns are pure text (tool calls are inline `<call>` elements, replies
    /// `[reply to …]` blocks), so this only serializes a non-str content. The
    /// page `limit` is the bound that belongs here: recall exists to hand back a
    /// detail the model no longer has, and a per-turn cap silently drops the very
    /// part it went looking for — the caller says how many turns it wants, and
    /// each one arrives intact. Python `_recall_render`.
    private func recallRender(_ m: JSON) -> String {
        // A NON-STRING CONTENT IS PYTHON'S DUMP, not compact raw JSON:
        // `json.dumps(content, default=str)` writes `', '`/`': '` and escapes
        // every non-ASCII scalar, so `{"k": "\u00e9"}` is what the model reads
        // back — and what its own `grep` has to match against.
        m["content"].asString ?? pyJsonDumps(m["content"])
    }

    /// args: query?:str (case-insensitive substring over the serialized turn —
    /// matches tool args/replies too), limit?:int (default 20, max 100),
    /// before?:int (store index — page backward). Pages turns back from the
    /// DURABLE store (the FULL conversation, never trimmed — so anything
    /// compaction dropped is one call away). Read-only. A page always fits INLINE
    /// (the cap); trimmed from the oldest end when it would not, with the index
    /// to continue from. Returns {messages:[{index,role,content}], total,
    /// truncated, note?}. Python `_recall`.
    func recallVerb(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
        let full = try await loadHistory(agent.id.value, kernel, behind: true)
        let rawQ = payload["query"]
        let q = pyStrip((pyTruthy(rawQ) ? pyStr(rawQ) : "").lowercased())
        let limit = max(1, min((try? pyIntOr(payload["limit"], 20)) ?? 20, 100))
        var indexed = Array(full.enumerated())
        // An int and only an int (python: `isinstance(before, int)`, bools out).
        if case .integer(let before) = payload["before"] {
            indexed = indexed.filter { $0.offset < Int(before) }
        }
        if !q.isEmpty {
            indexed = indexed.filter { pyJsonDumps($0.element).lowercased().contains(q) }
        }
        let truncated = indexed.count > limit
        var page = Array(indexed.suffix(limit))
        // A PAGE MUST FIT THE WINDOW IT IS GOING INTO. Measured 2026-09-05
        // (python): the default 20 turns of 6.8KB reads came back as one
        // 14,786-token reply against a 12,000 window; `too_small` fired as a
        // failfast and the chain died. The page is trimmed from the OLDEST end
        // (the newest of what was asked for is kept) and says so, with the index
        // to page further back from — the caller loses a page, never the run.
        let cap = AIContext.inlineCap(agent, modelWindow: config.modelWindow)
        var trimmed = 0
        func messages(_ pg: [(offset: Int, element: JSON)]) -> JSON {
            .array(
                pg.map { (i, m) in
                    .object([
                        "index": .integer(Int64(i)),
                        "role": m["role"],
                        "content": .string(recallRender(m)),
                    ])
                })
        }
        func serialized(_ pg: [(offset: Int, element: JSON)]) -> Int64 {
            // THE UNIT THE DELIVERY MEASURES: the whole reply, serialized, as one
            // user turn (`routeReply` dehydrates on exactly that). Sizing the inner
            // messages alone let a 5,243-byte recall slip past the cap and come
            // back as a stub (measured 2026-09-06, python recall_after_fold).
            // Margin as in `fitReply`.
            let body = renderReply(
                .object([
                    "messages": messages(pg),
                    "total": .integer(Int64(indexed.count)),
                    "truncated": .bool(true),
                    "note": .string(
                        "page trimmed by 999 older turn(s) to fit your window (cap "
                            + "\(cap) tok); pass before=999999 to page further back."),
                ]))
            return AIContext.estimateOne(.object(["role": .string("user"), "content": .string(body)]))
        }
        while page.count > 1 && serialized(page) > Int64(Double(cap) * 0.8) {
            page.removeFirst()
            trimmed += 1
        }
        var reply: OrderedDictionary<String, JSON> = [
            "messages": messages(page),
            "total": .integer(Int64(indexed.count)),
            "truncated": .bool(truncated || trimmed > 0),
        ]
        if trimmed > 0 {
            reply["note"] = .string(
                "page trimmed by \(trimmed) older turn(s) to fit your window (cap "
                    + "\(cap) tok); pass before=\(page[0].offset) to page further back.")
        }
        if page.isEmpty {
            // An empty page with no reason reads as "your history is lost" — the
            // opposite of what it usually means. Say why, so the caller can tell
            // the cases apart.
            let fired = getProjection(agent.id.value)["fired"].asBool ?? false
            let note: String
            if !q.isEmpty && !full.isEmpty {
                note = "query \(pyRepr(q)) matched none of the \(full.count) stored turns."
            } else if !fired {
                note =
                    "nothing has been compacted away — recall pages back turns that "
                    + "compaction DROPPED, and none have been. If you can see this reply, "
                    + "your live context already holds the whole conversation."
            } else {
                note = "no stored turns in this page."
            }
            reply["note"] = .string(note)
        }
        return .object(reply)
    }

    /// `context_status` verb: the context-budget posture + the last overflow
    /// projection + the model's derived reaction + where the last `assemble`
    /// took its view from. Read-only. Python `_context_status`.
    func contextStatusVerb(agent: Agent, kernel: Kernel) async throws -> JSON {
        let reaction = try await deriveReaction(agent: agent, kernel: kernel)
        let key = agent.id.value
        return .object([
            "context_window": .integer(AIContext.resolveContextWindow(agent, modelWindow: config.modelWindow)),
            "output_reserve": .integer(AIContext.outputReserve(agent)),
            "budget": .integer(AIContext.budget(agent, modelWindow: config.modelWindow)),
            "trigger": .double(AIConstants.TRIGGER),
            "calibration": .double(round3(AICalibration.calibration(kernel, key))),
            "reported_prompt_tokens":
                AICalibration.reportedTokens(kernel, key).map { JSON.integer(Int64($0)) } ?? .null,
            "strategy": AIContext.strategy(agent),
            "last_projection": getProjection(agent.id.value),
            "last_reaction": reaction ?? .null,
            "view_source": getViewSource(agent.id.value).map(JSON.string) ?? .null,
        ])
    }

    /// The two context-posture fields reflect carries, in python's order.
    func contextReflectFields(agent: Agent) -> OrderedDictionary<String, JSON> {
        var out: OrderedDictionary<String, JSON> = [:]
        out["context_window"] = .integer(AIContext.resolveContextWindow(agent, modelWindow: config.modelWindow))
        out["context_strategy"] = AIContext.strategy(agent)
        out["memory_store_id"] = agent.metaValue(forKey: "memory_store_id") ?? .null
        // WHERE its key is read — `{store, ref}`, a pointer, never the secret. An agent
        // copying this one's record needs it (e2e subai, 2026-10-13: a keyed door, and
        // the template it was told to read did not show it).
        out["credentials"] = agent.metaValue(forKey: "credentials") ?? .null
        return out
    }
}
