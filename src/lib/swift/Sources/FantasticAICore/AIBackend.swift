// AIBackend — the shared reflect-driven LLM-agent machinery behind the
// `AIProvider` seam. The ollama / NIM / Apple-FM bundles supply only
// their `AIProvider` impl + an `AIBackendConfig`, and dispatch every
// verb through the `handle` method here.
//
// Extracted from the three (formerly duplicated) Swift backends. The
// shared layer owns: per-(agent, client_id) chat history, the
// epoch-bump cancellation state, the verb bodies (reflect / boot /
// shutdown / send / history / interrupt / backend_state), the spawned
// streaming task, and the token / done event emission. The providers
// own only the upstream wire (NDJSON, SSE+429+Bearer, on-device
// session).
//
// Wire contract (byte-identical to the Python reference, enforced by
// FantasticParityTests):
//
//   send → {queued, stream_id, message_id, client_id}
//   token event → {type:token, stream_id, message_id, delta,
//                  accumulated, client_id}
//   done  event → {type:done, stream_id, message_id, accumulated?,
//                  client_id, error?}
//   history → {messages, client_id}
//   interrupt → {interrupted:true}
//
// This module MUST NOT import FoundationModels (or any provider SDK).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Construct the shared backend machinery for one bundle. Returns an
/// `AIBackend` the bundle's `handle` delegates straight to. Mirrors
/// Rust's `buildAIBackend` / the Python `ai_core` factory.
public func buildAIBackend(_ config: AIBackendConfig) -> AIBackend {
    AIBackend(config: config)
}

public final class AIBackend: @unchecked Sendable {
    public let config: AIBackendConfig

    /// Per-`send` wall-clock ceiling. A generation that streams past this
    /// is force-stopped with a terminal `done{error:"send: timeout …"}`.
    /// Bounds runaway tool loops / a stuck provider. Mirrors Python
    /// `SEND_TIMEOUT` / Rust `SEND_TIMEOUT_SECS` (default 180s; the env
    /// override `FANTASTIC_AI_SEND_TIMEOUT` is read once at init).
    public static let defaultSendTimeoutSeconds: Double = 180
    private let sendTimeoutSeconds: Double = {
        if let raw = ProcessInfo.processInfo.environment["FANTASTIC_AI_SEND_TIMEOUT"],
            let v = Double(raw), v > 0
        {
            return v
        }
        return AIBackend.defaultSendTimeoutSeconds
    }()

    /// Cancellation state — epoch-bump pattern (lifted verbatim from
    /// the FM backend, which had the most robust version). `interrupt`
    /// bumps `interruptEpoch`; every stream records the epoch it
    /// started under and polls `isStreamCancelled` each tick. A stream
    /// is cancelled iff its `startedAt` epoch is older than the current
    /// epoch OR its id is explicitly in `cancelledStreams`. Epoch
    /// counters are race-free against staggered stream-death and don't
    /// bleed into post-interrupt sends.
    private let cancelLock = NSLock()
    private var interruptEpoch: UInt64 = 0
    private var cancelledStreams: Set<String> = []

    /// In-flight stream count for `backend_state` reporting.
    private let stateLock = NSLock()
    private var inFlight: Int = 0

    /// Lazy per-agent "menu of capabilities" cache (id → reflected
    /// peers). `nil`/absent means "rebuild on the next assemble". The
    /// `refresh_menu` verb + each tool batch invalidate it. Mirrors the
    /// Rust `BackendState.menu` / Python `_menu_cache`.
    private let menuLock = NSLock()
    private var menuCache: [String: [JSON]] = [:]

    /// Per-agent send coordination (FIFO lock + queue + in-flight entry)
    /// behind the `status` verb + phase events. Lazily created per id.
    private let runStatesLock = NSLock()
    private var runStates: [String: AIAgentRunState] = [:]

    /// Context-Protocol state, keyed by agent id. `projectionCache` is the
    /// PUBLIC `context_status.last_projection` summary; `compactionMark` is the
    /// PRIVATE `(fired_at_index, client_id)` reaction cursor `deriveReaction`
    /// scans from. Both guarded by `projectionLock`.
    let projectionLock = NSLock()
    var projectionCache: [String: JSON] = [:]
    var compactionMark: [String: (Int, String)] = [:]

