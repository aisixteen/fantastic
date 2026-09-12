// AIBackend — the shared reflect-driven LLM-agent machinery behind the
// `AIProvider` seam. The ollama / NIM / Apple-FM bundles supply only
// their `AIProvider` impl + an `AIBackendConfig`, and dispatch every
// verb through the `handle` method here.
//
// Extracted from the three (formerly duplicated) Swift backends. The
// shared layer owns: per-(agent, client_id) chat history, the
// epoch-bump cancellation state, the verb bodies (reflect / boot /
// send / history / status / recall / …), the spawned
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
//   history → {rows, next, total}
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

    /// In-flight stream count (bumped around every pass).
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
    /// Agents whose next turn must open with `context_recall` (enough folds
    /// landed) + the fold count since the last automatic recall. Python
    /// `_recall_due` / `_folds_since`.
    var recallDue: Set<String> = []
    var foldsSince: [String: Int] = [:]
    /// Where the last `assemble` took its view from — "array" or "store…".
    /// Diagnostics only. Python `_view_source`.
    var viewSource: [String: String] = [:]


    init(config: AIBackendConfig) {
        self.config = config
    }

    /// Get (or lazily create) the coordination state for an agent. Internal, not
    /// private: the projection seam (ContextProjection.swift, same type,
    /// different file) reads it.
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
            // On boot, an agent with a store REBUILDS ITS VIEW — `context_recall`.
            // A fresh agent has nothing to recall and returns nothing. The
            // incarnation is bumped FIRST: a reply dispatched under the previous
            // one must not become a live turn of this one. Python `_make_boot`.
            AIIncarnation.bump(agent.id.value)
            if loadHistory(agent: agent, kernel: kernel).isEmpty { return nil }
            let provider = await sideProvider(agent: agent, kernel: kernel)
            return await contextRecall(
                agent: agent, payload: .object([:]), kernel: kernel, provider: provider)
        case "shutdown":
            // A lifecycle no-op, never advertised: the AI is told by
            // `shutdown_agents` only when its reflect map carries the verb.
            return .null
        case "send":
            return await sendVerb(agent: agent, payload: payload, kernel: kernel)
        // ONE INQUIRY VERB, WHOEVER RECEIVES IT: a person answers `inquiry` from a
        // record, this agent answers it by THINKING, and the asker writes the
        // same call either way.
        case "inquiry":
            let opened = inquiryOpen(payload)
            if let refusal = opened.refusal { return refusal }
            let out = await sendVerb(agent: agent, payload: opened.send!, kernel: kernel)
            return inquiryClose(out, payload)
        case "history":
            return historyVerb(agent: agent, payload: payload, kernel: kernel)
        case "recall":
            return recallVerb(agent: agent, payload: payload, kernel: kernel)
        // FULL ASYNC: page a stored reply body by pid, or FIND the line first.
        case "pull":
            return pullVerb(agent: agent, payload: payload, kernel: kernel)
        case "grep":
            return grepVerb(agent: agent, payload: payload, kernel: kernel)
        case "context_status":
            return await contextStatusVerb(agent: agent, kernel: kernel)
        // Rebuild the view from the store: on boot, after folds, on request.
        case "context_recall":
            let dry = payload["dry"].asBool ?? false
            let provider = dry ? nil : await sideProvider(agent: agent, kernel: kernel)
            return await contextRecall(
                agent: agent, payload: payload, kernel: kernel, provider: provider)
        case "status":
            return statusVerb(agentId: agentId, payload: payload)
        default:
            // Python's `f"{name}: unknown type {t!r}"` — the bundle name, the
            // verb repr'd, `None` when the payload named no type.
            return .object([
                "error": .string(
                    "\(config.kind): unknown type \(payload["type"] == .null ? "None" : pyRepr(verb))")
            ])
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
        guard let entry = st.takeCurrentForCancel(taskId: String(taskId.value)) else { return }
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

    // MARK: - reflect

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
        fields["verbs"] = .object(Self.VERBS)
        // A generation IS its kernel dispatch task; python names the way to kill
        // it as a top-level field, not a verb.
        fields["cancel"] = .string(CANCEL_DOC)
        return .object(fields)
    }

    /// Python's `reflect.verbs` for every ai_core backend — the FIRST LINE of
    /// each verb's docstring, in `_build`'s order, verbatim. One map, every
    /// backend: the wording never differed per provider in python.
    static let VERBS: OrderedDictionary<String, JSON> = [
        "reflect": .string("Identity + model + endpoint + generating flag + context posture. No args."),
        "send": .string("args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller — cli (stdout) for client_id='cli', or the browser tab whose WS is subscribed and filters by client_id otherwise. Persists per-client chat.json. Per-backend FIFO lock: concurrent callers serialize. If the lock is held when this call arrives, emits a `status` event (phase='queued', detail.ahead) for the caller; first `token` (or `status` of phase != queued) for the same client_id implicitly unqueues. Optional system_prompt:str REPLACES the auto-built prompt (caller-supplied role/context — read it from a state agent yourself, no yaml coupling here); optional messages:list REPLACES persisted history (fully stateless). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Optional send_to:str hands this pass's TEXT to that agent id as a fresh turn, once. The model routes its own work with `<reply_to>` inside its calls. Returns {response, final, client_id}."),
        "inquiry": .string("args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?, client_id:str?. Sends THIS agent an inquiry and returns {answer, request_id} — the answer is the one it emits with the `answer` tool. The same verb a human answers; a sender writes one call for either."),
        "history": .string("args: limit:int? (default: as many as fit inline), after:str? (a row id — page forward from it). The conversation at DIGEST resolution — {id, parent, kind, role, head, bytes} per row, oldest first, paged. Bodies are behind `pull`; a row's text is behind `recall`. Returns {rows, next, total}."),
        "status": .string("args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}."),
        "context_status": .string("No args. The context-budget posture + the last overflow projection + the model's"),
        "recall": .string("args: query?:str (case-insensitive substring over the serialized turn — matches tool args/replies too), limit?:int (default 20, max 100), before?:int (store index — page backward). Pages turns back from the DURABLE store (the FULL conversation, never trimmed — so anything compaction dropped is one call away). Read-only. A page always fits INLINE (the cap); trimmed from the oldest end when it would not, with the index to continue from. Returns {messages:[{index,role,content}], total, truncated, note?}."),
        "pull": .string("args: pid:str (req — the pid on a settled reply's tag), offset:int? (0), length:int? (chars; default and max = your inline cap). Reads ONE PAGE of a reply body that was too large to inline. Returns {pid, offset, text, next_offset, eof} — page with `offset: next_offset` until `eof`."),
        "grep": .string("args: query:str (req — case-insensitive substring), ids:list[int]? (row ids / pids / fold ids to search; default: every row of this conversation and every stored reply body it received), limit:int? (matches, default: as many as fit inline), width:int? (chars of each matched line returned, from its start; default 200 — raise it when the value you need sits deep in a long line). Finds the LINE that mentions something, wherever it is — a stored body you never pulled, a digest line, an old turn. Returns {matches:[{id, line, text}], total, truncated}."),
        "context_recall": .string("args: dry:bool? (mechanical fit only — no model call). Rebuild this agent's VIEW from its store: fit the head to the budget at digest resolution, then ask the model which rows it wants in full, persist the id list, log the decision as a row, and drop RAM scratch. Runs on boot, after a fold batch, or on request. Returns {kept, expanded, dropped, budget, why, degraded}."),
        "boot": .string("On boot, an agent with a store REBUILDS ITS VIEW — `context_recall`. A fresh"),
    ]

