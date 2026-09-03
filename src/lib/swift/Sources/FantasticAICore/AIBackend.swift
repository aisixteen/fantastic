// AIBackend — the shared reflect-driven LLM-agent machinery behind the
// `AIProvider` seam. The ollama / NIM / Apple-FM bundles supply only
// their `AIProvider` impl + an `AIBackendConfig`, and dispatch every
// verb through the `handle` method here.
//
// Extracted from the three (formerly duplicated) Swift backends. The
// shared layer owns: per-(agent, client_id) chat history, the
// epoch-bump cancellation state, the verb bodies (reflect / boot /
// shutdown / send / history / interrupt / backend_state), the spawned
// streaming task, the token / done event emission, and — since the
// dispatch phase — READING THE MODEL'S TOOL CALLS OUT OF ITS OWN TEXT
// AND DISPATCHING THEM (`ToolParse` + `Routing`). The providers own
// only the upstream wire (NDJSON, SSE+429+Bearer, on-device session).
//
// ONE pass per `send`, and the chain is the MODEL's: after the pass, its
// completion is parsed and every call it wrote is dispatched, with each reply
// delivered to that call's own `reply_to` as a fresh turn. `reply_to:'self'`
// (the default when the key is absent) is what continues a chain — one turn at
// a time, and only because the model asked again. Emitting NO call ends the
// turn, so stop stays free. This is NOT a hard-coded loop: nothing here decides
// how many turns a task deserves, and there is no strategy in the prompt.
// Tool-calling is RAW prompt-and-parse throughout — no provider's native tool
// API is ever used (see `AIProvider`).
//
// Wire contract (byte-identical to the Python reference, enforced by
// FantasticParityTests):
//
//   send → {response, final, client_id}   (py/rust parity; the pass runs
//                                          INLINE. `{error, client_id}`
//                                          when it does not finish.)
//   token event → {type:token, stream_id, message_id, delta,
//                  accumulated, client_id}
//   done  event → {type:done, stream_id, message_id, accumulated?,
//                  client_id, error?}
//   history → {messages, client_id}
//   interrupt → {interrupted:true}
//
// `accumulated` on both the token and done events is the RAW completion — the
// exact concatenation of the deltas already streamed, `<think>…</think>` markers
// included. The DURABLE assistant turn is the STRIPPED text (see
// `Thinking.swift`): one stream, two uses, and the event field stays equal to
// what the client received.
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

/// What every backend advertises INSTEAD of an `interrupt` verb — the reflect
/// surface that teaches a caller how to kill a turn. Byte-identical to Rust's
/// `fantastic_ai_core::CANCEL_DOC`.
public let CANCEL_DOC: String = """
    No `interrupt` verb — an in-flight `send` IS a kernel task. \
    Kill it with the substrate's universal system verbs: send 'kernel' {type:'ps'} for the live \
    task table, find the row {agent:'<this agent>', verb:'send'}, then send 'kernel' \
    {type:'cancel', task_id:'<agent>:<n>'}. The turn closes with status(phase='done', \
    reason='cancelled') + done, its HTTP stream and FIFO lock are released, and the blocked \
    caller's `send` resolves to {cancelled:true}. Cancel kills the task's whole SUBTREE, so any \
    work the turn spawned dies with it.
    """

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

    /// In-flight stream count for `backend_state` reporting.
    private let stateLock = NSLock()
    private var inFlight: Int = 0

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

    /// Get (or lazily create) the coordination state for an agent. Internal, not
    /// private: `recallVerb` (ContextProjection.swift, same type, different file)
    /// reads the in-flight entry's client to resolve a bare `recall`.
    func runState(_ id: String) -> AIAgentRunState {
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
        case "backend_state":
            return backendStateReply(agent: agent)
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    /// The `onCancel` bundle hook, shared by every backend: the kernel is killing
    /// `taskId` and will cancel this generation's Swift task the moment we
    /// return.
    ///
    /// The cancellation itself tears the provider stream down (the `AsyncStream`
    /// finishes, its `onTermination` cancels the upstream HTTP task) and the
    /// generation releases its FIFO guard as it unwinds. What NEITHER does is
    /// close the turn out for the caller. So that is exactly what happens here,
    /// while the task is still alive: clear the in-flight entry (`status` would
    /// otherwise report a phantom generation forever) and emit the final
    /// `status(done, reason:"cancelled")` + `done` the client is waiting on.
    ///
    /// No-op when the cancelled task is some OTHER dispatch on this agent (a
    /// `history`, a `reflect`) — the entry is matched on its task id.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async {
        let st = runState(agentId.value)
        guard let entry = st.takeCurrentForCancel(taskId: taskId.value) else { return }
        await emitStatus(
            agentId: agentId, clientId: entry.clientId, phase: "done", st: st,
            extraDetail: [
                "reason": .string("cancelled"),
                "send_id": .string(entry.sendId),
            ],
            kernel: kernel)
        await emitDone(
            kernel: kernel, agentId: agentId,
            streamId: entry.streamId, messageId: entry.messageId,
            clientId: entry.clientId, accumulated: entry.textSoFar,
            error: "cancelled")
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
            "args: client_id?, query? (substring), limit? (max 100), before?. Pages turns back from the durable store (lossless on demand after compaction). Returns {messages, total, truncated, client_id}."
        )
        verbs["context_status"] = .string(
            "No args. Context-budget posture + last compaction + derived reaction. Returns {context_window, output_reserve, budget, strategy, last_projection, last_reaction}."
        )
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