    init(config: AIBackendConfig) {
        self.config = config
    }

    /// Get (or lazily create) the coordination state for an agent.
    private func runState(_ id: String) -> AIAgentRunState {
        runStatesLock.lock()
        defer { runStatesLock.unlock() }
        if let s = runStates[id] { return s }
        let s = AIAgentRunState()
        runStates[id] = s
        return s
    }

    /// Fractional unix seconds — status timestamps + entry ages.
    func nowSecs() -> Double { Date().timeIntervalSince1970 }

    /// Opaque id for one user submission (status correlation).
    private func mintSendId() -> String { "snd_\(UUID().uuidString.prefix(8))" }

    // MARK: - Verb dispatch

    /// Single entry point — the bundle's `handle` forwards here.
    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent")])
        }
        switch verb {
        case "reflect":
            return reflectReply(agent: agent)
        case "boot":
            return .object(["ok": .bool(true)])
        case "shutdown":
            return .object(["ok": .bool(true)])
        case "send":
            return await sendVerb(agent: agent, payload: payload, kernel: kernel)
        case "history":
            return await historyVerb(agent: agent, payload: payload, kernel: kernel)
        case "recall":
            return await recallVerb(agent: agent, payload: payload, kernel: kernel)
        case "context_status":
            return await contextStatusVerb(agent: agent, kernel: kernel)
        case "status":
            return statusVerb(agentId: agentId, payload: payload)
        case "interrupt":
            return interruptVerb(payload: payload)
        case "refresh_menu":
            invalidateMenu(agentId)
            return .object(["refreshed": .bool(true)])
        case "backend_state":
            return backendStateReply(agent: agent)
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    // MARK: - reflect / backend_state

    private func reflectReply(agent: Agent) -> JSON {
        var fields: OrderedDictionary<String, JSON> = [:]
        fields["id"] = .string(agent.id.value)
        fields["sentence"] = .string(config.sentence)
        fields["kind"] = .string(config.kind)
        fields["provider"] = .string(config.provider)
        for (k, v) in config.reflectExtra(agent) {
            fields[k] = v
        }
        // Context Protocol: context_window/context_strategy + the shared emits map.
        for (k, v) in contextReflectFields(agent: agent) {
            fields[k] = v
        }
        // Merge the shared protocol verbs (recall, context_status) into the
        // per-backend `verbs` map so the capability is discoverable.
        var verbs = config.verbs.asObject ?? [:]
        verbs["recall"] = .string(
            "args: client_id?, query? (substring), limit? (max 100), before?. Pages turns back from the durable store (lossless on demand after compaction). Returns {messages, total, truncated, client_id}.")
        verbs["context_status"] = .string(
            "No args. Context-budget posture + last compaction + derived reaction. Returns {context_window, output_reserve, budget, strategy, last_projection, last_reaction}.")
        fields["verbs"] = .object(verbs)
        return .object(fields)
    }

    private func backendStateReply(agent: Agent) -> JSON {
        var fields: OrderedDictionary<String, JSON> = [:]
        fields["provider"] = .string(config.provider)
        for (k, v) in config.backendStateExtra(agent) {
            fields[k] = v
        }
        return .object(fields)
    }

    // MARK: - send

    private func sendVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        guard let text = payload["text"].asString else {
            return .object(["error": .string("send requires text")])
        }
        let clientId = payload["client_id"].asString ?? "cli"

        // Build the provider FIRST — backends that must refuse before
        // touching history (NIM no-api_key, FM unavailable) return their
        // exact refusal body here, preserving wire byte-identity.
        let result = await config.makeProvider(agent, clientId, kernel)
        let provider: any AIProvider
        switch result {
        case .refused(let body):
            return body
        case .provider(let p):
            provider = p
        }

        let streamId = "stm_\(UUID().uuidString.prefix(8))"
        let messageId = "msg_\(UUID().uuidString.prefix(8))"
        let userMessageId = "msg_\(UUID().uuidString.prefix(8))"

        // Enqueue this submission. Only ONE generation runs at a time per
        // agent (FIFO) — concurrent callers queue in arrival order, which
        // also keeps the file_bridge-persisted history race-free.
        let sendId = mintSendId()
        let st = runState(agent.id.value)
        st.enqueue(
            AIQueuedEntry(
                clientId: clientId, text: text, sendId: sendId, queuedAt: nowSecs()))

        // Contention probe (best-effort, like Rust's try_lock): if a
        // generation already holds the lock, tell the caller it's queued.
        if await st.fifo.busy() {
            let ahead = max(0, st.queueDepth() - 1)
            await kernel.emit(
                agent.id,
                .object([
                    "type": .string("queued"),
                    "source": .string(agent.id.value),
                    "send_id": .string(sendId),
                    "client_id": .string(clientId),
                ]))
            await emitStatus(
                agentId: agent.id, clientId: clientId, phase: "queued", st: st,
                extraDetail: ["send_id": .string(sendId), "ahead": .integer(Int64(ahead))],
                kernel: kernel)
        }

        bumpInFlight(+1)
        Task { [weak self, weak kernel] in
            guard let self = self, let kernel = kernel else { return }
            await st.fifo.acquire()
            st.popToCurrent(sendId: sendId, startedAt: self.nowSecs(), phase: "thinking")
            await self.emitStatus(
                agentId: agent.id, clientId: clientId, phase: "thinking", st: st,
                kernel: kernel)
            await self.runStream(
                provider: provider,
                streamId: streamId,
                messageId: messageId,
                userMessageId: userMessageId,
                agent: agent,
                clientId: clientId,
                st: st,
                userText: text,
                kernel: kernel
            )
            st.clearCurrent()
            await st.fifo.release()
        }

        return .object([
            "queued": .bool(true),
            "stream_id": .string(streamId),
            "message_id": .string(messageId),
            "client_id": .string(clientId),
        ])
    }

    private func runStream(
        provider: any AIProvider,
        streamId: String,
        messageId: String,
        userMessageId: String,
        agent: Agent,
        clientId: String,
        st: AIAgentRunState,
        userText: String,
        kernel: Kernel
    ) async {
        let agentId = agent.id
        defer {
            clearStreamCancel(streamId)
            bumpInFlight(-1)
        }

        // Snapshot the cancel epoch before the first token so interrupts
        // that fire mid-stream are detected via `startedAt < epoch`.
        let startedAtEpoch = currentEpoch()
        let deadline = Date().addingTimeInterval(sendTimeoutSeconds)

        // Load + persist the user turn UNDER the FIFO lock (we hold it
        // here), so serialized sends on one client can't race load→save.
        // The user row carries `id` only for stateless backends (FM bubble
        // identity); ollama/NIM don't. Stateless keeps history UI-only.
        let prior = await loadHistory(agent: agent, client: clientId, kernel: kernel)
        let persistBase = prior + [userTurn(id: userMessageId, text: userText)]
        await saveHistory(agent: agent, client: clientId, kernel: kernel, rows: persistBase)
        let modelHistory = config.stateless ? [] : prior

        // Rebuild the system block every turn from the live substrate
        // (primer + self-reflect + agent menu + send how-to + per-backend
        // extra). The system block is NOT persisted.
        let systemContent = await assembleSystemPrompt(agent: agent, kernel: kernel)
        var messages: [JSON] = [
            .object(["role": .string("system"), "content": .string(systemContent)])
        ]
        messages.append(contentsOf: modelHistory)
        messages.append(
            .object(["role": .string("user"), "content": .string(userText)]))

        // Context-Protocol seam: shape what the MODEL sees this turn to fit the
        // window (prepending the canonical [context-notice]), ONCE at entry —
        // never mid-tool-loop. `messages` is the model view; persistence uses
        // `persistBase`/`newTurns` (the full conversation), so the durable store
        // is never trimmed and the notice is never persisted. Skipped for
        // stateless backends (FM owns its context; modelHistory is empty).
        if !config.stateless {
            switch await projectContext(
                provider: provider, agent: agent, clientId: clientId, messages: messages,
                kernel: kernel)
            {
            case .projected(let projected):
                messages = projected
            case .failed(let err):
                // too_small failsafe / unknown-strategy — the model is NOT
                // called. The seam already emitted context:too_small. Surface
                // the error as a terminal done + leave the base (user turn) persisted.
                await emitStatus(
                    agentId: agentId, clientId: clientId, phase: "done", st: st,
                    extraDetail: ["reason": .string("error")], kernel: kernel)
                await emitDone(
                    kernel: kernel, agentId: agentId, streamId: streamId, messageId: messageId,
                    clientId: clientId, accumulated: "",
                    error: err["error"].asString ?? "context projection error")
                return
            }
        }

        // The agentic loop: stream a pass, dispatch any tool-calls back
        // through the kernel, feed the results in, repeat until the model
        // stops emitting tools. FM yields no tool-calls (Apple runs them
        // inside the session), so it runs exactly one pass. Mirrors Rust
        // `run_generation` / Python `_run`.
        var accumulated = ""  // generation-wide, for the UI token stream
        var lastText = ""  // current pass text → the final assistant turn
        var newTurns: [JSON] = []  // assistant/tool turns to persist at the end
        var cancelled = false
        var iteration = 0

        loop: while true {
            iteration += 1
            if iteration > 1 {
                // Re-entering after a tool batch — back to `thinking`.
                await emitStatus(
                    agentId: agentId, clientId: clientId, phase: "thinking", st: st,
                    kernel: kernel)
            }
            var passText = ""
            var passToolCalls: [JSON] = []
            var firstToken = true
            do {
                // RAW tool-calling: the provider streams pure text; the SHARED
                // parser splits content tokens from `<tool_call>` envelopes (no
                // native tools). `renderForModel` projects stored turns to plain
                // text every chat template renders.
                let stream = parseToolCalls(provider.chat(messages: renderForModel(messages)))
                for try await chunk in stream {
                    if Date() > deadline {
                        await provider.stop()
                        await emitStatus(
                            agentId: agentId, clientId: clientId, phase: "done", st: st,
                            extraDetail: ["reason": .string("timeout")], kernel: kernel)
                        await emitDone(
                            kernel: kernel, agentId: agentId,
                            streamId: streamId, messageId: messageId,
                            clientId: clientId, accumulated: accumulated,
                            error: "send: timeout after \(Int(sendTimeoutSeconds))s")
                        return
                    }
                    if isStreamCancelled(streamId: streamId, startedAt: startedAtEpoch) {
                        await provider.stop()
                        if config.emitInterruptedError {
                            // FM shape: terminal `done{error:"interrupted"}`,
                            // partial turn NOT persisted.
                            await emitStatus(
                                agentId: agentId, clientId: clientId, phase: "done", st: st,
                                extraDetail: ["reason": .string("interrupted")], kernel: kernel)
                            await emitDone(
                                kernel: kernel, agentId: agentId,
                                streamId: streamId, messageId: messageId,
                                clientId: clientId, accumulated: accumulated,
                                error: "interrupted")
                            return
                        }
                        cancelled = true
                        break
                    }
                    switch chunk {
                    case .token(let delta):
                        if firstToken {
                            firstToken = false
                            await emitStatus(
                                agentId: agentId, clientId: clientId, phase: "streaming",
                                st: st, kernel: kernel)
                        }
                        accumulated += delta
                        passText += delta
                        st.updateCurrent { $0.textSoFar += delta }
                        await kernel.emit(
                            agentId,
                            .object([
                                "type": .string("token"),
                                "stream_id": .string(streamId),
                                "message_id": .string(messageId),
                                "delta": .string(delta),
                                "accumulated": .string(accumulated),
                                "client_id": .string(clientId),
                            ]))
                    case .toolCall(let call):
                        passToolCalls.append(call)
                    }
                }
            } catch {
                await emitStatus(
                    agentId: agentId, clientId: clientId, phase: "done", st: st,
                    extraDetail: ["reason": .string("error")], kernel: kernel)
                await emitDone(
                    kernel: kernel, agentId: agentId,
                    streamId: streamId, messageId: messageId,
                    clientId: clientId, accumulated: accumulated,
                    error: "\(error)")
                return
            }
            lastText = passText

            // No tools (or interrupted) → this pass is the final answer.
            if passToolCalls.isEmpty || cancelled { break loop }

            // Record the assistant turn as TEXT — its prose plus the
            // `<tool_call>` envelope(s) it emitted (no structured `tool_calls`
            // field; raw, single-truth). Next pass the model re-reads its own
            // call as text.
            let callTags = passToolCalls.map { renderToolCallChunk($0) }.joined(separator: "\n")
            let assistantText =
                passText.isEmpty
                ? callTags : (callTags.isEmpty ? passText : passText + "\n" + callTags)
            let assistantTurnWithTools: JSON = .object([
                "role": .string("assistant"),
                "content": .string(assistantText),
            ])
            messages.append(assistantTurnWithTools)
            newTurns.append(assistantTurnWithTools)

            // Phase → tool_calling, noting the first call for `status`.
            let toolDetail = toolEntry(passToolCalls.first)
            st.updateCurrent { $0.lastTool = toolDetail }
            await emitStatus(
                agentId: agentId, clientId: clientId, phase: "tool_calling", st: st,
                extraDetail: toolDetail.map { ["tool": $0] } ?? [:], kernel: kernel)

            // Dispatch, then fold every reply into ONE `role:tool` turn carrying
            // `<tool_response>` text (mapped to role:user by `renderForModel`).
            let results = await dispatchToolCalls(
                passToolCalls, parallel: config.parallelTools, kernel: kernel)
            let toolContent = results.map {
                "<tool_response name=\"\($0.0)\">\($0.1)</tool_response>"
            }.joined(separator: "\n")
            let toolTurn: JSON = .object([
                "role": .string("tool"), "content": .string(toolContent),
            ])
            messages.append(toolTurn)
            newTurns.append(toolTurn)

            // The population may have changed (a tool created/deleted an
            // agent) — rebuild the menu before the next pass.
            invalidateMenu(agentId)
        }

        // Persist the full chat: base (prior + user turn) + the
        // intermediate tool turns + the final assistant turn. One write of
        // the whole conversation (matches Rust/Python `save_history`). The
        // error / FM-interrupt / timeout paths return earlier and leave the
        // base (with just the user turn) as the persisted record.
        let finalRows =
            persistBase + newTurns
            + [assistantTurn(id: messageId, content: lastText)]
        await saveHistory(agent: agent, client: clientId, kernel: kernel, rows: finalRows)

        await emitStatus(
            agentId: agentId, clientId: clientId, phase: "done", st: st,
            extraDetail: ["reason": .string("ok")], kernel: kernel)
        await emitDone(
            kernel: kernel, agentId: agentId,
            streamId: streamId, messageId: messageId,
            clientId: clientId, accumulated: accumulated, error: nil)
    }

    /// Compact `{call_id, target, verb}` summary of a tool-call chunk for
    /// the `status` event / snapshot. `nil` when no call.
    private func toolEntry(_ call: JSON?) -> JSON? {
        guard let call else { return nil }
        let fn = call["function"]
        var args: JSON = .object([:])
        if let s = fn["arguments"].asString {
            args = (try? JSON.parse(s)) ?? .object([:])
        } else if fn["arguments"].asObject != nil {
            args = fn["arguments"]
        }
        return .object([
            "call_id": .string(call["id"].asString ?? ""),
            "target": .string(args["target_id"].asString ?? ""),
            "verb": .string(args["payload"]["type"].asString ?? ""),
        ])
    }

    // MARK: - status (verb + phase events)

    /// Broadcast a `status` event AND keep the in-flight entry's phase in
    /// sync so the on-demand `status` verb agrees. Mirrors Rust
    /// `emit_status`. Routed on the agent's own inbox (swift's uniform
    /// token/done delivery), tagged with `client_id`.
    private func emitStatus(
        agentId: AgentId,
        clientId: String,
        phase: String,
        st: AIAgentRunState,
        extraDetail: [String: JSON] = [:],
        kernel: Kernel
    ) async {
        var sendId: String?
        var startedAt: Double?
        st.updateCurrent {
            $0.phase = phase
            sendId = $0.sendId
            startedAt = $0.startedAt
        }
        var detail: OrderedDictionary<String, JSON> = [:]
        for (k, v) in extraDetail { detail[k] = v }
        if let sendId, detail["send_id"] == nil { detail["send_id"] = .string(sendId) }
        if let startedAt, detail["started_at"] == nil { detail["started_at"] = .double(startedAt) }
        if detail["queue_depth"] == nil {
            detail["queue_depth"] = .integer(Int64(st.queueDepth()))
        }
        await kernel.emit(
            agentId,
            .object([
                "type": .string("status"),
                "source": .string(agentId.value),
                "phase": .string(phase),
                "detail": .object(detail),
                "ts": .double(nowSecs()),
                "client_id": .string(clientId),
            ]))
    }

    /// `status` verb — privacy-filtered snapshot of the in-flight entry +
    /// this client's own pending queue. Other clients' text is redacted.
    /// Mirrors Rust `status_snapshot`.
    private func statusVerb(agentId: AgentId, payload: JSON) -> JSON {
        let requesting = payload["client_id"].asString.map(safeClient)
        let st = runState(agentId.value)
        let cur = st.currentSnapshot()
        let queue = st.queueSnapshot()

        let currentOut: JSON = cur.map { redactEntry($0, requesting: requesting) } ?? .null

        var minePending: [JSON] = []
        var othersPending = 0
        for q in queue {
            if let req = requesting, q.clientId == req {
                minePending.append(
                    .object([
                        "send_id": .string(q.sendId),
                        "text": .string(q.text),
                        "queued_at": .double(q.queuedAt),
                    ]))
            } else {
                othersPending += 1
            }
        }

        return .object([
            "source": .string(agentId.value),
            "client_id": requesting.map(JSON.string) ?? .null,
            "generating": .bool(cur != nil),
            "current": currentOut,
            "mine_pending": .array(minePending),
            "others_pending": .integer(Int64(othersPending)),
        ])
    }

    /// Render the in-flight entry, exposing text fields only to its owner.
    private func redactEntry(_ c: AICurrentEntry, requesting: String?) -> JSON {
        let isMine = requesting.map { $0 == c.clientId } ?? false
        let elapsed = max(0, nowSecs() - c.startedAt)
        var out: OrderedDictionary<String, JSON> = [
            "client_id": .string(c.clientId),
            "send_id": .string(c.sendId),
            "started_at": .double(c.startedAt),
            "phase": .string(c.phase),
            "elapsed": .double(elapsed),
            "is_mine": .bool(isMine),
        ]
        if isMine {
            out["text"] = .string(c.text)
            out["text_so_far"] = .string(c.textSoFar)
            if let t = c.lastTool { out["last_tool"] = t }
        }
        return .object(out)
    }

    // MARK: - prompt assembly + tool dispatch

    /// Rebuild the system prompt from the live substrate: lean primer
    /// (id-index of the tree + bundle catalog), the agent's own
    /// self-reflect, the lazy menu of peers, the universal `send` how-to,
    /// and any per-backend extra (FM's always-inject memory). Mirrors
    /// Rust `assemble_messages` / Python `_assemble`.
    func assembleSystemPrompt(agent: Agent, kernel: Kernel) async -> String {
        let selfId = agent.id
        let primer = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("reflect"),
                "tree": .string("ids"),
                "bundles": .string("ids"),
            ]))
        let me = await kernel.send(
            selfId, .object(["type": .string("reflect"), "tree": .string("none")]))

        // Lazy menu: rebuild only on a cache miss.
        let menu: [JSON]
        if let cached = cachedMenu(selfId.value) {
            menu = cached
        } else {
            let built = await buildMenu(selfId: selfId, kernel: kernel)
            storeMenu(selfId.value, built)
            menu = built
        }

        var blocks: [String] = [
            renderReflect(primer),
            "You are `\(selfId.value)`. " + renderReflect(me),
            renderMenu(menu),
            SEND_HOWTO,
        ]
        let extra = await config.systemPromptExtra(agent, kernel)
        if !extra.isEmpty { blocks.append(extra) }
        return blocks.joined(separator: "\n\n")
    }

    /// Drop an agent's cached menu so the next assemble rebuilds it.
    private func invalidateMenu(_ id: AgentId) {
        menuLock.lock()
        defer { menuLock.unlock() }
        menuCache[id.value] = nil
    }

    /// Synchronous menu-cache accessors (NSLock can't span `await`).
    private func cachedMenu(_ id: String) -> [JSON]? {
        menuLock.lock()
        defer { menuLock.unlock() }
        return menuCache[id]
    }

    private func storeMenu(_ id: String, _ menu: [JSON]) {
        menuLock.lock()
        defer { menuLock.unlock() }
        menuCache[id] = menu
    }

    /// Dispatch one provider tool-call (always the universal `send`)
    /// through the kernel and shape the `role:tool` reply message. The
    /// call's `function.arguments` may be a JSON string (OpenAI/NIM) or a
    /// parsed object (ollama) — both resolve to `{target_id, payload}`.
    private func dispatchToolCall(_ call: JSON, kernel: Kernel) async -> (String, String) {
        let fn = call["function"]
        let name = fn["name"].asString ?? "send"
        let rawArgs = fn["arguments"]
        var args: JSON = .object([:])
        if let s = rawArgs.asString {
            args = (try? JSON.parse(s)) ?? .object([:])
        } else if rawArgs.asObject != nil {
            args = rawArgs
        }
        let target = args["target_id"].asString ?? ""
        let payload = args["payload"]
        let reply: JSON
        if target.isEmpty {
            reply = .object(["error": .string("empty target_id")])
        } else {
            reply = await kernel.send(AgentId(target), payload)
        }
        return (name, reply.serialize())
    }

    /// Dispatch a batch of tool-calls, preserving model-emitted order in the
    /// returned `(name, replyJSON)` pairs. Concurrent when `parallel` (ollama /
    /// FM), serial otherwise (NIM).
    private func dispatchToolCalls(
        _ calls: [JSON], parallel: Bool, kernel: Kernel
    ) async -> [(String, String)] {
        if !parallel {
            var out: [(String, String)] = []
            for c in calls {
                out.append(await dispatchToolCall(c, kernel: kernel))
            }
            return out
        }
        return await withTaskGroup(of: (Int, (String, String)).self) { group in
            for (i, c) in calls.enumerated() {
                group.addTask { (i, await self.dispatchToolCall(c, kernel: kernel)) }
            }
            var indexed: [(Int, (String, String))] = []
            for await r in group { indexed.append(r) }
            return indexed.sorted { $0.0 < $1.0 }.map { $0.1 }
        }
    }

    /// Render one OpenAI-shaped tool-call chunk back into the `<tool_call>`
    /// text envelope for persistence in the assistant turn.
    private func renderToolCallChunk(_ call: JSON) -> String {
        let fn = call["function"]
        let name = fn["name"].asString ?? "send"
        let rawArgs = fn["arguments"]
        let args: JSON
        if let s = rawArgs.asString {
            args = (try? JSON.parse(s)) ?? .object([:])
        } else if rawArgs.asObject != nil {
            args = rawArgs
        } else {
            args = .object([:])
        }
        return renderToolCall(name: name, arguments: args)
    }

    /// Project the internal message list to what the model reads — pure text,
    /// every role a chat template renders. Tool replies are stored as
    /// `role:tool` (so projection/recall tool-pairing stays intact) but mapped
    /// to `role:user` here, because many templates (incl. tiny local models)
    /// don't render a `tool` role. Keeps only role + content.
    private func renderForModel(_ messages: [JSON]) -> [JSON] {
        messages.map { m in
            let role = m["role"].asString ?? ""
            let outRole = role == "tool" ? "user" : role
            return .object(["role": .string(outRole), "content": m["content"]])
        }
    }

    /// Emit the terminal `done` event. On the error path, `accumulated`
    /// is included only when the backend historically did so (NIM).
    private func emitDone(
        kernel: Kernel,
        agentId: AgentId,
        streamId: String,
        messageId: String,
        clientId: String,
        accumulated: String,
        error: String?
    ) async {
        var event: OrderedDictionary<String, JSON> = [:]
        event["type"] = .string("done")
        event["stream_id"] = .string(streamId)
        event["message_id"] = .string(messageId)
        if error == nil || config.includeAccumulatedOnError {
            event["accumulated"] = .string(accumulated)
        }
        event["client_id"] = .string(clientId)
        if let error = error {
            event["error"] = .string(error)
        }
        await kernel.emit(agentId, .object(event))
    }

    // MARK: - history / interrupt

    private func historyVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        let clientId = payload["client_id"].asString ?? "cli"
        let messages = await loadHistory(agent: agent, client: clientId, kernel: kernel)
        return .object([
            "messages": .array(messages),
            "client_id": .string(clientId),
        ])
    }

    private func interruptVerb(payload: JSON) -> JSON {
        cancelLock.lock()
        interruptEpoch &+= 1
        cancelLock.unlock()
        _ = payload  // client_id reserved for per-stream cancel later
        return .object(["interrupted": .bool(true)])
    }

    // MARK: - history row shapes

    private func userTurn(id: String, text: String) -> JSON {
        var row: OrderedDictionary<String, JSON> = [:]
        if config.stateless { row["id"] = .string(id) }
        row["role"] = .string("user")
        row["content"] = .string(text)
        row["complete"] = .bool(true)
        return .object(row)
    }

    private func assistantTurn(id: String, content: String) -> JSON {
        var row: OrderedDictionary<String, JSON> = [:]
        if config.stateless { row["id"] = .string(id) }
        row["role"] = .string("assistant")
        row["content"] = .string(content)
        row["complete"] = .bool(true)
        return .object(row)
    }

    // MARK: - history persistence (routed through file_bridge_id)

    /// The bound file_bridge agent id, if the operator (ollama / NIM) or
    /// the backend's own boot (FM) wired one. No id ⇒ no persistence
    /// (history stays empty across turns) — the canonical "no provider ⇒
    /// nothing persisted", NOT a fallback.
    private func fileBridgeId(_ agent: Agent) -> String? {
        agent.metaValue(forKey: "file_bridge_id")?.asString
    }

    /// Trim + sanitise a client id so it's safe as a filename suffix.
    /// Mirrors Rust `safe_client` / Python `_safe_client`.
    func safeClient(_ clientId: String) -> String {
        let trimmed = clientId.trimmingCharacters(in: .whitespaces)
        let raw = trimmed.isEmpty ? "cli" : trimmed
        let mapped = raw.map { c -> Character in
            (c.isLetter || c.isNumber || c == "." || c == "_" || c == "-") ? c : "_"
        }
        let out = String(mapped.prefix(64))
        return out.isEmpty ? "cli" : out
    }

    /// Per-client chat thread, STORE-RELATIVE (`agents/<id>/…`) so wiring
    /// `file_bridge_id` to the `.fantastic` store lands the sidecar next
    /// to the agent's own record. Matches Rust/Python `chat_path`.
    private func chatPath(_ id: AgentId, _ client: String) -> String {
        "agents/\(id.value)/chat_\(safeClient(client)).json"
    }

    /// Load a client's persisted chat via the bound file_bridge. Empty on
    /// no provider / missing file / unparseable.
    func loadHistory(agent: Agent, client: String, kernel: Kernel) async -> [JSON] {
        guard let fid = fileBridgeId(agent) else { return [] }
        let r = await kernel.send(
            AgentId(fid),
            .object(["type": .string("read"), "path": .string(chatPath(agent.id, client))]))
        guard let content = r["content"].asString,
            let parsed = try? JSON.parse(content),
            let arr = parsed.asArray
        else { return [] }
        return arr
    }

    /// Persist a client's full chat via the bound file_bridge. No-op when
    /// no provider is wired (matches Rust's ignore-on-unset).
    private func saveHistory(agent: Agent, client: String, kernel: Kernel, rows: [JSON]) async {
        guard let fid = fileBridgeId(agent) else { return }
        _ = await kernel.send(
            AgentId(fid),
            .object([
                "type": .string("write"),
                "path": .string(chatPath(agent.id, client)),
                "content": .string(JSON.array(rows).serialize()),
            ]))
    }

    // MARK: - cancellation (epoch-bump) + in-flight

    /// Snapshot the current epoch — call at the top of a stream so the
    /// stream's birth epoch is fixed before the first cancel poll.
    public func currentEpoch() -> UInt64 {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        return interruptEpoch
    }

    /// True iff the stream should bail: its id was explicitly cancelled,
    /// or an interrupt fired after it started (epoch advanced).
    public func isStreamCancelled(streamId: String, startedAt: UInt64) -> Bool {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        return cancelledStreams.contains(streamId) || startedAt < interruptEpoch
    }

    private func clearStreamCancel(_ id: String) {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        cancelledStreams.remove(id)
    }

    private func bumpInFlight(_ delta: Int) {
        stateLock.lock()
        defer { stateLock.unlock() }
        inFlight = max(0, inFlight + delta)
    }

    /// Current in-flight stream count (for `backend_state` reporting).
    public func readInFlight() -> Int {
        stateLock.lock()
        defer { stateLock.unlock() }
        return inFlight
    }
}
