// AIBackend — the shared reflect-driven LLM-agent machinery behind the
// `AIProvider` seam. The ollama / responses / vllm / Apple-FM bundles supply only
// their `AIProvider` impl + an `AIBackendConfig`, and dispatch every
// verb through the `handle` method here.
//
// Extracted from the three (formerly duplicated) Swift backends. The
// shared layer owns: per-(agent, client_id) chat history, the
// epoch-bump cancellation state, the verb bodies (reflect / boot /
// send / history / status / recall / …), the spawned
// streaming task, the token / status event emission, and — since the
// dispatch phase — READING THE MODEL'S TOOL CALLS OUT OF ITS OWN TEXT
// AND DISPATCHING THEM (`ToolParse` + `Routing`). The providers own
// only the upstream wire (NDJSON, SSE+Bearer, on-device session).
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
// Wire contract (python `ai_core/core.py`):
//
//   send → {response, final, client_id, message_id}   (the pass runs INLINE;
//          `{error, client_id}` for a timeout, the projection's own refusal
//          body verbatim, and a RAISE — settled as
//          `{error:"<Type>: <msg>", raised:true}` — for a provider failure)
//   token event  → {type:'token', text, source, client_id}
//   status event → {type:'status', source, phase, detail, ts, client_id}
//   history → {rows, next, total}
//
// There is no `done` event: `status(phase:'done')` is the completion. Events go
// to the caller only (`toCaller`): a `send` to the `cli` agent for client `cli`,
// an emit on this agent's inbox for anyone else.
//
// The token `text`s are the RAW completion, `<think>…</think>` markers included;
// the DURABLE assistant turn is the STRIPPED text (see `Thinking.swift`).
//
// This module MUST NOT import any provider SDK.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Construct the shared backend machinery for one bundle. Returns an
/// `AIBackend` the bundle's `handle` delegates straight to. Mirrors
/// Rust's `buildAIBackend` / the Python `ai_core` factory.
public func buildAIBackend(_ config: AIBackendConfig) -> AIBackend {
    // The AI core keeps process-wide tables keyed by `Kernel.serial`; this
    // is where their cleanup registers, once, so a kernel that shuts down takes
    // its rows with it.
    AISideTables.installTeardownHook()
    return AIBackend(config: config)
}

/// What every backend's docs say INSTEAD of an `interrupt` verb — served by
/// `docs` (tier detailed). Byte-identical to python's `ai_core.CANCEL`.
public let CANCEL_DOC: String = """
    No `interrupt` verb — an in-flight `send` IS a kernel task. Kill it with the \
    substrate's universal system verbs: send 'kernel' <type>ps</type> for the live task \
    table, find the row {agent:'<this agent>', verb:'send'}, then send 'kernel' \
    <type>cancel</type> <task_id>n</task_id>. The turn closes with status(phase='done', \
    reason='cancelled') + done, its HTTP stream and FIFO lock are released, and the \
    blocked caller's `send` resolves to {cancelled:true}. Cancel kills the task's whole \
    SUBTREE, so any work the turn spawned dies with it.
    """


public final class AIBackend: @unchecked Sendable {
    public let config: AIBackendConfig

    /// This backend's per-`send` ceiling — `AIConstants.SEND_TIMEOUT` unless the
    /// config names another.
    private var sendTimeoutSeconds: Double { config.sendTimeout }

    /// In-flight stream count (bumped around every pass).
    private let stateLock = NSLock()
    private var inFlight: Int = 0

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

    /// The coordination state for an agent of `kernel` — shared by every backend
    /// and the router (see `AIRunStates`), because python's `_queue`/`_current`
    /// are module dicts they all read.
    func runState(_ kernel: Kernel, _ id: String) -> AIAgentRunState {
        AIRunStates.of(kernel, id)
    }

    /// Fractional unix seconds — status timestamps + entry ages.
    func nowSecs() -> Double { Date().timeIntervalSince1970 }

    /// Opaque id for one user submission (status correlation).
    ///
    /// python `secrets.token_urlsafe(8)`: 8 random bytes, base64url, padding
    /// stripped — ELEVEN characters and NO prefix. Swift minted `snd_<8 hex>`,
    /// which no python-shaped consumer (or capture diff) recognises.
    private func mintSendId() -> String {
        var bytes = Data(count: 8)
        for i in 0..<8 { bytes[i] = UInt8.random(in: UInt8.min...UInt8.max) }
        return bytes.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    // MARK: - Verb dispatch

    /// Single entry point — the bundle's `handle` forwards here.
    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent")])
        }
        switch verb {
        case "reflect":
            return reflectReply(agent: agent, kernel: kernel)
        case "boot":
            // LAZY: never touches the memory store. Checks the record names one
            // (fail loud otherwise), starts a new incarnation, and marks the view
            // for rebuilding — the first turn reads the head from the store and
            // runs `context_recall`. The incarnation is bumped FIRST: a reply
            // dispatched under the previous one must not become a live turn of
            // this one. Python `_make_boot`.
            AIIncarnation.bump(kernel, agent.id.value)
            do {
                _ = try storeOf(agent.id.value, kernel)
            } catch let e as MemoryStoreError {
                return .object([
                    "error": .string(e.message), "reason": .string("missing_arg"),
                    "arg": .string("memory_store_id"),
                ])
            }
            // A new incarnation reads its store afresh; bodies are this run's only.
            forgetHead(agent.id.value, kernel)
            markRecallDue(agent.id.value)
            return nil
        case "send":
            return try await sendVerb(agent: agent, payload: payload, kernel: kernel)
        case "stop":
            return await stopVerb(agentId: agent.id, payload: payload, kernel: kernel)
        // What the agent HOLDS in memory — its loaded head; never a store read.
        case "rows":
            return rowsVerb(agent.id.value, payload, kernel)
        // ONE INQUIRY VERB, WHOEVER RECEIVES IT: a person answers `inquiry` from a
        // record, this agent answers it by THINKING, and the asker writes the
        // same call either way.
        case "inquiry":
            return inquiryAccept(agent.id.value, payload, kernel)
        case "history":
            return try await historyVerb(agent: agent, payload: payload, kernel: kernel)
        case "recall":
            return try await recallVerb(agent: agent, payload: payload, kernel: kernel)
        // FIND the line anywhere in memory — rows, shared rows, bodies held this run.
        case "grep":
            return try await grepVerb(agent: agent, payload: payload, kernel: kernel)
        case "context_status":
            return try await contextStatusVerb(agent: agent, kernel: kernel)
        // Rebuild the view from the store: on boot, after folds, on request.
        case "context_recall":
            // PYTHON TRUTHINESS: `dry:1`, `"false"` and `[0]` are all dry.
            // `asBool` took only a JSON `true`, so a model writing `<dry>1</dry>`
            // got a real model call it had asked not to make.
            let dry = pyTruthy(payload["dry"])
            let provider = dry ? nil : await sideProvider(agent: agent, kernel: kernel)
            return try await contextRecall(
                agent: agent, payload: payload, kernel: kernel, provider: provider)
        // A new thread on the same store: the view starts here, nothing is deleted.
        case "context_clear":
            return try await contextClear(agent: agent, payload: payload, kernel: kernel)
        // THE ROW EVENT: this agent announces its own rows to its watchers.
        case "watch":
            return watchVerb(agent: agent, payload: payload, kernel: kernel)
        case "unwatch":
            return unwatchVerb(agent: agent, payload: payload, kernel: kernel)
        case "status":
            return statusVerb(agentId: agentId, payload: payload, kernel: kernel)
        default:
            // Python's `f"{name}: unknown type {t!r}"` — the bundle name and the
            // RAW payload value repr'd: `None` when the payload named no type,
            // `5` for a number, `'shutdown'` for a verb this bundle does not
            // have (its verb map has no `shutdown` entry, so the lifecycle
            // no-op swift answered was a reply python never sends).
            return Refusal.make(
                Refusal.unhandled, "\(config.kind): unknown type \(pyRepr(payload["type"]))",
                extra: [("verb", payload["type"])])
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
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        let st = runState(kernel, agentId.value)
        guard let entry = st.takeCurrentForCancel(taskId: String(taskId.value)) else { return }
        // THE CUT (owner, 2026-10-04): every open part ABORTS for its watchers, and
        // the turn leaves the incoming message + a marker row — never a half answer
        // — so the model's next turn knows what it was asked, what it had visibly
        // said, and who stopped it. Nothing of a stopped pass was dispatched.
        var unsent: [StreamPart] = []
        if let parts = entry.parts {
            unsent = await parts.stop("stopped").filter { $0.kind == "call" }
        }
        let who = entry.stoppedBy ?? "cancel"
        let (inId, inParent) = rowIdsForIncoming(entry.text, kernel: kernel)
        try await saveHistory(
            agentId.value, kernel,
            [
                incomingRow(entry.text, id: inId, parent: inParent, notice: entry.notice),
                .object([
                    "role": .string("assistant"),
                    "content": .string(cutMarker(entry.textSoFar, "stopped by \(who)", unsent)),
                    "id": .integer(kernel.mintId()), "parent": .integer(inId),
                ]),
            ])
        // BOTH CORRELATION FIELDS, explicitly: the entry is already OUT of
        // `_current` by now (that is what stops `status` reporting a phantom
        // generation), so nothing can auto-fill them. `started_at` was missing,
        // and it is the only thing that says how long the killed turn ran.
        await emitStatus(
            kernel: kernel, agentId: agentId, clientId: entry.clientId, phase: "done",
            detail: [
                ("reason", .string("cancelled")),
                ("send_id", .string(entry.sendId)),
                ("started_at", .double(entry.startedAt)),
            ])
    }

