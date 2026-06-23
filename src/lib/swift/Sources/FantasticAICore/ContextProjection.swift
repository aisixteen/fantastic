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

extension AIBackend {
    // MARK: - the canonical notice

    /// The ONE canonical inbound context-notice — composed at the SEAM from a
    /// strategy's projection artifact. A `role:user` turn (the role every
    /// backend reliably attends to). Carries the protocol affordances: `recall`
    /// to page dropped turns back, and persist-to-memory. Model view ONLY.
    public func contextNotice(
        strategy: String, summary: String?, omittedMarker: Bool, droppedN: Int
    )
        -> JSON
    {
        var lines = [
            "[context-notice] Your conversation exceeded the window and was compacted "
                + "(strategy=\(strategy), \(droppedN) earlier turn(s) dropped from THIS view)."
        ]
        if let s = summary {
            lines.append("Summary of the dropped span:\n" + s)
        } else if omittedMarker {
            lines.append("An earlier span was omitted in place.")
        }
        lines.append(
            "The full transcript is intact in durable storage. To page dropped turns back, "
                + "send {type:'recall', query?, limit?} to your OWN id. If the dropped span "
                + "holds durable facts (names, decisions, preferences), persist them to your "
                + "memory agent now via the send tool — the earlier turns are leaving your live view."
        )
        return .object(["role": .string("user"), "content": .string(lines.joined(separator: "\n"))])
    }

    // MARK: - the seam