/// The outcome of ONE provider pass.
///
/// `send` answers with the finished text on every runtime, so the reason a pass
/// did NOT finish has to travel back with it rather than being flattened to a
/// missing value.
enum AIPassOutcome {
    /// The RAW completion — what routing parses.
    case ok(String)
    /// Terminal failure (timeout, provider error, context failsafe). The string
    /// is the message `send` reports as `{error: …}`.
    case failed(String)
    /// The KERNEL killed the task. The turn was already closed out for the
    /// client by `onCancel`; nothing more should be emitted or routed.
    case cancelled
}

    // MARK: - send

    private func sendVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        // MISSING `text` IS THE EMPTY STRING, not a refusal: python's
        // `payload.get("text", "")` and rust's `unwrap_or("")` both run the turn, so
        // a caller-driven agent kicking off from its system prompt alone worked on
        // two kernels and was refused on the third with a reason they never produce.
        let text = payload["text"].asString ?? ""
        // SANITISED AT ENTRY, as python's `_send` does. The queue/current entry held
        // the RAW id while `statusVerb` compared against `safeClient(requesting)`, so
        // a browser client `tab 1/x` asking after its OWN work was told `is_mine:false,
        // mine_pending:[], others_pending:1`.
        let clientId = safeClient(payload["client_id"].asString ?? "cli")
        // A caller-supplied `system_prompt` REPLACES the default system block
        // (the caller teaches its own role/context there). Mirrors py/rust.
        let systemPrompt = payload["system_prompt"].asString
        // Where this pass's TEXT goes when the model issues no call of its own.
        // A one-shot handoff for the CALLER; the MODEL routes its own work with
        // `reply_to` inside its `<tool_call>`s. Mirrors python's `send_to`.
        let sendTo: String? = {
            guard let s = payload["send_to"].asString,
                !s.trimmingCharacters(in: .whitespaces).isEmpty
            else { return nil }
            return s
        }()

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
        // also keeps the chat-store-persisted history race-free.
        let sendId = mintSendId()
        let st = runState(agent.id.value)
        st.enqueue(
            AIQueuedEntry(
                clientId: clientId, text: text, sendId: sendId, queuedAt: nowSecs()))

        // Contention probe (best-effort, like Rust's try_lock): if a generation
        // already holds the lock, tell the caller it's queued. ONE event, and it is
        // the `status` one — a bare `{type:'queued'}` rode ahead of it on rust/swift
        // until 2026-09-02, an undocumented type python never sends, which broke any
        // per-submission event count and every cross-runtime event-stream diff.
        if await st.fifo.busy() {
            let ahead = max(0, st.queueDepth() - 1)
            await emitStatus(
                agentId: agent.id, clientId: clientId, phase: "queued", st: st,
                extraDetail: ["send_id": .string(sendId), "ahead": .integer(Int64(ahead))],
                kernel: kernel)
        }

        bumpInFlight(+1)
        let agentId = agent.id

        // RUN THE GENERATION INLINE — python/rust parity.
        //
        // This dispatch used to spawn the generation as its own task and ack
        // `{queued:true, stream_id, message_id}` immediately, while python and
        // rust both run the pass here and answer `{response, final, client_id}`.
        // One verb, two contracts: a caller that reads `reply["response"]` got
        // the text on two kernels and an empty ack on the third, so anything
        // driving swift had to know it was driving swift.
        //
        // Cancellation stays the KERNEL's, exactly as in rust: this dispatch IS
        // the task row, so `ps` sees it and `cancel <task_id>` drops this future,
        // releasing the provider stream and the FIFO guard.
        await st.fifo.acquire()
        // Killed while queued: never became the current turn (so `onCancel` has
        // nothing to close out) — drop the queue row and hand the lock on.
        if Task.isCancelled {
            st.dropQueued(sendId: sendId)
            bumpInFlight(-1)
            await st.fifo.release()
            return .object([
                "error": .string("send: cancelled"), "client_id": .string(clientId),
            ])
        }
        st.popToCurrent(
            sendId: sendId, startedAt: nowSecs(), phase: "thinking",
            streamId: streamId, messageId: messageId,
            // The task we are running AS — how `onCancel` recognises its own turn.
            taskId: KernelTaskLocals.currentTask?.value)
        await emitStatus(
            agentId: agentId, clientId: clientId, phase: "thinking", st: st,
            kernel: kernel)
        let produced = await runStream(
            provider: provider,
            streamId: streamId,
            messageId: messageId,
            userMessageId: userMessageId,
            agent: agent,
            clientId: clientId,
            st: st,
            userText: text,
            systemPrompt: systemPrompt,
            kernel: kernel
        )
        // `onCancel` already cleared the entry on the killed path; clearing again
        // is a no-op. The FIFO lock is released either way — a killed generation
        // unwinds through here, it does not strand the agent.
        //
        // The ORDER here is the invariant, not an accident: `runStream` persisted
        // the assistant turn, `clearCurrent` retires the in-flight entry, and
        // `release()` frees the FIFO BEFORE routing is spawned — so a
        // self-addressed follow-up takes the lock immediately and reads a history
        // that already contains the turn it is answering.
        st.clearCurrent()
        await st.fifo.release()

        let reply: JSON
        switch produced {
        case .ok(let textOut):
            reply = .object([
                "response": .string(textOut),
                "final": .string(textOut),
                "client_id": .string(clientId),
            ])
        case .failed(let why):
            reply = .object(["error": .string(why), "client_id": .string(clientId)])
        case .cancelled:
            reply = .object([
                "error": .string("send: cancelled"), "client_id": .string(clientId),
            ])
        }

        // ROUTE this pass's output where it was addressed — only on the ok path.
        // A failed pass is terminal (python's `if not reply.get("error")` gate),
        // and a broken chain must not keep spending turns.
        //
        // DETACHED as its own task row (`verb:"route_reply"`), which is why
        // chaining works: awaiting a self-addressed send inline would keep THIS
        // dispatch from returning until the whole chain finished. `ps` must not
        // label routing as `"send"` either, and the row makes each hop
        // individually visible and killable. NEVER `Task.detached`: that loses
        // the task-locals and the row would land parentless, outside `cancel`'s
        // reach.
        if case .ok(let textOut) = produced {
            let deadline = sendTimeoutSeconds
            kernel.spawnTask(agent: agentId, verb: "route_reply") {
                await routeReply(
                    selfId: agentId, kernel: kernel, text: textOut,
                    clientId: clientId, sendTo: sendTo, systemPrompt: systemPrompt,
                    incoming: text,
                    // THE LAYER'S PER-CALL DEADLINE, threaded to every model call
                    // the router dispatches. The kernel defaults nothing.
                    sendTimeout: deadline)
            }
        }
        return reply
    }

    /// Run ONE provider pass. Returns the RAW completion when the pass finished
    /// OK — that text is what routing parses — and a FAILURE case at every other
    /// exit (cancelled, timed out, provider error, context failsafe), which is how
    /// the caller knows a failed pass is terminal and must not be routed.
    ///
    /// The reason travels WITH the outcome because `send` returns it: python and
    /// rust both answer `{error, client_id}` when a pass does not finish, and a
    /// bare `nil` could not say which of four things happened.
    private func runStream(
        provider: any AIProvider,
        streamId: String,
        messageId: String,
        userMessageId: String,
        agent: Agent,
        clientId: String,
        st: AIAgentRunState,
        userText: String,
        systemPrompt: String?,
        kernel: Kernel
    ) async -> AIPassOutcome {
        let agentId = agent.id
        defer { bumpInFlight(-1) }

        let deadline = Date().addingTimeInterval(sendTimeoutSeconds)

        // Load the prior turns UNDER the FIFO lock (we hold it here), so
        // serialized sends on one client can't race load→save.
        //
        // The user turn is NOT persisted here. It is persisted ONCE, with the
        // assistant turn, after the stream completes — matching python
        // (`ai_core.core._save_history`, one call site) and rust
        // (`agent_loop.rs`, one call site). Saving it early made a cancelled or
        // errored turn leave a user row with no answer, which python and rust
        // never produce: there, an interrupted turn persists NOTHING, because
        // the exchange it was part of never happened.
        //
        // The user row carries `id` only for stateless backends (FM bubble
        // identity); ollama/NIM don't. Stateless keeps history UI-only.
        let prior = await loadHistory(agent: agent, client: clientId, kernel: kernel)
        // A trimmed VIEW, not a rewrite of the store: stale whole-state reads
        // deduped and answered reflects reduced to a stub — the model's own
        // distillation carries the facts forward (see `HistoryView.swift`). The
        // durable sidecar keeps every turn, so the trace stays complete and `recall`
        // pages anything back.
        let modelHistory = config.stateless ? [] : dropAnsweredReflects(dedupeStaleReads(prior))

        // Rebuild the system block every turn from the live substrate (a lean
        // primer + self-reflect + the envelope how-to + the context how-to), OR
        // use the caller's `system_prompt` override verbatim. NO agent menu — the
        // primer's id-index plus `reflect` is how an agent is found. Not persisted.
        let systemContent = await assembleSystemPrompt(
            agent: agent, systemPrompt: systemPrompt, kernel: kernel)
        var messages: [JSON] = [
            .object(["role": .string("system"), "content": .string(systemContent)])
        ]
        messages.append(contentsOf: modelHistory)
        messages.append(
            .object(["role": .string("user"), "content": .string(userText)]))

        // Context-Protocol seam: shape what the MODEL sees this turn to fit the
        // window (prepending the canonical [context-notice]), ONCE at entry.
        // `messages` is the model view; persistence APPENDS this turn to the chat
        // store (the full conversation), so the durable store is never trimmed and
        // the notice is never persisted. Skipped for stateless backends (FM owns its context;
        // modelHistory is empty).
        if !config.stateless {
            switch await projectContext(
                provider: provider, agent: agent, clientId: clientId, messages: messages,
                // THE DURABLE STORE'S LENGTH, not the view's: `messages` carries the
                // deduped/trimmed history, so counting it puts the reaction cursor
                // short by exactly the turns the trimmers dropped.
                storeLen: prior.count,
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
                return .failed(err["error"].asString ?? "context projection error")
            }
        }

        // ONE provider pass — stream tokens straight to the caller. What happens
        // NEXT is the model's: this text is handed to `routeReply`, which
        // dispatches whatever calls it contains. The loop is emergent because
        // nothing here schedules another pass; the model does, by writing a call
        // with a `reply_to`. `renderForModel` projects stored turns to plain text
        // every chat template renders. Mirrors Rust `run_generation` / Python
        // `_run`.
        var accumulated = ""
        var firstToken = true
        do {
            let stream = provider.chat(messages: renderForModel(messages))
            for try await chunk in stream {
                // The KERNEL killed this task (`cancel <task_id>`). `onCancel`
                // already closed the turn out for the client — status(done,
                // reason:"cancelled") + done — while we were still alive. Bail
                // WITHOUT emitting anything more and WITHOUT persisting the
                // partial turn; returning drops the provider stream, whose
                // `onTermination` cancels the upstream HTTP task.
                if Task.isCancelled { return .cancelled }
                if Date() > deadline {
                    await emitStatus(
                        agentId: agentId, clientId: clientId, phase: "done", st: st,
                        extraDetail: ["reason": .string("timeout")], kernel: kernel)
                    await emitDone(
                        kernel: kernel, agentId: agentId,
                        streamId: streamId, messageId: messageId,
                        clientId: clientId, accumulated: accumulated,
                        error: "send: timeout after \(Int(sendTimeoutSeconds))s")
                    return .failed("send: timeout after \(Int(sendTimeoutSeconds))s")
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
                }
            }
        } catch {
            // A cancelled task's stream throws as it is torn down — that is our
            // OWN kill, not a provider failure. `onCancel` already emitted the
            // terminal events; stay silent.
            if Task.isCancelled { return .cancelled }
            await emitStatus(
                agentId: agentId, clientId: clientId, phase: "done", st: st,
                extraDetail: ["reason": .string("error")], kernel: kernel)
            await emitDone(
                kernel: kernel, agentId: agentId,
                streamId: streamId, messageId: messageId,
                clientId: clientId, accumulated: accumulated,
                error: "\(error)")
            return .failed("\(error)")
        }
        // The stream can also FINISH (rather than throw) when the task is
        // cancelled — same contract: the turn is already closed out, persist
        // nothing.
        if Task.isCancelled { return .cancelled }

        // APPEND this send's user + assistant turns to the durable chat store.
        // NOT the projected view, and NOT the whole conversation re-written: the
        // store owns the full transcript and each turn is one `append`. THE ONLY
        // WRITE, and the only one python/rust make either. The error /
        // FM-interrupt / timeout / cancel paths all return before this point,
        // so an interrupted turn persists nothing at all — neither half of an
        // exchange that never completed.
        //
        // PARSE THE RAW TEXT, PERSIST THE STRIPPED TEXT (Python core.py:1016-1018).
        // The deliberation is not part of the conversation: re-feeding a reasoning
        // model its own prior thought is against every vendor's guidance and it
        // compounds turn over turn. The RAW text (markers in) is what a reader
        // parses for tool calls — a call the model wrote mid-thought still counts —
        // and it is what the `token`/`done` events already carried.
        let newTurns = [
            userTurn(id: userMessageId, text: userText),
            assistantTurn(id: messageId, content: withoutThinking(accumulated)),
        ]
        await saveHistory(agent: agent, client: clientId, kernel: kernel, newTurns: newTurns)

        await emitStatus(
            agentId: agentId, clientId: clientId, phase: "done", st: st,
            extraDetail: ["reason": .string("ok")], kernel: kernel)
        await emitDone(
            kernel: kernel, agentId: agentId,
            streamId: streamId, messageId: messageId,
            clientId: clientId, accumulated: accumulated, error: nil)

        // The RAW completion — markers in — is what routing parses: a call the
        // model wrote mid-thought still dispatches. Only the DURABLE turn above
        // is stripped.
        return .ok(accumulated)
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

    // MARK: - prompt assembly

    /// Build the DEFAULT system prompt from the live substrate: a lean primer
    /// (id-index of the tree + bundle catalog), the agent's own self-reflect, the
    /// envelope how-to (`SEND_TO_HOWTO` — how to reach another agent, and where
    /// its answer goes), the context how-to, and any per-backend extra (FM's
    /// always-inject memory). A caller-supplied `systemPrompt` REPLACES the
    /// default (the caller supplies its own role/context); the per-backend extra
    /// is still appended. NO agent menu, and no STRATEGY — the two how-tos state
    /// mechanisms only, which `promptStatesTheMechanismNotAStrategy` pins.
    /// Mirrors Rust `assemble_messages` / Python `_assemble`.
    func assembleSystemPrompt(agent: Agent, systemPrompt: String?, kernel: Kernel) async -> String {
        var blocks: [String]
        if let override = systemPrompt,
            !override.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            blocks = [override]
        } else {
            let selfId = agent.id
            let primer = await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("reflect"),
                    "tree": .string("ids"),
                    "bundles": .string("ids"),
                ]))])[0].result
            let me = await kernel.send([Item(selfId, .object(["type": .string("reflect"), "tree": .string("none")]))])[0].result
            blocks = [
                renderReflect(primer),
                "You are `\(selfId.value)`. " + renderReflect(me),
                SEND_TO_HOWTO,
                CONTEXT_HOWTO,
            ]
        }
        let extra = await config.systemPromptExtra(agent, kernel)
        if !extra.isEmpty { blocks.append(extra) }
        return blocks.joined(separator: "\n\n")
    }

    /// Project the internal message list to what the model reads — pure text,
    /// every role a chat template renders. Tool replies (a caller may persist
    /// `role:tool` turns of its own) are mapped to `role:user` here, because
    /// many templates (incl. tiny local models) don't render a `tool` role.
    /// Keeps only role + content.
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

    // MARK: - history

    private func historyVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        // Sanitised at entry, like `send` — the sidecar is keyed by the SAFE id, so
        // reading with the raw one answered from a thread that never existed.
        let clientId = safeClient(payload["client_id"].asString ?? "cli")
        let messages = await loadHistory(agent: agent, client: clientId, kernel: kernel)
        return .object([
            "messages": .array(messages),
            "client_id": .string(clientId),
        ])
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

    // MARK: - the chat sidecar (append-only JSONL on the AI's own leg)

    /// Trim + sanitise a client id so it is safe as a filename suffix.
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

    /// The chat sidecar's path on the agent's OWN file leg — one JSONL file per
    /// client, one JSON line per turn. The encoding is the AI core's PRIVATE
    /// choice as a consumer; the substrate imposes no format on anyone.
    /// Mirrors python `_chat_path` (durable_memory is gone; memory is files).
    private func chatPath(agentId: String, client: String) -> String {
        "agents/\(agentId)/chat_\(safeClient(client)).jsonl"
    }

    private func chatLeg(_ agent: Agent) -> String? {
        agent.metaValue(forKey: "file_bridge_id")?.asString.flatMap { $0.isEmpty ? nil : $0 }
    }

    /// The client's conversation, parsed line-by-line off the JSONL sidecar.
    /// Empty on a missing leg, a missing file, or unreadable content; a torn
    /// tail line (a crash mid-append) is skipped rather than poisoning the
    /// whole history.
    ///
    /// BOOKKEEPING, PROCESSED FROM ABOVE — the agent does not choose to have a
    /// conversation log. Confused-deputy-safe: the target is the record's OWN
    /// `file_bridge_id`, the verb is literal, the path self-scoped
    /// (`agents/<self>/…`, client sanitized). Mirrors python `_load_history`.
    func loadHistory(agent: Agent, client: String, kernel: Kernel) async -> [JSON] {
        guard let fb = chatLeg(agent) else { return [] }
        let path = chatPath(agentId: agent.id.value, client: client)
        let raw: Data = await kernel.asBookkeeping {
            var acc = Data()
            var offset = 0
            while true {
                let (r, body) = await kernel.sendWithBinary(
                    AgentId(fb),
                    .object([
                        "type": .string("read_stream"), "path": .string(path),
                        "offset": .integer(Int64(offset)),
                    ]), Data())
                guard case .object = r, r["error"].asString == nil else { break }
                if !body.isEmpty { acc.append(body) }
                if r["eof"].asBool ?? true { break }
                if body.isEmpty { break }
                offset = Int(r["next_offset"].asInt ?? Int64(offset + body.count))
            }
            return acc
        }
        guard !raw.isEmpty, let text = String(data: raw, encoding: .utf8) else { return [] }
        var turns: [JSON] = []
        for line in text.components(separatedBy: "\n") {
            let t = line.trimmingCharacters(in: .whitespaces)
            guard !t.isEmpty, let parsed = try? JSON.parse(Data(t.utf8)),
                case .object = parsed
            else { continue }
            turns.append(parsed)
        }
        return turns
    }

    /// APPEND this turn's new messages — one `write_stream` (absent offset =
    /// append-at-end, creates the file) per turn, one JSON line each. The
    /// durable record is never trimmed; the projection only shapes what the
    /// model sees. PROCESSED FROM ABOVE — see `loadHistory`; and it clears the
    /// caller's kill flag too: a cancel must never corrupt what was already
    /// durable. Mirrors python `_save_history`.
    private func saveHistory(
        agent: Agent, client: String, kernel: Kernel, newTurns: [JSON]
    ) async {
        if newTurns.isEmpty { return }
        guard let fb = chatLeg(agent) else { return }
        let path = chatPath(agentId: agent.id.value, client: client)
        for turn in newTurns {
            let line = turn.serialize() + "\n"
            _ = await kernel.asBookkeeping {
                await kernel.sendWithBinary(
                    AgentId(fb),
                    .object(["type": .string("write_stream"), "path": .string(path)]),
                    Data(line.utf8)
                ).0
            }
        }
    }

    // MARK: - in-flight

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