    /// args: stream_id:str? (a part of the generation in flight). STOP this
    /// agent's in-flight generation — how a receiver of its streaming parts stops
    /// it: the provider stream closes, open parts abort, the turn keeps a
    /// `[stopped by <you>]` marker. Returns {stopped:bool}. Python `_stop`.
    ///
    /// The stopper is recorded on the in-flight entry and the generation's kernel
    /// task is cancelled — `onCancel` does the rest, as for any `cancel`.
    private func stopVerb(agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let st = runState(kernel, agentId.value)
        guard let cur = st.currentSnapshot(), let tid = cur.taskId else {
            return .object(["stopped": .bool(false)])
        }
        let sid = payload["stream_id"]
        if sid != .null {
            guard let parts = cur.parts, let s = sid.asString,
                await parts.parts().contains(where: { $0.streamId == s })
            else {
                return .object(["stopped": .bool(false)])
            }
        }
        // `current_origin() or "operator"`.
        let origin = KernelTaskLocals.currentOrigin?.value ?? ""
        let who = origin.isEmpty ? "operator" : origin
        st.updateCurrent { if $0.sendId == cur.sendId { $0.stoppedBy = who } }
        _ = await kernel.cancelTask(TaskId(Int64(tid)!))
        return .object(["stopped": .bool(true)])
    }

    /// The `onDelete` bundle hook, shared by every backend (vllm drops its own
    /// table beside it). Drops every row this module keeps for the deleted id,
    /// so an id re-created later starts clean and nothing leaks (audit ARCH2-2):
    /// the FIFO lock and queue, the in-flight entry, the projection and its
    /// marks, the fold and recall counters, the calibration, the silent streaks,
    /// the strip set, and this kernel's head, bodies and watchers.
    /// Python `on_delete`.
    ///
    /// NOT the incarnation: it stays monotonic across a re-create, so a reply to
    /// a call the deleted agent made never becomes a live turn of the new one.
    public func onDelete(agentId: AgentId, kernel: Kernel) async {
        let id = agentId.value
        AIRunStates.forget(kernel, id)
        projectionLock.withLock {
            projectionCache[id] = nil
            compactionMark[id] = nil
            viewSource[id] = nil
            foldsSince[id] = nil
            recallDue.remove(id)
        }
        AICalibration.forget(kernel, id)
        await inFlightTurns.forget(id)
        AISilentStreaks.forget(kernel, agent: id)
        AIStripStrategy.forget(kernel, id)
        AIMemory.forget(kernel, id)
        AIInquiries.forget(kernel, agent: id)
    }

    // MARK: - reflect

    /// PYTHON'S KEYS, IN PYTHON'S ORDER: `id, sentence, model, endpoint,
    /// context_window, context_strategy, verbs, generating, cancel, emits,
    /// concurrency`, then the backend's ordered extras.
    ///
    /// This line is READ BY THE MODEL — `_render_reflect` renders it into the
    /// system prompt's `You are <id>.` line, field by field, in this order. Swift
    /// opened with `kind`/`provider` (keys python does not have), put the extras
    /// before the protocol fields, and carried no `generating` or `concurrency`
    /// at all, so every swift agent's self-description was a different sentence
    /// from python's.
    private func reflectReply(agent: Agent, kernel: Kernel) -> JSON {
        var fields: OrderedDictionary<String, JSON> = [:]
        fields["id"] = .string(agent.id.value)
        fields["sentence"] = .string(config.sentence)
        // KEY PRESENCE, not truthiness: `rec.get("model", default)` returns a
        // `model: null` on the record as null.
        fields["model"] = agent.metaValue(forKey: "model") ?? .null
        fields["endpoint"] = agent.metaValue(forKey: "endpoint") ?? .null
        // Context Protocol: context_window + the RAW context_strategy.
        for (k, v) in contextReflectFields(agent: agent) {
            fields[k] = v
        }
        // A generation IS its kernel dispatch task — the backend holds no task
        // handle of its own. An in-flight entry present ⇔ generating.
        fields["generating"] = .bool(runState(kernel, agent.id.value).currentSnapshot() != nil)
        for (k, v) in config.reflectExtra(agent, kernel) {
            fields[k] = v
        }
        return .object(fields)
    }

    /// The `emits` map, byte-identical to python's. THREE channels, not four:
    /// `done` is not one of them — `status(phase='done')` IS the completion
    /// event, and advertising a second one is two truths for one transition.
    /// What every backend emits — served by `docs` (tier detailed), byte-identical
    /// to python's `ai_core.EMITS`.
    public static let REFLECT_EMITS: OrderedDictionary<String, JSON> = [
        "token": .string(
            "{type:'token', text:str, source, client_id} — streaming chunk, routed ONLY to the caller: cli (stdout) when client_id='cli', else this agent's own inbox (a browser filters by client_id)."
        ),
        "status": .string(
            "{type:'status', source, client_id, ts, phase:'queued'|'thinking'|'streaming'|'done', detail:{send_id, started_at, queue_depth, ...}} — the one channel for phase transitions. On done, detail.reason='ok'|'cancelled'|'timeout'|'error'|'awaiting'|'dead_turn' (only thinking — discarded, rerolled)|'gave_up' (MAX_SILENT_TURNS call-less turns in a row; detail.error says so). On queued, detail.ahead."
        ),
        "context": .string(
            "{type:'context', source, client_id, ts, phase:'compacted'|'too_small', detail} — compacted: {strategy, dropped_messages, kept_messages, summarized}, the view was cut to fit the window; too_small: {context_window, system_tokens, hint}, even the system block + one turn won't fit and the model is NOT called. Pull counterpart: `context_status`."
        ),
    ]

    /// python `ai_core.NOTES` — served by `docs` (tier detailed).
    public static let NOTES: OrderedDictionary<String, JSON> = [
        "concurrency": .string("A FIFO lock around `send`: one generation at a time; other callers wait and get a `queued` status. `reflect`/`history`/`status` skip the lock."),
    ]