    /// Shape `messages` to fit the budget, prepend the notice, emit the
    /// `context` event. Returns `.success(projectedModelMessages)`, or
    /// `.failure(errorJSON)` for the `too_small` failsafe / unknown-strategy
    /// config error. Sets the public projection summary + private reaction
    /// cursor. Mirrors `_project_context`.
    func projectContext(
        provider: any AIProvider, agent: Agent, clientId: String, messages: [JSON], kernel: Kernel
    ) async -> ProjectionOutcome {
        let agentKey = agent.id.value
        let b = AIContext.budget(agent)
        if AIContext.estimateTokens(messages) <= b {
            setProjection(agentKey, .object(["fired": .bool(false)]))
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
        // The live user turn AND the notice envelope are both non-negotiable.
        if bodyBudget < lastCost + AIContext.noticeEnvelopeReserve {
            let window = AIContext.resolveContextWindow(agent)
            let hint =
                "the system prompt (\(sysTokens) tok) leaves no room in the \(window)-token "
                + "window for even one turn; reduce agents/menu or raise context_window"
            setProjection(agentKey, .object(["fired": .bool(false), "too_small": .bool(true)]))
            clearMark(agentKey)
            await kernel.emit(
                agent.id,
                .object([
                    "type": .string("context"), "source": .string(agentKey),
                    "ts": .double(nowSecs()), "phase": .string("too_small"),
                    "detail": .object([
                        "context_window": .integer(window), "system_tokens": .integer(sysTokens),
                        "hint": .string(hint),
                    ]),
                    "client_id": .string(clientId),
                ]))
            return .failed(
                .object(["error": .string("\(config.kind): context_insufficient — \(hint)")]))
        }
        let stratName = AIContext.strategyName(agent)
        if !AIStrategies.isKnown(stratName) {
            return .failed(
                .object([
                    "error": .string(
                        "\(config.kind): unknown context_strategy \(stratName) (valid: compact, truncate)"
                    )
                ]))
        }
        let recent = AIContext.recentN(agent)
        let proj: AIProjection =
            stratName == "truncate"
            ? AIStrategies.truncate(body: body, budget: bodyBudget)
            : await AIStrategies.compact(
                body: body, recentN: recent, budget: bodyBudget, provider: provider)
        let droppedPre = max(0, body.count - proj.body.count)
        let notice = contextNotice(
            strategy: stratName, summary: proj.summary, omittedMarker: proj.omittedMarker,
            droppedN: droppedPre)
        // Single budget authority: trim oldest body turns (tool-pairing-safe)
        // if degenerate. Never drop the last (live) turn — the failsafe above
        // guarantees room for [notice + live turn].
        var outBody = proj.body
        while outBody.count > 1
            && AIContext.estimateTokens(systemBlock + [notice] + outBody) > b
        {
            outBody = AIStrategies.dropOrphanTools(Array(outBody.dropFirst()))
        }
        let droppedN = max(0, body.count - outBody.count)
        let summarized = proj.summary != nil
        setProjection(
            agentKey,
            .object([
                "fired": .bool(true), "strategy": .string(stratName),
                "kept_turns": .integer(Int64(outBody.count)),
                "dropped_turns": .integer(Int64(droppedN)),
                "summarized": .bool(summarized),
            ]))
        setMark(agentKey, index: max(0, body.count - 1), client: clientId)
        await kernel.emit(
            agent.id,
            .object([
                "type": .string("context"), "source": .string(agentKey),
                "ts": .double(nowSecs()), "phase": .string("compacted"),
                "detail": .object([
                    "strategy": .string(stratName),
                    "dropped_turns": .integer(Int64(droppedN)),
                    "kept_turns": .integer(Int64(outBody.count)),
                    "summarized": .bool(summarized),
                ]),
                "client_id": .string(clientId),
            ]))
        return .projected(systemBlock + [notice] + outBody)
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
    /// `send` tool-calls — a `recall` to its OWN id, or a memory write
    /// (`set`/`append`/`replace`). `nil` if no compaction has fired.
    func deriveReaction(agent: Agent, kernel: Kernel) async -> JSON? {
        let key = agent.id.value
        let fired = getProjection(key)["fired"].asBool ?? false
        guard fired, let (idx, clientId) = markFor(key) else { return nil }
        let store = await loadHistory(agent: agent, client: clientId, kernel: kernel)
        var recalled = false
        var persisted = false
        var recallCount: Int64 = 0
        for m in store.dropFirst(idx) {
            guard m["role"].asString == "assistant" else { continue }
            // Tool calls now live as `<tool_call>` text inside the assistant
            // content — extract them with the same shared parser the loop uses.
            for (_, args) in extractToolCalls(m["content"].asString ?? "") {
                let target = args["target_id"].asString
                let ptype = args["payload"]["type"].asString
                if target == key && ptype == "recall" {
                    recalled = true
                    recallCount += 1
                } else if ptype == "set" || ptype == "append" || ptype == "replace" {
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

    /// Compact ONE stored turn for a `recall` reply: content capped so paging
    /// back can't itself blow the window. Turns are now pure text (tool
    /// calls/replies are inline `<tool_call>`/`<tool_response>` text), so this
    /// is just a cap. Bounds the REPLY only, never the store.
    private func recallRender(_ m: JSON) -> String {
        var s = m["content"].asString ?? ""
        if s.isEmpty { s = m.serialize() }
        return String(s.prefix(2000))
    }

    /// `recall` verb: page turns back from the DURABLE chat store (the FULL
    /// conversation, never trimmed). Read-only. args: client_id?, query?
    /// (case-insensitive substring), limit? (default 20, max 100), before?.
    func recallVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        let clientId = safeClient(payload["client_id"].asString ?? "cli")
        let full = await loadHistory(agent: agent, client: clientId, kernel: kernel)
        let q = (payload["query"].asString ?? "").lowercased()
            .trimmingCharacters(in: .whitespaces)
        let limit = Int(min(max(payload["limit"].asInt ?? 20, 1), 100))
        let before = payload["before"].asInt.map { Int($0) }
        var indexed = Array(full.enumerated())
        if let before { indexed = indexed.filter { $0.offset < before } }
        if !q.isEmpty {
            indexed = indexed.filter { $0.element.serialize().lowercased().contains(q) }
        }
        let total = indexed.count
        let truncated = total > limit
        let page = indexed.suffix(limit)
        let messages: [JSON] = page.map { (i, m) in
            .object([
                "index": .integer(Int64(i)),
                "role": m["role"].asString.map(JSON.string) ?? .null,
                "content": .string(recallRender(m)),
            ])
        }
        return .object([
            "messages": .array(messages), "total": .integer(Int64(total)),
            "truncated": .bool(truncated), "client_id": .string(clientId),
        ])
    }

    /// `context_status` verb: the context-budget posture + the last overflow
    /// projection + the model's derived reaction. Read-only.
    func contextStatusVerb(agent: Agent, kernel: Kernel) async -> JSON {
        let reaction = await deriveReaction(agent: agent, kernel: kernel)
        return .object([
            "context_window": .integer(AIContext.resolveContextWindow(agent)),
            "output_reserve": .integer(AIContext.outputReserve(agent)),
            "budget": .integer(AIContext.budget(agent)),
            "strategy": .string(AIContext.strategyName(agent)),
            "last_projection": getProjection(agent.id.value),
            "last_reaction": reaction ?? .null,
        ])
    }

    /// Shared protocol verb docs + the `emits` map, merged into reflect across
    /// all backends (the wire is shared even though per-backend `verbs` differ).
    func contextReflectFields(agent: Agent) -> OrderedDictionary<String, JSON> {
        var out: OrderedDictionary<String, JSON> = [:]
        out["context_window"] = .integer(AIContext.resolveContextWindow(agent))
        out["context_strategy"] = .string(AIContext.strategyName(agent))
        out["emits"] = .object([
            "token": .string(
                "{type:'token', stream_id, message_id, delta, accumulated, client_id}"),
            "status": .string(
                "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'tool_calling'|'done', detail}"
            ),
            "done": .string(
                "{type:'done', stream_id, message_id, accumulated?, client_id, error?}"),
            "context": .string(
                "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail} — the Context Protocol push half. compacted: detail={strategy, dropped_turns, kept_turns, summarized}. too_small: detail={context_window, system_tokens, hint} (model NOT called). Pull counterpart: the context_status verb."
            ),
        ])
        return out
    }
}