/// The outcome of ONE provider pass.
///
/// `send` answers with the finished text on every runtime, so the reason a pass
/// did NOT finish has to travel back with it rather than being flattened to a
/// missing value.
enum AIPassOutcome {
    /// The RAW completion — what routing parses — and the assistant row's id,
    /// the batch token every reply tag of this turn names (`↑<turn>`).
    case ok(String, Int64)
    /// Terminal failure (timeout, provider error, context failsafe). The string
    /// is the message `send` reports as `{error: …}`.
    case failed(String)
    /// The KERNEL killed the task. The turn was already closed out for the
    /// client by `onCancel`; nothing more should be emitted or routed.
    case cancelled
}

    // MARK: - send

    /// Open an `inquiry`: validate it and render it as the `send` that runs.
    ///
    /// ONE INQUIRY VERB, WHOEVER RECEIVES IT. A person answers `inquiry` from a record
    /// and a peer AI answers it by THINKING; the sender writes the same call
    /// either way. Before this an AI sent an inquiry answered `unknown type`, so
    /// "consult whoever can answer" was true only for humans.
    private func inquiryOpen(_ payload: JSON) -> (send: JSON?, refusal: JSON?) {
        let q = payload["text"].asString ?? ""
        if q.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return (
                nil,
                .object([
                    "error": .string("inquiry: `text` must be a non-empty string"),
                    "reason": .string("missing_arg"), "arg": .string("text"),
                ])
            )
        }
        // The inquiry IS the turn. `options`/`kind` ride in the text because a
        // model reads prose, not a schema; `action` is carried verbatim so an
        // answering agent can see what it is being asked to endorse.
        var text = q
        if let opts = payload["options"].asArray, !opts.isEmpty {
            text += "\n\nOptions: " + opts.map { $0.asString ?? $0.serialize() }
                .joined(separator: ", ")
        }
        if case .object = payload["action"] {
            text += "\n\nProposed action: " + payload["action"].serialize()
        }
        var sendPayload = payload.asObject ?? [:]
        sendPayload["type"] = .string("send")
        sendPayload["text"] = .string(text)
        return (.object(sendPayload), nil)
    }

    /// Close an `inquiry`: read the answer the model DECIDED on out of its turn.
    ///
    /// Taking the turn's prose would make every stray sentence an answer.
    /// `inquiry_answer` is a control call in the same class as `done`: the model
    /// says what its answer IS, and a turn that never emits one is told so
    /// rather than having its paragraphs harvested.
    private func inquiryClose(_ out: JSON, _ payload: JSON) -> JSON {
        if out["error"].asString != nil { return out }
        let said = extractToolCalls(out["response"].asString ?? "")
            .last { $0.name == "inquiry_answer" }
        guard let said else {
            return .object([
                "error": .string(
                    "you were sent an inquiry and did not answer it: emit "
                        + "<call name=\"inquiry_answer\"><answer>what you concluded"
                        + "</answer></call>"),
                "reason": .string("unhandled"),
                "request_id": payload["request_id"],
            ])
        }
        let answer = said.arguments["answer"].asString ?? ""
        // THE PLACEHOLDER IS NOT AN ANSWER. The refusal above SHOWS the shape,
        // and a model that could not answer once will copy that example back
        // verbatim on the retry — measured 2026-09-04 on peer_review, where
        // "..." was written into review.md as the verdict and scored as a
        // reviewer that had judged. Same trap as `send_binary`'s literal
        // "@body", and it earns the same refusal.
        if answer.trimmingCharacters(in: CharacterSet(charactersIn: " .\t\n")).isEmpty {
            return .object([
                "error": .string(
                    "that is the PLACEHOLDER, not an answer — `answer` came back as "
                        + "\"\(answer)\". Say what you actually concluded."),
                "reason": .string("bad_arg"), "arg": .string("answer"),
                "request_id": payload["request_id"],
            ])
        }
        return .object([
            "answer": .string(answer),
            "request_id": payload["request_id"],
        ])
    }

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
        // `reply_to` inside its `<call>`s. Mirrors python's `send_to`.
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
            taskId: KernelTaskLocals.currentTask.map { String($0.value) })
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
        case .ok(let textOut, let turnId):
            reply = .object([
                "response": .string(textOut),
                "final": .string(textOut),
                "client_id": .string(clientId),
                "turn_id": .integer(turnId),
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
        if case .ok(let textOut, let turnId) = produced {
            let deadline = sendTimeoutSeconds
            // THE COORDINATION STATE, threaded through so `await` can count what
            // is actually pending for this agent — its queue and its FIFO lock.
            let st = runState(agentId.value)
            kernel.spawnTask(agent: agentId, verb: "route_reply") {
                await routeReply(
                    selfId: agentId, kernel: kernel, text: textOut,
                    clientId: clientId, sendTo: sendTo, systemPrompt: systemPrompt,
                    incoming: text,
                    // THE LAYER'S PER-CALL DEADLINE, threaded to every model call
                    // the router dispatches. The kernel defaults nothing.
                    sendTimeout: deadline,
                    runState: st,
                    turnId: turnId)
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

        // A RECALL OWED FROM THE LAST FOLD BATCH runs first, so this turn is
        // assembled over the head the recall chose — python `_recall_due`.
        if takeRecallDue(agentId.value) {
            _ = await contextRecall(
                agent: agent, payload: .object([:]), kernel: kernel, provider: provider)
        }
        // The message list UNDER the FIFO lock (we hold it here), so serialized
        // sends can't race load→save: the system block rebuilt from the live
        // substrate (or the caller's override), the VIEW of the store, the live
        // turn. `storeLen` is the durable store's length behind it.
        //
        // The user turn is NOT persisted here. It is persisted ONCE, with the
        // assistant turn, after the stream completes — matching python
        // (`ai_core.core._save_history`, one call site) and rust
        // (`agent_loop.rs`, one call site). Saving it early made a cancelled or
        // errored turn leave a user row with no answer, which python and rust
        // never produce: there, an interrupted turn persists NOTHING, because
        // the exchange it was part of never happened.
        let (assembled, storeLen) = await assemble(
            agent: agent, userText: userText, systemPrompt: systemPrompt, kernel: kernel)
        var messages = assembled

        // ROW IDS for this turn: the incoming row (a root, or a delivered reply
        // turn that names its parent on its tag) and the assistant row that
        // answers it. The batch token on this turn's reply tags is the assistant
        // row's own id, so `↑` on every reply points at something the model can
        // see. Python `core.py` (`_row_ids_for_incoming` + `_mint_row_id`).
        let (inId, inParent) = rowIdsForIncoming(userText, kernel: kernel)
        let turnId = kernel.rows.mintId()

        // Context-Protocol seam: shape what the MODEL sees this turn to fit the
        // window (prepending the canonical [context-notice]), ONCE at entry.
        // `messages` is the model view; persistence APPENDS this turn to the chat
        // store (the full conversation), so the durable store is never trimmed and
        // the notice is never persisted.
        do {
            switch await projectContext(
                provider: provider, agent: agent, clientId: clientId, messages: messages,
                // THE DURABLE STORE'S LENGTH, not the view's: `messages` carries the
                // deduped/trimmed history, so counting it puts the reaction cursor
                // short by exactly the turns the trimmers dropped.
                storeLen: storeLen,
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
        let newTurns: [Row] = [
            [
                "role": .string("user"), "content": .string(userText),
                "id": .integer(inId), "parent": inParent.map(JSON.integer) ?? .null,
            ],
            [
                "role": .string("assistant"), "content": .string(withoutThinking(accumulated)),
                "id": .integer(turnId), "parent": .integer(inId),
            ],
        ]
        saveHistory(agent: agent, kernel: kernel, newTurns: newTurns)

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
        return .ok(accumulated, turnId)
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
                config.compactPrompt ? SEND_TO_HOWTO_COMPACT : SEND_TO_HOWTO,
            ]
            // The SMALL-window backend skips the context block: it costs more of
            // the window than it saves (see `compactPrompt`).
            if !config.compactPrompt { blocks.append(CONTEXT_HOWTO) }
            // OPT-IN, on the record. Both verbs exist on every AI, but teaching
            // them costs every agent prompt tokens for a discipline most runs
            // never need — and a block like this changes behavior just by being
            // read. Titles matter only when a strategy STRIPS a body; the
            // async/rows block only when replies arrive as rows. Each flag turns
            // its block on INDEPENDENTLY of `context_strategy`, so a comparison
            // can hold the model's behavior fixed and vary one thing.
            let flag = { (k: String) in agent.metaValue(forKey: k)?.asBool ?? false }
            if flag("context_titles") { blocks.append(TITLES_HOWTO) }
            if flag("context_async") { blocks.append(ASYNC_HOWTO) }
        }
        let extra = await config.systemPromptExtra(agent, kernel)
        if !extra.isEmpty { blocks.append(extra) }
        return blocks.joined(separator: "\n\n")
    }

    /// Build the message list: the system block, the MODEL VIEW of the store,
    /// the live user turn. Returns `(messages, storeLen)` — `storeLen` is how
    /// many turns the DURABLE store held when this view was built. The view is
    /// deduped and trimmed, so its length is NOT a store index; `projectContext`
    /// needs the store one to mark where this turn's reaction will land. Python
    /// `_assemble`.
    func assemble(agent: Agent, userText: String, systemPrompt: String?, kernel: Kernel) async
        -> ([JSON], Int)
    {
        let sysContent = await assembleSystemPrompt(
            agent: agent, systemPrompt: systemPrompt, kernel: kernel)
        // The system block is rebuilt on EVERY user turn; the store holds only the
        // user/assistant turns. The howto is not persisted — it always reflects
        // the latest state at send-time.
        var messages: [JSON] = [.object(["role": .string("system"), "content": .string(sysContent)])]
        var storeLen = 0
        do {
            let hist = loadHistory(agent: agent, kernel: kernel)
            storeLen = hist.count
            // THE VIEW ARRAY IS THE STATE. When one exists and every id it names
            // still resolves, the model sees exactly those rows at exactly those
            // resolutions — the same bytes before and after a restart. Otherwise
            // (no array yet, or an id that no longer resolves) the view is the
            // trimmed store, as before: stale whole-state reads deduped and answered
            // reflects reduced to a stub — the model's own distillation carries the
            // facts (see `HistoryView.swift`).
            let view = loadView(agent.id, kernel: kernel)
            var byId: [Int64: JSON] = [:]
            var idx: [Int64: Int] = [:]
            for (i, r) in hist.enumerated() {
                if let id = rowId(r), id != 0 {
                    byId[id] = r
                    idx[id] = i
                }
            }
            let entries = view?["ids"]?.asArray ?? []
            let resolves = !entries.isEmpty
                && entries.allSatisfy { e in e["id"].asInt.map { byId[$0] != nil } ?? false }
            if view != nil && resolves {
                for e in entries {
                    messages.append(applyRes(byId[e["id"].asInt!]!, e["res"].asString ?? "full"))
                }
                // THE ARRAY IS THE HEAD AS OF ITS WRITE; THE LOG KEEPS GROWING. Every
                // row appended since — the model's own turns, the replies that
                // landed — rides behind it in full. Measured 2026-09-06 (python,
                // recall_after_fold): served the array alone, the model never saw the
                // read it had just made and restarted the survey from filler_0, sixty
                // turns in a circle.
                let newest = entries.map { idx[$0["id"].asInt!]! }.max()!
                messages.append(
                    contentsOf: dropAnsweredReflects(dedupeStaleReads(Array(hist[(newest + 1)...]))))
                setViewSource(agent.id.value, "array")
            } else {
                messages.append(contentsOf: dropAnsweredReflects(dedupeStaleReads(hist)))
                setViewSource(agent.id.value, view == nil ? "store" : "store (stale array)")
            }
        }
        messages.append(.object(["role": .string("user"), "content": .string(userText)]))
        return (messages, storeLen)
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

    /// The conversation at DIGEST resolution — {id, parent, kind, role, head,
    /// bytes} per row, oldest first, paged; `{rows, next, total}`. Bodies are
    /// behind `pull`; a row's text is behind `recall`. Python `_make_history`.
    private func historyVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        let rows = loadHistory(agent: agent, kernel: kernel)
        var digest: [JSON] = rows.compactMap { r in
            guard let id = r["id"].asInt else { return nil }
            let content = r["content"].asString ?? ""
            // `kind` FALLS BACK TO `role`: a plain turn carries no kind, and a
            // reader that switches on it must still see what it is. 120
            // CHARACTERS, newlines flattened — a head is for recognising a row,
            // not for reading it.
            return .object([
                "id": .integer(id),
                "parent": r["parent"],
                "kind": r["kind"].asString.map(JSON.string) ?? r["role"],
                "role": r["role"],
                "head": .string(String(content.prefix(120)).replacingOccurrences(of: "\n", with: " ")),
                "bytes": .integer(Int64(content.count)),
            ])
        }
        // python `int(payload.get("limit") or 0)`, a bad value reading as 0.
        let limit = pyIntOr(payload["limit"], 0) ?? 0
        if let after = asMessageId(payload["after"]) {
            // AN UNKNOWN CURSOR PAGES NOTHING, rather than starting over.
            let at = digest.firstIndex { $0["id"].asInt == after }.map { $0 + 1 } ?? digest.count
            digest.removeFirst(at)
        }
        // THE CAP INVARIANT: a digest page must answer UNDER the inline cap or it
        // is stored and dehydrated and the model pulls its own history. `limit`
        // bounds the count; the cap bounds the bytes.
        let (kept, truncated) = AIContext.fitReply(
            digest, capTokens: AIContext.inlineCap(agent), limit: limit > 0 ? limit : nil)
        return .object([
            "rows": .array(kept),
            "next": truncated ? (kept.last?["id"] ?? .null) : .null,
            "total": .integer(Int64(rows.count)),
        ])
    }

    // MARK: - the conversation: rows in the KERNEL's message store

    /// The kernel's own bookkeeping kinds. In the same table, never the
    /// conversation. WHAT IS NOT CONVERSATION: an allow-list of kinds to HIDE, so
    /// a kind added to the kernel and forgotten here lands in the model's prompt
    /// — which is how `settle` first arrived as a turn reading `null` (python,
    /// 2026-09-11). Python `_BOOKKEEPING`, rust `history.rs`.
    static let bookkeeping: Set<String> = ["call", "task", "body", "view", "settle"]

    /// Trim + sanitise a client id — the status/queue axis, never a store key.
    /// Python `_safe_client`: `(client_id or "cli").strip() or "cli"`, then every
    /// char that is not alphanumeric or `._-` becomes `_`. No length cap.
    func safeClient(_ clientId: String) -> String {
        let trimmed = clientId.trimmingCharacters(in: .whitespacesAndNewlines)
        let raw = trimmed.isEmpty ? "cli" : trimmed
        return String(
            raw.map { c -> Character in
                (c.isLetter || c.isNumber || c == "." || c == "_" || c == "-") ? c : "_"
            })
    }

    /// The agent's conversation — every row, in order — from the KERNEL ROW
    /// STORE. AGENT ID IS CONVERSATION ID: a second conversation is a second
    /// agent, so there is no client dimension here. Nothing is read from disk:
    /// persistence of the store is the kernel's concern (a loader hydrates it, a
    /// persister tails it), never this bundle's. Python `_load_history`.
    func loadHistory(agent: Agent, kernel: Kernel) -> [JSON] {
        kernel.rows.forAgent(agent.id).filter { r in
            guard let k = r["kind"]?.asString else { return true }
            return !Self.bookkeeping.contains(k)
        }.map { JSON.object($0) }
    }

    /// APPEND this turn's rows to the kernel row store. Bookkeeping from above —
    /// the same standing as the ledger's own put; no grant, no gate, no disk. A
    /// pre-minted `id` is accepted, which is what lets a turn name itself in the
    /// reply tags it sends before its own row exists. Python `_save_history`.
    func saveHistory(agent: Agent, kernel: Kernel, newTurns: [Row]) {
        for turn in newTurns {
            do { try kernel.rows.put(agent.id, turn) } catch {
                // A refused row is the hydrator's mistake, not the model's — say
                // it and keep the rest of the turn, rather than dropping a
                // conversation.
                FileHandle.standardError.write(Data("  [history] \(agent.id.value): \(error)\n".utf8))
            }
        }
    }

    /// (id, parent) for an incoming turn. A delivered reply turn names the
    /// assistant turn that made the calls (`↑`) on its first tag; anything else
    /// is a ROOT — a fresh message from a person or a peer. Python
    /// `_row_ids_for_incoming` + `_TAG_LINKS`.
    func rowIdsForIncoming(_ text: String, kernel: Kernel) -> (Int64, Int64?) {
        let rid = kernel.rows.mintId()
        return (rid, incomingParent(text))
    }

    // MARK: - in-flight

    private func bumpInFlight(_ delta: Int) {
        stateLock.lock()
        defer { stateLock.unlock() }
        inFlight = max(0, inFlight + delta)
    }

}

// EVERY ROW HAS AN ADDRESS — an int from the kernel's one counter. `parent` is
// CAUSALITY (the row this one responds to); a thread is the parent chain, derived
// when needed. `reply_to` on a send is DELIVERY and is left alone. Python
// `_TAG_LINKS`: `^\[reply to \S+ · [^\]]*?↑(\S+)[^\]]*\]`.
private let tagLinks = try! NSRegularExpression(
    pattern: "^\\[reply to \\S+ \u{b7} [^\\]]*?\u{2191}(\\S+)[^\\]]*\\]")

/// The `↑<turn>` an incoming turn's FIRST tag names, if it is a delivered reply.
func incomingParent(_ text: String) -> Int64? {
    let ns = text as NSString
    guard let m = tagLinks.firstMatch(in: text, range: NSRange(location: 0, length: ns.length)),
        m.numberOfRanges > 1
    else { return nil }
    return asMessageId(.string(ns.substring(with: m.range(at: 1))))
}