    /// Python's `VERBS` docs for every ai_core backend (served by `docs`), WHOLE —
    /// each verb's docstring, in `_build`'s order, verbatim. One map, every
    /// backend: the wording never differed per provider in python.
    public static let VERBS: OrderedDictionary<String, JSON> = [
        "reflect": .string("Identity + model + endpoint + generating flag + context posture. No args."),
        "send": .string("args: text:str (req), client_id:str? (default 'cli'), system_prompt:str?, send_to:str?. Runs one turn on `text` and streams tokens to ONLY the caller — cli (stdout) for client_id='cli', else the browser tab whose WS filters by client_id. The conversation is this agent's own (its memory store). A FIFO lock serializes callers: one that waits gets a `status` event (phase='queued', detail.ahead), and its first `token` (or non-queued `status`) unqueues it. `system_prompt` REPLACES the built prompt for this delivery. `send_to` hands this pass's TEXT to that agent id as a fresh turn, once. `_call_stack` is reserved for the recursion guard. The model routes its own work with `<reply_to>` inside its calls. Returns {response, final, client_id}."),
        "stop": .string("args: stream_id:str? (a part of the generation in flight). STOP this agent's in-flight generation — how a receiver of its streaming parts stops it: the provider stream closes, open parts abort, the turn keeps a `[stopped by <you>]` marker. Returns {stopped:bool}."),
        "rows": .string("args: after:int? (a row id — the page starts after it), limit:int?. A VIEW of this agent's conversation exactly as the agent HOLDS it in memory — its loaded head, in the order it lived it; no store read, no transform, so what a UI draws is what the agent has. Before the agent has loaded its head (a fresh boot, before its first turn) it holds nothing: {rows:[], loaded:false}. Returns {rows, next, total, loaded}."),
        "inquiry": .string("args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?. Sends THIS agent an inquiry: accepted at once ({accepted, re, request_id}); the answer it later gives with `inquiry_answer` — or the news that it ended without one — reaches the asker as a message under the inquiry's pid. The same verb a human answers; a sender writes one call for either."),
        "history": .string("args: fold:int? (a fold or clear id — list ITS members instead), limit:int? (default: as many as fit inline), after:int? (a row id — page forward from it). The conversation at DIGEST resolution — {id, parent, kind, role, head, bytes} per row, oldest first, paged. With `fold`, that fold's title + summary and its members (one level down; a member that is itself a fold opens the same way). A row's text is behind `recall` / `grep`. Returns {rows, next, total, fold?}."),
        "status": .string("args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}."),
        "context_status": .string("No args. The context-budget posture + the last overflow projection + the model's\nderived reaction to it. Returns {context_window, output_reserve, budget, strategy,\nlast_projection:{fired, strategy?, kept_messages?, dropped_messages?, summarized?,\ntoo_small?}|null, last_reaction:{recalled, persisted, recall_count}|null}."),
        "recall": .string("args: query:str? (substring of a serialized turn, tool args and replies included), limit:int? (default 20, max 100), before:int? (store index — page backward). Pages turns back from the DURABLE store — the FULL conversation, so anything compaction dropped is one call away. Read-only. A page always fits INLINE; trimmed from the oldest end when it would not, with the index to continue from. Returns {messages:[{index,role,content}], total, truncated, note?}."),
        "grep": .string("args: query:str (req — case-insensitive substring), ids:list[int]? (row, pid or fold ids to search), limit:int? (default: as many as fit inline), width:int? (chars per matched line, default 200). Finds the LINE that mentions something ANYWHERE in your memory — every row you ever wrote or received (behind every clear, inside every fold; a fold id searches its members), and the reply bodies still held this run. Raise `width` when the value sits deep in a long line. Returns {matches:[{id, line, text}], total, truncated}."),
        "context_recall": .string("args: dry:bool? (mechanical fit only — no model call). Rebuild this agent's VIEW from its store: with `context_strategy` fold, a head longer than the budget is FOLDED first (oldest spans into fold rows, as a turn's projection folds); then fit the head to the budget at digest resolution, ask the model which rows it wants in full, persist the id list, log the decision as a row, and drop RAM scratch. Runs on boot, after a fold batch, or on request. Returns {kept, expanded, dropped, folded, budget, why, degraded}."),
        "context_clear": .string("args: why:str? — start a NEW THREAD on the same store: a CLEAR-FOLD — one row whose members are every row since the previous clear (no summary) — and the model's view begins at its `[cleared <id> · N rows]` line. Nothing is deleted or forgotten: grep, recall and `history {fold: <id>}` reach every member. Empties the persisted view and drops RAM scratch (projection, fold counters, pending recall). Returns {cleared:id, rows:N}."),
        "watch": .string("args: body:bool? (default false), watcher:str? (operator only — an inbox id to deliver to). Announce every row this agent appends to YOUR inbox as {type:'row', source, row:{id, kind, role, bytes}} — content-free; with body:true the row's `content` too. Needs a registered inbox. Returns {watching:true, watcher, body}."),
        "unwatch": .string("args: watcher:str? (operator only). Stop announcing rows to you. Returns {watching:false}."),
        "boot": .string("LAZY: never touches the memory store. Checks the record names one (fail loud otherwise), starts a new incarnation, and marks the view for rebuilding — the first turn reads the head from the store and runs `context_recall`."),
    ]

    /// `VERBS.keys` — what every ai_core bundle advertises statically (python
    /// `VERBS` of `_build()`), the same table reflect renders.
    public static let verbNames: [String] = Array(VERBS.keys)

    /// The outcome of ONE provider pass.
    ///
    /// `send` answers with the finished text on every runtime, so the reason a pass
    /// did NOT finish has to travel back with it rather than being flattened to a
    /// missing value.
    enum AIPassOutcome {
        /// The RAW completion — what routing parses — the assistant row's id, the
        /// batch token every reply tag of this turn names (`↑<turn>`), and every
        /// closed call part `(stream_id, body)` — what routing stamps each
        /// dispatched message with (python's `call_streams`).
        /// text, message id, the closed call parts, and `cut`: the model stopped
        /// with a block open, so NOTHING of the pass dispatches (owner, 2026-10-04).
        case ok(String, Int64, [(streamId: String, body: String)], Bool)
        /// A DEAD TURN: the RAW completion held nothing once the thinking was
        /// stripped. Nothing was stored; routing rerolls it with the no-call
        /// reminder (python `_run`'s `dead_turn` reply).
        case dead(String)
        /// The projection REFUSED the turn (too_small failsafe, unknown strategy).
        /// Its body is returned to the caller VERBATIM — no `client_id` bolted on,
        /// and no `status(done)`, because the model was never called.
        case refused(JSON)
        /// The KERNEL killed the task. The turn was already closed out for the
        /// client by `onCancel`; nothing more should be emitted or routed.
        case cancelled
    }

    // MARK: - send

    /// `inquiry` on an AI: accepted at once, answered LATER (owner, 2026-10-06).
    ///
    /// The human's bundle answers this verb from a record; an AI answers it by
    /// working and THINKING, in its own time. Either way the asker reads the answer
    /// as `[reply to <pid> · <who> inquiry]`, a later message under the same pid, so
    /// it never branches on who is at the other end. The inquiry runs as its own
    /// chain on this agent; the answer — given in ANY turn of that chain, after
    /// whatever work it needs — reaches the asker through `closeInquiry`. It used to
    /// be read off this agent's FIRST turn only, so a reviewer that read the spec
    /// and ran `git diff` before answering was told it had not answered (measured
    /// 2026-10-06, peer_review:rust). Python `_inquiry`.
    private func inquiryAccept(_ id: String, _ payload: JSON, _ kernel: Kernel) -> JSON {
        let q = payload["text"].asString ?? ""
        if q.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return .object([
                "error": .string("inquiry: `text` must be a non-empty string"),
                "reason": .string("missing_arg"), "arg": .string("text"),
            ])
        }
        let asker = KernelTaskLocals.currentOrigin?.value
        let task = KernelTaskLocals.currentTask
        let pid = task.map { String($0.value) } ?? "None"
        let client = safeClient("inquiry-\(pid)")
        // `options`/`kind` ride in the text because a model reads prose, not a
        // schema; `action` is carried verbatim so an answering AI can see what it
        // is being asked to endorse.
        var text = inquiryHead(asker: asker ?? "the operator", pid: pid) + "\n" + q
        if let opts = payload["options"].asArray, !opts.isEmpty {
            // `str(o)` per option — `true` reads `True`, a dict its python repr.
            text += "\n\nOptions: " + opts.map { pyStr($0) }.joined(separator: ", ")
        }
        if case .object = payload["action"] {
            // `json.dumps(act)` — python separators, ensure_ascii.
            text += "\n\nProposed action: " + pyJsonDumps(payload["action"])
        }
        let replyRaw = payload["reply_client_id"]
        AIInquiries.register(
            kernel, id, client,
            OpenInquiry(
                asker: asker, pid: pid,
                replyClient: safeClient(pyTruthy(replyRaw) ? pyStr(replyRaw) : ""),
                requestId: payload["request_id"]))
        deliverSoon(
            kernel: kernel, target: id, text: text, clientId: client,
            systemPrompt: payload["system_prompt"].asString, notice: false)
        return .object([
            "accepted": .bool(true),
            "re": task.map { .integer($0.value) } ?? .null,
            "request_id": payload["request_id"],
        ])
    }

    private func sendVerb(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
        // THE RECURSION GUARD, BEFORE THE LOCK — so a cycle can never deadlock
        // on the per-backend FIFO lock (A→B→A would otherwise block on A's held
        // lock until the send ceiling expired). `_call_stack` is the caller's
        // reserved key; nothing on the reply-routing path carries one, so a
        // `reply_to:'self'` chain is not a cycle. Python `core.py:2197-2218`.
        // `str(client_id)` for a truthy non-string (python `_safe_client`, 2026-09-20).
        let guardClientId = safeClient(pyTruthy(payload["client_id"]) ? pyStr(payload["client_id"]) : "")
        let callStack = payload["_call_stack"].asArray ?? []
        if callStack.contains(where: { $0.asString == agent.id.value }) {
            return .object([
                "error": .string("\(config.kind): cycle detected"),
                "response": .string(""),
                "cycle": .array(callStack + [.string(agent.id.value)]),
                "client_id": .string(guardClientId),
            ])
        }
        if callStack.count >= AIConstants.MAX_CALL_DEPTH {
            return .object([
                "error": .string(
                    "\(config.kind): max call depth \(AIConstants.MAX_CALL_DEPTH) reached"),
                "response": .string(""),
                "client_id": .string(guardClientId),
            ])
        }
        // NO STORE, NO TURN — said up front, not discovered mid-generation.
        do {
            _ = try storeOf(agent.id.value, kernel)
        } catch let e as MemoryStoreError {
            return .object([
                "error": .string(e.message), "reason": .string("missing_arg"),
                "client_id": .string(guardClientId),
            ])
        }
        if pyTruthy(payload["late"]) {
            // A reply to a call a PREVIOUS incarnation made (see `deliverTurn`):
            // recorded in this agent's own store, never a turn. The TARGET writes
            // it — a store keys every row by its writer.
            let lateText = payload.asObject?["text"].map { pyStr($0) } ?? ""
            try await saveHistory(
                agent.id.value, kernel,
                [
                    .object([
                        "role": .string("user"),
                        "parent": .null,
                        "kind": .string("late"),
                        "content": .string(
                            "[late reply — answered a call made before this agent last "
                                + "booted; recorded, not acted on]\n" + lateText),
                    ])
                ])
            return .object(["late": .bool(true), "client_id": .string(guardClientId)])
        }
        // MISSING `text` IS THE EMPTY STRING, not a refusal: python's
        // `payload.get("text", "")` and rust's `unwrap_or("")` both run the turn, so
        // a caller-driven agent kicking off from its system prompt alone worked on
        // two kernels and was refused on the third with a reason they never produce.
        var text = payload["text"].asString ?? ""
        // A MESSAGE IS TEXT WHATEVER THE CALLER TYPED. The tag grammar types a
        // bare number (`<text>586</text>` → 586), and "586" is a perfectly good
        // message; reading only `asString` dropped it and ran an empty turn.
        // A bool is NOT a number here, exactly as python's `isinstance` excludes
        // it, so `<text>true</text>` stays the empty turn it was.
        switch payload["text"] {
        case .integer, .double: text = pyStr(payload["text"])
        default: break
        }
        // AN ANSWER UNDER AN INQUIRY'S PID settles what this agent was owed.
        if let t = payload["text"].asString { AIInquiries.settle(kernel, agent.id.value, t) }
        // SANITISED AT ENTRY, as python's `_send` does. The queue/current entry held
        // the RAW id while `statusVerb` compared against `safeClient(requesting)`, so
        // a browser client `tab 1/x` asking after its OWN work was told `is_mine:false,
        // mine_pending:[], others_pending:1`.
        let clientId = safeClient(pyTruthy(payload["client_id"]) ? pyStr(payload["client_id"]) : "")
        // A caller-supplied `system_prompt` REPLACES the default system block
        // (the caller teaches its own role/context there). Mirrors py/rust.
        let systemPrompt = payload["system_prompt"].asString
        // Where this pass's TEXT goes when the model issues no call of its own.
        // A one-shot handoff for the CALLER; the MODEL routes its own work with
        // `reply_to` inside its `<call>`s. Mirrors python's `send_to`.
        // KEPT RAW, refused only when it is not a string or is blank after a full
        // python strip: `send_to:' peer '` hands off to `' peer '` (and the
        // unknown-agent refusal names it, spaces included), while `'\n\t'` — which
        // `.whitespaces` alone does not strip — is no hand-off at all.
        let sendTo: String? = {
            guard let s = payload["send_to"].asString, !pyStrip(s).isEmpty else { return nil }
            return s
        }()

        // WHO SAID IT, TO WHOM, UNDER WHICH NUMBER. A message from another AGENT
        // opens with a header the KERNEL vouches for — the sender is the
        // dispatch's own context, never the sender's prose. Without it a peer's
        // message arrived anonymous (measured 2026-09-10, multisubai: solvers
        // signed their prose by hand) and a group message was indistinguishable
        // from a private one.
        if let sender = KernelTaskLocals.currentOrigin?.value, sender != agent.id.value {
            var parts = ["from \(sender)"]
            if let to = payload["to"].asArray, !to.isEmpty {
                parts.append("to " + to.map { pyStr($0) }.joined(separator: ", "))
            }
            if payload["msg"] != .null { parts.append("msg \(pyStr(payload["msg"]))") }
            if payload["re"] != .null { parts.append("re \(pyStr(payload["re"]))") }
            // the stream this message was previewed as — what an AI consumer that
            // watched it stream matches its draft to (owner, 2026-10-04)
            if payload["stream_id"] != .null {
                parts.append("stream \(pyStr(payload["stream_id"]))")
            }
            text = "[" + parts.joined(separator: " \u{b7} ") + "]\n" + text
        }

        // The provider is built INSIDE the turn (python `_run`), after `queued` /
        // `thinking` — so a failed build ends the turn with `done(error)`.

        // Enqueue this submission. Only ONE generation runs at a time per
        // agent (FIFO) — concurrent callers queue in arrival order, which
        // also keeps the chat-store-persisted history race-free.
        let sendId = mintSendId()
        let st = runState(kernel, agent.id.value)
        // A notice is this agent's own harness text: honoured only when the agent
        // sent it to itself (or the operator did), so no peer can hide a message
        // from a person's view. Python `_send`.
        let origin = KernelTaskLocals.currentOrigin?.value
        let notice = pyTruthy(payload["notice"]) && (origin == nil || origin == agent.id.value)
        st.enqueue(
            AIQueuedEntry(
                clientId: clientId, text: text, sendId: sendId, queuedAt: nowSecs(),
                notice: notice))

        // Contention probe (best-effort, like Rust's try_lock): if a generation
        // already holds the lock, tell the caller it's queued. ONE event, and it is
        // the `status` one — a bare `{type:'queued'}` rode ahead of it on rust/swift
        // until 2026-09-02, an undocumented type python never sends, which broke any
        // per-submission event count and every cross-runtime event-stream diff.
        if await st.fifo.busy() {
            let ahead = max(0, st.queueDepth() - 1)
            await emitStatus(
                kernel: kernel, agentId: agent.id, clientId: clientId, phase: "queued",
                detail: [("send_id", .string(sendId)), ("ahead", .integer(Int64(ahead)))])
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
            // The task we are running AS — how `onCancel` recognises its own turn.
            taskId: KernelTaskLocals.currentTask.map { String($0.value) })
        await emitStatus(
            kernel: kernel, agentId: agentId, clientId: clientId, phase: "thinking")

        // ONE CEILING OVER THE WHOLE GENERATION — recall, assembly, projection
        // and the stream — exactly as python's `asyncio.timeout(cfg.send_timeout)`
        // wraps `_run`. Swift bounded only the gap between two chunks, so a
        // provider that stalled BEFORE the first token, or an assembly that hung,
        // ran forever under a ceiling that looked like it applied.
        let produced: AIPassOutcome?
        let userText = text
        do {
            produced = try await withGenerationCeiling(sendTimeoutSeconds) { [self] in
                let provider: any AIProvider
                // `runStream` owns the in-flight count it was bumped for; a turn
                // that never reaches it gives the count back here.
                do {
                    provider = try await providerInTurn(agent, clientId, kernel)
                } catch {
                    bumpInFlight(-1)
                    throw error
                }
                return try await runStream(
                    provider: provider,
                    agent: agent,
                    clientId: clientId,
                    st: st,
                    userText: userText,
                    systemPrompt: systemPrompt,
                    kernel: kernel
                )
            }
        } catch let e as MemoryStoreError {
            // FAIL FAST: the store refused or is gone — no turn, said so.
            await emitStatus(
                kernel: kernel, agentId: agentId, clientId: clientId, phase: "done",
                detail: [("reason", .string("error")), ("error", .string(e.message))])
            st.clearCurrent()
            await st.fifo.release()
            return .object([
                "error": .string(e.message), "reason": .string("memory_store"),
                "client_id": .string(clientId),
            ])
        } catch {
            // A PROVIDER FAILURE RAISES (see `runStream`). The lock is
            // python's `finally`: it is released on the way out, or the agent is
            // stranded for good.
            st.clearCurrent()
            await st.fifo.release()
            throw error
        }
        // `onCancel` already cleared the entry on the killed path; clearing again
        // is a no-op. The FIFO lock is released either way — a killed generation
        // unwinds through here, it does not strand the agent.
        //
        // The ORDER here is the invariant, not an accident: `runStream` persisted
        // the assistant turn, `clearCurrent` retires the in-flight entry, and
        // `release()` frees the FIFO BEFORE routing is spawned — so a
        // self-addressed follow-up takes the lock immediately and reads a history
        // that already contains the turn it is answering.
        guard let produced else {
            // THE CEILING EXPIRED. python emits the terminal status INSIDE the
            // lock and answers `{error, client_id}`; the float is rendered
            // python's way (`180.0`, `0.05`), not as an int.
            await emitStatus(
                kernel: kernel, agentId: agentId, clientId: clientId, phase: "done",
                detail: [("reason", .string("timeout"))])
            st.clearCurrent()
            await st.fifo.release()
            return .object([
                "error": .string("send: timeout after \(pyFloatRepr(sendTimeoutSeconds))s"),
                "client_id": .string(clientId),
            ])
        }
        st.clearCurrent()
        await st.fifo.release()

        let reply: JSON
        switch produced {
        case .ok(let textOut, let messageId, _, _):
            reply = .object([
                "response": .string(textOut),
                "final": .string(textOut),
                "client_id": .string(clientId),
                "message_id": .integer(messageId),
                // the closed call parts (`callStreams`) feed routing only — never
                // the caller's reply
            ])
        case .dead(let textOut):
            // python's `{response, final, client_id, dead_turn: True}` — no
            // `message_id`: no row was minted for a turn that was not stored.
            reply = .object([
                "response": .string(textOut),
                "final": .string(textOut),
                "client_id": .string(clientId),
                "dead_turn": .bool(true),
            ])
        case .refused(let body):
            reply = body
        case .cancelled:
            reply = .object([
                "error": .string("send: cancelled"), "client_id": .string(clientId),
            ])
        }

        // ROUTE this pass's output where it was addressed — on the ok path, and
        // on a DEAD turn (rerolled as a silent turn). A failed pass is terminal
        // (python's `if not reply.get("error")` gate),
        // and a broken chain must not keep spending turns.
        //
        // DETACHED as its own task row (`verb:"route_reply"`), which is why
        // chaining works: awaiting a self-addressed send inline would keep THIS
        // dispatch from returning until the whole chain finished. `ps` must not
        // label routing as `"send"` either, and the row makes each hop
        // individually visible and killable. NEVER `Task.detached`: that loses
        // the task-locals and the row would land parentless, outside `cancel`'s
        // reach.
        // A DEAD turn routes too — as a silent turn, which is how it is rerolled.
        let routed:
            (
                text: String, messageId: Int64?, dead: Bool,
                callStreams: [(streamId: String, body: String)], cut: Bool
            )?
        switch produced {
        case .ok(let t, let m, let c, let x): routed = (t, m, false, c, x)
        case .dead(let t): routed = (t, nil, true, [], false)
        default: routed = nil
        }
        if let (textOut, messageId, deadTurn, callStreams, cut) = routed {
            let deadline = sendTimeoutSeconds
            // THE COORDINATION STATE, threaded through so `await` can count what
            // is actually pending for this agent — its queue and its FIFO lock.
            let st = runState(kernel, agentId.value)
            let incoming = text
            // THE ROUTED CALLS REACH THEIR TARGETS BEFORE THIS REPLY GOES BACK.
            // Python spawns the route task inside this handler and its event loop
            // runs that task up to its dispatch before the caller of this `send`
            // resumes — so a model's own call (`send ai …`) lands before the reply
            // its caller then delivers. A detached swift task could lose that race
            // (measured: parity `ai_paths` cycle, the two turns to `ai` swapped).
            // Released at launch, not at completion: the calls still SETTLE on
            // their own, and a self-addressed call fans out before it needs the
            // lock, so this never waits on this agent.
            let launched = StartSignal()
            kernel.spawnTask(agent: agentId, verb: "route_reply") {
                await routeReply(
                    selfId: agentId, kernel: kernel, text: textOut,
                    clientId: clientId, sendTo: sendTo, systemPrompt: systemPrompt,
                    incoming: incoming,
                    // THE LAYER'S PER-CALL DEADLINE, threaded to every model call
                    // the router dispatches. The kernel defaults nothing.
                    sendTimeout: deadline,
                    modelWindow: self.config.modelWindow,
                    runState: st,
                    messageId: messageId,
                    deadTurn: deadTurn,
                    callStreams: callStreams,
                    cut: cut,
                    launched: launched)
            }
            await launched.wait()
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
    /// python `_run`'s `_get_provider`: built inside the turn, and a raise is
    /// treated as python's `_send` treats any raise of the turn —
    /// `done(reason:error)` and re-raised.
    private func providerInTurn(_ agent: Agent, _ clientId: String, _ kernel: Kernel)
        async throws -> any AIProvider
    {
        do {
            return try await config.makeProvider(agent, clientId, kernel)
        } catch {
            await emitStatus(
                kernel: kernel, agentId: agent.id, clientId: clientId, phase: "done",
                detail: [("reason", .string("error")), ("error", .string(pyErrorText(error)))])
            throw error
        }
    }

    private func runStream(
        provider: any AIProvider,
        agent: Agent,
        clientId: String,
        st: AIAgentRunState,
        userText: String,
        systemPrompt: String?,
        kernel: Kernel
    ) async throws -> AIPassOutcome {
        let agentId = agent.id
        defer { bumpInFlight(-1) }

        // A RECALL OWED FROM THE LAST FOLD BATCH (or the boot) runs first, so this
        // turn is assembled over the head the recall chose — python `_recall_due`.
        if takeRecallDue(agentId.value) {
            _ = try await contextRecall(
                agent: agent, payload: .object([:]), kernel: kernel, provider: provider,
                systemPrompt: systemPrompt, clientId: clientId)
        }
        // The message list UNDER the FIFO lock (we hold it here), so serialized
        // sends can't race load→save: the system block rebuilt from the live
        // substrate (or the caller's override), the VIEW of the store, the live
        // turn. `storeLen` is how many turns the head held behind it.
        //
        // The user turn is NOT persisted here. It is persisted ONCE, with the
        // assistant turn, after the stream completes. A STOPPED turn's rows are
        // `onCancel`'s: the incoming message + the cut marker, never a half answer.
        let (assembled, storeLen) = try await assemble(
            agent: agent, userText: userText, systemPrompt: systemPrompt, kernel: kernel)
        var messages = assembled

        // Context-Protocol seam: shape what the MODEL sees this turn to fit the
        // window (prepending the canonical [context-notice]), ONCE at entry. The
        // window is read off the record EVERY turn, so a model switched to a
        // smaller one (`update_agent`) is fitted — folded — on its very next turn.
        switch try await projectContext(
            provider: provider, agent: agent, clientId: clientId, messages: messages,
            // THE STORE'S LENGTH, not the view's: `messages` carries the
            // deduped/trimmed history, so counting it puts the reaction cursor
            // short by exactly the turns the trimmers dropped.
            storeLen: storeLen,
            kernel: kernel)
        {
        case .projected(let projected):
            messages = projected
        case .failed(let err):
            // too_small failsafe / unknown-strategy — the model is NOT called, so
            // there is no generation to close out: python returns the refusal body
            // VERBATIM and emits NOTHING here (the seam already emitted
            // `context:too_small`).
            return .refused(err)
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
        // STREAMING PARTS (owner, 2026-10-04): the same chunks, split into parts as
        // they are written — animation for the addressee, never action
        // (`StreamParts`).
        let parts = AIStreamParts(
            splitter: PartSplitter(source: agentId.value),
            publisher: PartPublisher(
                kernel: kernel, selfId: agentId, clientId: clientId,
                re: KernelTaskLocals.currentTask?.value))
        st.updateCurrent { $0.parts = parts }
        do {
            let model = renderForModel(messages)
            // A FRESH reading per pass: a provider that reports nothing this time
            // must not leave last pass's number standing.
            provider.usage?.lastPromptTokens = nil
            for try await chunk in provider.chat(messages: model) {
                // The KERNEL killed this task (`cancel <task_id>`). `onCancel`
                // already closed the turn out for the client — status(done,
                // reason:"cancelled") + done, the incoming row + the cut marker —
                // while we were still alive. Bail WITHOUT emitting anything more
                // and WITHOUT persisting the partial turn; returning drops the
                // provider stream, whose `onTermination` cancels the upstream HTTP
                // task.
                if Task.isCancelled { return .cancelled }
                switch chunk {
                case .token(let delta):
                    if firstToken {
                        firstToken = false
                        await emitStatus(
                            kernel: kernel, agentId: agentId, clientId: clientId,
                            phase: "streaming")
                    }
                    accumulated += delta
                    st.updateCurrent { $0.textSoFar += delta }
                    // `{type:'token', text, source, client_id}` — python's four
                    // keys. Swift carried `stream_id`/`message_id`/`delta`/
                    // `accumulated` instead, so a client written against the
                    // documented event read `text` as missing on this runtime
                    // alone.
                    await toCaller(
                        kernel: kernel, agentId: agentId, clientId: clientId,
                        event: [
                            "type": .string("token"),
                            "text": .string(delta),
                            "source": .string(agentId.value),
                        ])
                    await parts.feed(delta)
                }
            }
        } catch {
            // A cancelled task's stream throws as it is torn down — that is our
            // OWN kill, not a provider failure. `onCancel` already emitted the
            // terminal events; stay silent.
            if Task.isCancelled { return .cancelled }
            await parts.stop("error")
            // A PROVIDER FAILURE RAISES (python `_send`'s exception handler). The
            // status carries `str(e)` and the handler RAISES, so the kernel
            // settles `{error:"<Type>: <msg>", raised:true}` and the caller can
            // tell a crash from a refusal. Swallowing it into `{error,
            // client_id}` made every provider bug look like a decision the
            // backend had taken.
            await emitStatus(
                kernel: kernel, agentId: agentId, clientId: clientId, phase: "done",
                detail: [("reason", .string("error")), ("error", .string(pyErrorText(error)))])
            throw error
        }
        // The stream can also FINISH (rather than throw) when the task is
        // cancelled — same contract: the turn is already closed out (`onCancel`
        // wrote its rows), persist nothing more.
        if Task.isCancelled { return .cancelled }
        // ONLY A SUCCESSFUL GENERATION DISPATCHES (owner, 2026-10-04): a pass cut
        // with a block open sends nothing, its earlier closed calls included, and
        // the marker lists every call of the pass as not sent.
        let cutCalls = await parts.finish()

        // APPEND this send's user + assistant turns to the durable chat store.
        // NOT the projected view, and NOT the whole conversation re-written: the
        // store owns the full transcript and each turn is one `append`. THE ONLY
        // WRITE on a finished pass, as in python. The error / FM-interrupt /
        // timeout paths return before this point and persist nothing — neither
        // half of an exchange that never completed; a cancel's rows are
        // `onCancel`'s (the incoming message + the cut marker).
        //
        // PARSE THE RAW TEXT, PERSIST THE STRIPPED TEXT (Python core.py:1016-1018).
        // The deliberation is not part of the conversation: re-feeding a reasoning
        // model its own prior thought is against every vendor's guidance and it
        // compounds turn over turn. The RAW text (markers in) is what the
        // `token`/`done` events already carried; routing reads only the answer.
        // `status(phase='done')` IS THE COMPLETION EVENT, and it is emitted
        // BEFORE the turn is persisted — python's order (`core.py:2039-2064`).
        // A subscriber that watches the store and the events together sees the
        // same sequence on every runtime; swift persisted first, so its two rows
        // landed before the event that announces them.
        // THE CALIBRATION: what the endpoint REPORTED for this prompt, against our
        // own estimate of what we sent.
        if let reported = provider.usage?.lastPromptTokens, reported > 0 {
            AICalibration.record(
                kernel, agentId.value, reported: reported,
                estimate: AIContext.estimateTokens(renderForModel(messages)))
        }

        // A DEAD TURN — nothing left once the thinking is stripped — did not
        // answer, whatever the provider's finish reason says. Measured 2026-09-30
        // on vLLM: a thinking budget of 8 tokens ended the thought, the model
        // stopped cleanly (`finish_reason: stop`) with an empty answer, and the
        // turn was stored as a completed, empty assistant row. The ANSWER is
        // discarded here — never stored, never counted as an answer — and the
        // turn is routed like any that dispatched nothing: the no-call reminder
        // rerolls it (owner, 2026-09-30). Failing the turn outright ended a
        // 30-pass chain on its first dead turn (E31, measured). Python `_run`.
        if pyStrip(withoutThinking(accumulated)).isEmpty {
            // The INCOMING row is kept — only the empty answer is dropped. The
            // turn this one answered may be a reply batch carrying the results of
            // the model's own calls; unstored, the reroll would arrive with those
            // results gone and the model would re-ask for them (found in the swift
            // port's review, 2026-09-30).
            let (inId, inParent) = rowIdsForIncoming(userText, kernel: kernel)
            try await saveHistory(
                agentId.value, kernel,
                [
                    incomingRow(
                        userText, id: inId, parent: inParent,
                        notice: st.currentSnapshot()?.notice ?? false)
                ])
            await emitStatus(
                kernel: kernel, agentId: agentId, clientId: clientId, phase: "done",
                detail: [("reason", .string("dead_turn"))])
            return .dead(accumulated)
        }

        await emitStatus(
            kernel: kernel, agentId: agentId, clientId: clientId, phase: "done",
            detail: [("reason", .string("ok"))])

        // ROW IDS for this turn: the incoming row (a root, or a delivered reply
        // turn that names its parent on its tag) and the assistant row that
        // answers it. The batch token on this turn's reply tags is the assistant
        // row's own id, so `↑` on every reply points at something the model can
        // see.
        //
        // MINTED HERE, AT THE APPEND — not at turn start. The store returns an
        // owner's rows in ID order and the head cache keeps APPEND order; the two
        // agree only while ids are minted as rows are appended. Minted at turn
        // start, a fold or view row saved mid-turn (the projection) took a higher
        // id yet landed first, and after a restart the conversation came back in a
        // different order than the agent had lived it (python, E78).
        let (inId, inParent) = rowIdsForIncoming(userText, kernel: kernel)
        let messageId = kernel.mintId()
        // APPEND this send's user + assistant turns to the memory store. NOT the
        // projected view: the store keeps the FULL conversation so context
        // management (compaction/recall) works next turn. PARSE THE RAW TEXT,
        // PERSIST THE STRIPPED TEXT — the deliberation is not part of the
        // conversation.
        try await saveHistory(
            agentId.value, kernel,
            [
                incomingRow(
                    userText, id: inId, parent: inParent,
                    notice: st.currentSnapshot()?.notice ?? false),
                // A pass the model stopped with a block open leaves the CUT
                // MARKER instead, on top of the context (owner, 2026-10-04).
                .object([
                    "role": .string("assistant"),
                    "content": .string(
                        cutCalls.map { cutMarker(accumulated, "model stopped generating", $0) }
                            ?? withoutThinking(accumulated)),
                    "id": .integer(messageId), "parent": .integer(inId),
                ]),
            ])

        // The RAW completion — markers in — goes to routing, which reads only the
        // ANSWER: a call written inside thinking never dispatches (owner, 2026-10-07).
        return .ok(accumulated, messageId, await parts.closedCalls(), cutCalls != nil)
    }

    // MARK: - status (verb + phase events)

    /// `status` verb — privacy-filtered snapshot of the in-flight entry +
    /// this client's own pending queue. Other clients' text is redacted.
    /// Mirrors Rust `status_snapshot`.
    private func statusVerb(agentId: AgentId, payload: JSON, kernel: Kernel) -> JSON {
        // `cid = payload.get("client_id"); if cid: cid = _safe_client(cid)` — an
        // EMPTY string is falsy, so it is neither sanitised nor treated as a
        // caller: it is ECHOED as `client_id: ""` and the snapshot takes the
        // redacted branch. Swift ran `safeClient("")`, which answers `"cli"`, so
        // a probe asking with `''` was told about cli's work.
        let rawJ = payload["client_id"]
        let requesting: String? = pyTruthy(rawJ) ? safeClient(pyStr(rawJ)) : rawJ.asString
        let asked = requesting?.isEmpty == false
        let st = runState(kernel, agentId.value)
        let cur = st.currentSnapshot()
        let queue = st.queueSnapshot()

        let currentOut: JSON =
            cur.map { entry(for: $0, requesting: asked ? requesting : nil) } ?? .null

        var minePending: [JSON] = []
        var othersPending = 0
        for q in queue {
            if asked, q.clientId == requesting {
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

    /// Render the in-flight entry. TWO SHAPES, python's (`_status_snapshot`):
    ///
    /// - a caller asked (`client_id` truthy) → the composed
    ///   `{phase, send_id, started_at, elapsed, is_mine}`, plus `text`,
    ///   `text_so_far` and any `last_tool` when the entry is that caller's own.
    /// - nobody asked → `_redact_text(cur)`: the ENTRY's own keys minus the
    ///   text-bearing ones, `task_id`, the streaming `parts` (the live splitter +
    ///   publisher) and `stopped_by` — bookkeeping, not status, and the splitter is
    ///   not JSON — in the entry's order
    ///   (`client_id, send_id, queued_at, started_at, phase`), then `elapsed`
    ///   and `is_mine:false`.
    ///
    /// Swift rendered one shape for both, so a headless snapshot carried
    /// `phase` before `send_id` and never carried `queued_at` at all.
    private func entry(for c: AICurrentEntry, requesting: String?) -> JSON {
        // `time.time() - cur.get("started_at", time.time())` — python does not
        // clamp; a clock that stepped backwards reads negative on both.
        let elapsed = nowSecs() - c.startedAt
        guard let requesting else {
            return .object([
                "client_id": .string(c.clientId),
                "send_id": .string(c.sendId),
                "queued_at": .double(c.queuedAt),
                "started_at": .double(c.startedAt),
                "phase": .string(c.phase),
                "elapsed": .double(elapsed),
                "is_mine": .bool(false),
            ])
        }
        let isMine = requesting == c.clientId
        var out: OrderedDictionary<String, JSON> = [
            "phase": .string(c.phase),
            "send_id": .string(c.sendId),
            "started_at": .double(c.startedAt),
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
    /// THE system block a turn uses — the one choice `assemble` builds the turn on
    /// and `contextRecall` sizes the body budget against, so the two never drift.
    /// PRECEDENCE: the per-call argument (this delivery) > the record's own
    /// `system_prompt` (read LIVE, so an `update_agent` lands on the next turn — a
    /// main turn, a self-delivery, a turn woken by a reply) > the default block.
    /// Python `_system_content`.
    func systemContent(agent: Agent, systemPrompt: String?, kernel: Kernel) async -> String {
        let perCall = systemPrompt.flatMap { pyStrip($0).isEmpty ? nil : $0 }
        return await assembleSystemPrompt(
            agent: agent,
            systemPrompt: perCall
                ?? RecordSchema.recordSystemPrompt(agent.metaValue(forKey: "system_prompt")),
            kernel: kernel)
    }

    func assembleSystemPrompt(agent: Agent, systemPrompt: String?, kernel: Kernel) async -> String {
        var blocks: [String]
        if let override = systemPrompt,
            !override.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            blocks = [override]
        } else {
            let selfId = agent.id
            // THE `kernel` ADDRESS, not the root's own id: python targets
            // `kernel`, and the root id differs per runtime (`core` here,
            // `kernel_state` in python), so hard-coding it made the primer
            // resolve differently on each.
            let primer = await kernel.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("reflect"),
                        "tree": .string("ids"),
                        "bundles": .string("ids"),
                    ]))
            ])[0].result
            let full = await kernel.send([Item(selfId, .object(["type": .string("reflect"), "tree": .string("none")]))])[0].result
            // WHO YOU ARE, not how you are tuned: identity, memory and grants. The
            // rest of the agent's own reflect (sampling, kv table, timeouts,
            // endpoint) is operator posture the model never acts on — it cost ~1k
            // chars on every turn. Python `_SELF_KEYS`, in its order.
            var me = full
            if let obj = full.asObject {
                var kept = OrderedDictionary<String, JSON>()
                for k in SELF_KEYS { if let v = obj[k] { kept[k] = v } }
                me = .object(kept)
            }
            let store = agent.metaValue(forKey: "memory_store_id")
            blocks = [
                renderReflect(primer),
                "You are `\(selfId.value)`. " + renderReflect(me),
                SEND_TO_HOWTO,
                CONTEXT_HOWTO,
                // `rec0.get("memory_store_id") or "(none)"` — python truthiness.
                memoryHowto(store: store.map { pyTruthy($0) ? pyStr($0) : "(none)" } ?? "(none)"),
            ]
            // OPT-IN, on the record. Both verbs exist on every AI, but teaching
            // them costs every agent prompt tokens for a discipline most runs
            // never need — and a block like this changes behavior just by being
            // read. Titles matter only when a strategy STRIPS a body; the
            // async/rows block only when replies arrive as rows. Each flag turns
            // its block on INDEPENDENTLY of `context_strategy`, so a comparison
            // can hold the model's behavior fixed and vary one thing.
            // PYTHON TRUTHINESS: `context_titles: 1` on the record turns the
            // block on. `asBool` read only a JSON `true`, so the flag a scalar
            // grammar produces (`1`) silently did nothing.
            let flag = { (k: String) in pyTruthy(agent.metaValue(forKey: k) ?? .null) }
            if flag("context_titles") { blocks.append(TITLES_HOWTO) }
            if flag("context_async") { blocks.append(ASYNC_HOWTO) }
        }
        return blocks.joined(separator: "\n\n")
    }

    /// Build the message list: the system block, the MODEL VIEW of the store,
    /// the live user turn. A caller-supplied `systemPrompt` REPLACES the
    /// deterministic substrate prompt; absent that, the record's own
    /// `system_prompt` does. The conversation is ALWAYS this agent's
    /// own (its memory store): there is no caller-supplied history — a one-off
    /// completion is a disposable agent with its own store; context for another
    /// agent travels in the text.
    ///
    /// Returns `(messages, storeLen)` — `storeLen` is how many turns the head held
    /// when this view was built. The view is deduped and trimmed, so its length
    /// is NOT a store index; `projectContext` needs the store one to mark where
    /// this turn's reaction will land. Python `_assemble`.
    func assemble(
        agent: Agent, userText: String, systemPrompt: String?, kernel: Kernel
    ) async throws -> ([JSON], Int) {
        let sysContent = await systemContent(agent: agent, systemPrompt: systemPrompt, kernel: kernel)
        // The system block is rebuilt on EVERY user turn; the store holds only the
        // user/assistant turns. The howto is not persisted — it always reflects
        // the latest state at send-time.
        var messages: [JSON] = [.object(["role": .string("system"), "content": .string(sysContent)])]
        let hist = try await loadHistory(agent.id.value, kernel)
        let storeLen = hist.count
        // THE VIEW ARRAY IS THE STATE. When one exists and every id it names still
        // resolves, the model sees exactly those rows at exactly those resolutions
        // — the same bytes before and after a restart. Otherwise (no array yet, or
        // an id that no longer resolves) the view is the trimmed store: stale
        // whole-state reads deduped, repeated bodies stubbed, answered reflects
        // reduced to a stub (see `HistoryView.swift`).
        let view = try await loadView(agent.id.value, kernel)
        var byId: [Int64: JSON] = [:]
        var idx: [Int64: Int] = [:]
        for (i, r) in hist.enumerated() {
            if let id = rowId(r), id != 0 {
                byId[id] = r
                idx[id] = i
            }
        }
        let entries = view?["ids"].asArray ?? []
        let resolves = !entries.isEmpty
            && entries.allSatisfy { e in e["id"].asInt.map { byId[$0] != nil } ?? false }
        if view != nil && resolves {
            for e in entries {
                messages.append(applyRes(byId[e["id"].asInt!]!, e["res"].asString ?? "full"))
            }
            // THE ARRAY IS THE HEAD AS OF ITS WRITE; THE LOG KEEPS GROWING. Every
            // row appended since — the model's own turns, the replies that landed —
            // rides behind it in full. Measured 2026-09-06 (python,
            // recall_after_fold): served the array alone, the model never saw the
            // read it had just made and restarted the survey from filler_0, sixty
            // turns in a circle.
            let newest = entries.map { idx[$0["id"].asInt!]! }.max()!
            messages.append(
                contentsOf: dropReadDocs(
                    dedupeStaleReads(dedupeRepeatBodies(Array(hist[(newest + 1)...])))))
            setViewSource(agent.id.value, "array")
        } else {
            messages.append(
                contentsOf: dropReadDocs(dedupeStaleReads(dedupeRepeatBodies(hist))))
            // AN EMPTY `ids` IS A CLEARED VIEW, not a stale one: `context_clear`
            // empties the array so assembly renders the store from the clear on.
            setViewSource(
                agent.id.value,
                view == nil || entries.isEmpty ? "store" : "store (stale array)")
        }
        messages.append(.object(["role": .string("user"), "content": .string(userText)]))
        return (messages, storeLen)
    }

    /// Project the message list to what the model actually reads — its role and
    /// its text; every other key is stripped. Python `_render_for_model`.
    private func renderForModel(_ messages: [JSON]) -> [JSON] {
        messages.map { m in .object(["role": m["role"], "content": m["content"]]) }
    }

    // NO `done` EVENT. `status(phase='done')` IS the completion — two events for
    // one transition is two truths, and a subscriber watching the wrong one
    // drifts silently. python emits only the status; swift emitted both, so
    // every swift turn carried one event python never sends (and `reflect.emits`
    // documented three channels while the code produced four).

    // MARK: - history

    /// One row at DIGEST resolution — `{id, parent, kind, role, head, bytes}`.
    /// Python `_digest`.
    static func digest(_ r: JSON) -> JSON {
        // `str(r.get("content", ""))` — an ABSENT content is the empty string, a
        // null one is the word `None`, and a dict is its python repr.
        let content = pyStr(r.asObject?["content"] ?? .string(""))
        // `kind` FALLS BACK TO `role` on python TRUTHINESS. 120 CODE POINTS,
        // newlines flattened: a head is for recognising a row, not for reading it.
        let scalars = content.unicodeScalars
        var head = String.UnicodeScalarView()
        for c in scalars.prefix(120) { head.append(c == "\n" ? " " : c) }
        return .object([
            "id": r["id"],
            "parent": r["parent"],
            "kind": pyTruthy(r["kind"]) ? r["kind"] : r["role"],
            "role": r["role"],
            "head": .string(String(head)),
            "bytes": .integer(Int64(scalars.count)),
        ])
    }

    /// args: fold:int? (a fold or clear id — list ITS members instead),
    /// limit:int?, after:int?. The conversation at DIGEST resolution, oldest
    /// first, paged; `{rows, next, total, fold?}`. With `fold`, that fold's title
    /// + summary and its members (one level down). Python `_make_history`.
    private func historyVerb(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
        var head: JSON? = nil
        let rows: [JSON]
        if payload["fold"] != .null {
            guard let foldId = aiAsId(payload["fold"]) else {
                return .object([
                    "error": .string("history: `fold` must be a fold id (int)"),
                    "reason": .string("bad_arg"), "arg": .string("fold"),
                ])
            }
            let everything = try await loadHistory(agent.id.value, kernel, behind: true)
            var byId: [Int64: JSON] = [:]
            for r in everything { if let id = rowId(r), id != 0 { byId[id] = r } }
            guard let f = byId[foldId], ["fold", "clear"].contains(f["kind"].asString ?? "") else {
                return .object([
                    "error": .string("history: \(foldId) is not a fold in your conversation"),
                    "reason": .string("not_found"), "arg": .string("fold"),
                ])
            }
            let refs = f["refs"].asArray ?? []
            rows = refs.compactMap { m in m.asInt.flatMap { byId[$0] } }
            head = .object([
                "id": .integer(foldId),
                "kind": f["kind"],
                "title": f["title"],
                "summary": pyTruthy(f["body"]) ? f["body"] : .string(""),
                "members": .integer(Int64(refs.count)),
            ])
        } else {
            rows = try await loadHistory(agent.id.value, kernel)
        }
        var digest: [JSON] = rows.filter { pyTruthy($0["id"]) }.map(Self.digest)
        // python `int(payload.get("limit") or 0)`, a bad value reading as 0.
        let limit = (try? pyIntOr(payload["limit"], 0)) ?? 0
        if let after = aiAsId(payload["after"]) {
            // AN UNKNOWN CURSOR PAGES NOTHING, rather than starting over.
            let at = digest.firstIndex { $0["id"].asInt == after }.map { $0 + 1 } ?? digest.count
            digest.removeFirst(at)
        }
        // THE CAP INVARIANT: a digest page must answer UNDER the inline cap or it
        // is digested to its origin line. `limit` bounds the count; the cap bounds
        // the bytes. `limit or None` — only ZERO is "no limit".
        let (kept, truncated) = AIContext.fitReply(
            digest, capTokens: AIContext.inlineCap(agent, modelWindow: config.modelWindow), limit: limit != 0 ? limit : nil)
        var out: OrderedDictionary<String, JSON> = [
            "rows": .array(kept),
            "next": truncated ? (kept.last?["id"] ?? .null) : .null,
            "total": .integer(Int64(rows.count)),
        ]
        if let head { out["fold"] = head }
        return .object(out)
    }

    /// Trim + sanitise a client id — the status/queue axis, never a store key.
    /// Python `_safe_client`: `(client_id or "cli").strip() or "cli"`, then every
    /// CODE POINT that is not alphanumeric or `._-` becomes `_`. No length cap.
    ///
    /// CODE POINTS, and python's alphanumeric: swift mapped CHARACTERS (grapheme
    /// clusters) and asked `isLetter || isNumber`, so a decomposed `é` (e +
    /// U+0301) survived as one character where python writes `e_`, and
    /// `.whitespacesAndNewlines` left U+001C..U+001F in place where python's
    /// `strip` removes them.
    func safeClient(_ clientId: String) -> String {
        let stripped = pyStrip(clientId)
        let raw = stripped.isEmpty ? AIConstants.DEFAULT_CLIENT_ID : stripped
        var out = String.UnicodeScalarView()
        for c in raw.unicodeScalars {
            out.append(pyIsAlnum(c) || c == "." || c == "_" || c == "-" ? c : "_")
        }
        return String(out)
    }

    /// (id, parent) for an incoming turn. A delivered reply turn names the
    /// assistant turn that made the calls (`↑`) on its first tag; anything else
    /// is a ROOT — a fresh message from a person or a peer. Python
    /// `_row_ids_for_incoming` + `_TAG_LINKS`.
    func rowIdsForIncoming(_ text: String, kernel: Kernel) -> (Int64, Int64?) {
        let rid = kernel.mintId()
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
// `_TAG_LINKS`: `^\[reply to \S+ · [^\]]*?↑([^\s\]]+)[^\]]*\]`.
// `↑<id>` stops at whitespace OR the tag's closing `]` — python's `_TAG_LINKS`
// (2026-10-01): a tag ending on its `↑` segment with a later `]` in the body
// captured `360]`, which is no id, so the reply row lost its parent.
private let tagLinks = try! NSRegularExpression(
    pattern: "^\\[reply to \\S+ \u{b7} [^\\]]*?\u{2191}([^\\s\\]]+)[^\\]]*\\]")

/// python `str.strip(".")` — dots off both ends, nothing else (the whitespace
/// strip is a separate step, and the order matters: `'. x'` keeps its `x`).
func pyStripDots(_ s: String) -> String {
    var scalars = Array(s.unicodeScalars)
    while scalars.first == "." { scalars.removeFirst() }
    while scalars.last == "." { scalars.removeLast() }
    return String(String.UnicodeScalarView(scalars))
}

/// Race `body` against the send ceiling. `nil` = the ceiling won.
///
/// python has `asyncio.timeout`; swift has no equivalent, so the generation and
/// a sleeper run as siblings and the FIRST to finish decides. Losing the race
/// cancels the other, which is what tears the provider's HTTP stream down.
func withGenerationCeiling<T: Sendable>(
    _ seconds: Double,
    _ body: @escaping @Sendable () async throws -> T
) async throws -> T? {
    try await withThrowingTaskGroup(of: T?.self) { group in
        group.addTask { try await body() }
        group.addTask {
            // A CANCELLED sleep is a cancel, not an expiry: rethrow it. Swallowing
            // it here returned nil at once, so a `cancel` of the send read as the
            // ceiling expiring and a second `done` (reason `timeout`) followed the
            // `cancelled` one (parity `ai_paths/cases/cancel_midstream`).
            try await Task.sleep(nanoseconds: UInt64(max(0, seconds) * 1_000_000_000))
            return nil
        }
        let first = try await group.next() ?? nil
        group.cancelAll()
        return first
    }
}

/// The `↑<turn>` an incoming turn's FIRST tag names, if it is a delivered reply.
func incomingParent(_ text: String) -> Int64? {
    let ns = text as NSString
    guard let m = tagLinks.firstMatch(in: text, range: NSRange(location: 0, length: ns.length)),
        m.numberOfRanges > 1
    else { return nil }
    return aiAsId(.string(ns.substring(with: m.range(at: 1))))
}
