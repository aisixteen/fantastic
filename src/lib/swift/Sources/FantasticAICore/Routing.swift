// Reply routing — where ONE provider pass's output goes, and the ONLY path a
// chain continues on.
//
// Ported from the canonical Python `ai_core/core.py` (`_deliver`, `_route_reply`,
// core.py:449-661). The COMMENTS are part of the port: each one records a
// MEASURED failure with its date, and it is the only thing that stops the
// behaviour being "simplified" away later.
//
// After ONE provider pass, its output goes where it was addressed.
//
//   the model's own <call>s — each names a target, a payload and `reply_to`.
//     All are dispatched, and each reply is delivered to its own `reply_to`.
//     `reply_to:'self'` is what makes a chain, and the chain continues ONLY
//     because the model asked again. No call ends the turn: stop is the default.
//
//   the caller's `send_to` — hands this pass's TEXT to another agent, once.
//
// Nothing here bounds how far the model goes. Every dispatch is a `ps` row and
// `cancel` kills a subtree, so a runaway is visible and killable exactly like a
// script agent's `while(true)`. Deciding how much thinking is allowed is not this
// module's call — and there is deliberately NO per-route timeout: N calls in one
// pass serialize into N turns inside one route task, exactly as python does.
//
// SWIFT-SPECIFIC. Routing runs in its OWN kernel task (`verb:"route_reply"`),
// spawned from inside the generation's task by `AIBackend.sendVerb`. Python
// spawns for a harder reason — its `_lock_for` is a non-reentrant asyncio.Lock,
// so awaiting a self-addressed send inline deadlocks until `send_timeout`. Swift's
// `sendVerb` never holds the FIFO guard while it awaits a nested send, so here
// the spawn is a TRAMPOLINE, not a deadlock escape. The invariant it protects is
// ORDERING instead: the assistant turn is SAVED, then the FIFO guard is
// RELEASED, then routing starts — so a self-addressed follow-up reads a history
// that already contains the turn it is answering. A separate task row also keeps
// `ps` honest: routing is not labelled `"send"`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

// ─── the three model-facing error strings ───────────────────────
//
// These are PROMPTS, not diagnostics. They are the only thing a model has to go
// on when a call it wrote did not run, and they are byte-identical across
// runtimes — pinned by literal-text tests, because a "clearer" rewording in one
// runtime is drift the model pays for. (The third lives with the parser, in
// `Malformed.swift`, composed where the fault is detected.)

/// NAME THE SHAPE, not just the fault.
///
/// `ToolParse` translates a verb-named tool into a send whenever the args say
/// WHERE to send it; what reaches routing named a verb and no agent, and that is
/// a gap only the model can close — guessing a target would dispatch a write to
/// whichever agent we picked. Measured 2026-08-06: gemma-4-12B emitted
/// `{"name":"reflect","arguments":{"detail":"full"}}` and the bare fault told it
/// nothing about how to retry, so it kept re-deriving the envelope from the
/// system prompt instead. Every other error on this path states the fix; this one
/// used not to.
let NO_TARGET_ERROR =
    "no target_id, so nothing was dispatched — a send must name the AGENT it goes to. "
    + "Shape: <call name=\"send\"><target_id>agent</target_id><type>verb</type></call>. "
    + "Send `kernel` <type>reflect</type> for the list of agent ids."

/// NO PAYLOAD, NO SEND.
///
/// Dispatching nothing reaches the handler as nothing and raises an error about
/// OUR internals — python measured `'NoneType' object has no attribute 'get'`
/// three times in one run on 2026-08-06, naming neither the verb the model
/// omitted nor the agent it addressed. `ToolParse` translates every compressed
/// shape it can recognise; what arrives here with no payload really has no verb
/// in it, and saying so is the useful answer. Same shape as `NO_TARGET_ERROR`.
///
/// The target is rendered by python's `{target!r}`, which is NOT "wrap it in
/// single quotes": a target holding an apostrophe comes back double-quoted
/// (`send to "it's"`) and a backslash or a tab is escaped (`'C:\\notes'`,
/// `'tab\there'`).
func noPayloadError(target: String) -> String {
    "send to \(pyRepr(target)) carried no payload, so nothing was dispatched. "
        + "A call names its verb in `<type>`, e.g. <type>reflect</type>."
}

// ─── delivery ───────────────────────────────────────────────────

/// Hand `text` to `target` as a fresh turn. Exactly python's `_deliver`
/// (core.py:469-475).
///
/// A caller-supplied `system_prompt` rides along so a chain the CALLER opened
/// with its own convention keeps that convention for every hop; when the caller
/// supplied none the key is absent and each hop rebuilds the substrate's default
/// block from the live tree, which is the point of rebuilding it every turn.
///
/// NO `_call_stack`. python's `_deliver` does not carry one either, so python's
/// recursion guard is inert on this path — deliberately: propagating it would
/// make a `reply_to:'self'` chain read as a cycle on turn 2 and kill the exact
/// thing this module exists for. Pinned by `theSelfChainCarriesNoCallStack`.
func deliverTurn(
    kernel: Kernel,
    target: String,
    text: String,
    clientId: String,
    systemPrompt: String?,
    born: Int? = nil
) async {
    var payload: OrderedDictionary<String, JSON> = [:]
    payload["type"] = .string("send")
    payload["text"] = .string(text)
    payload["client_id"] = .string(clientId)
    if let born, AIIncarnation.of(kernel, target) != born {
        // A LATE REPLY TO A PREVIOUS INCARNATION. Kept as a ROW — it is real
        // data — but it does not wake a turn: the agent that made the call is
        // gone, and the one that replaced it is on other work. The TARGET writes
        // it (a store keys every row by its writer), so it rides a `send` marked
        // `late`. Python `_deliver`.
        payload["late"] = .bool(true)
    }
    if let systemPrompt = systemPrompt {
        payload["system_prompt"] = .string(systemPrompt)
    }
    let out = await kernel.send([Item(AgentId(target), .object(payload))])[0]

    // ANNOUNCE A FAILED HANDOFF. If the destination could not take the turn —
    // it is gated, gone, or its own send answered `{"error": …}` — say so on its
    // inbox instead of dropping the reply.
    //
    // Discarding it made a chain that DIED mid-flight identical to a model that
    // chose to stop: no error anywhere, the last turn simply never happened.
    // Only the first send's error was ever visible, i.e. never the ones that
    // matter. This does NOT throw: the routing dispatch is detached, and
    // unwinding here would kill sibling branches that are still fine.
    //
    // THE ANNOUNCEMENT IS AN ORDINARY `status` EVENT — `{type:'status', source,
    // phase:'done', detail:{reason, error}, ts, client_id}`, the shape reflect's
    // `emits` documents and the shape a UI/harness subscribes to. Rust and swift
    // put `reason`/`error` at the TOP level until 2026-09-02, so every subscriber
    // reading `ev.detail.reason` saw nothing: the silent-death regression this
    // announcement exists to end, wearing the announcement's own clothes.
    // FAILED IS TWO THINGS AND NOT A THIRD. python: the outcome status is
    // `failed`/`timed_out`, OR the reply carries a truthy `error`. A CANCELLED
    // handoff is NOT announced — an operator kill is a decision, and announcing
    // it woke the target with an error for work someone had deliberately
    // stopped. Swift's `!isCompleted` announced cancels too.
    let failedStatus = out.status == .failed || out.status == .timedOut
    let errored = pyTruthy(out.result["error"])
    if failedStatus || errored {
        // `reply.get("error") or out["status"]` — the RAW status, with no
        // `delivery ` prefix python never writes.
        let why: JSON = errored ? out.result["error"] : .string(out.status.rawValue)
        // THE SHARED EMITTER, so the announcement carries the target's own
        // `send_id`/`started_at`/`queue_depth` like every other status event;
        // this site built the envelope by hand and carried none of them.
        await emitStatus(
            kernel: kernel, agentId: AgentId(target), clientId: clientId, phase: "done",
            detail: [("reason", .string("error")), ("error", why)])
    }
}

/// How to get a reply's body again, in ONE line: the replying agent's own
/// `origin` when it wrote one (a file_bridge `read` names path, range and version);
/// otherwise the call that produced it — re-sending it may not give the same
/// bytes, and the line says so. Python `_origin_line`.
func originLine(_ result: JSON, target: String, verb: String, payload: JSON) -> String {
    if case .string(let o) = result["origin"], !pyStrip(o).isEmpty {
        let flat = pySplitWhitespace(o).joined(separator: " ")
        return String(String.UnicodeScalarView(flat.unicodeScalars.prefix(300)))
    }
    var args: OrderedDictionary<String, JSON> = [:]
    for (k, v) in payload.asObject ?? [:] where !["type", "client_id", "_call_stack"].contains(k) {
        args[k] = v
    }
    let dumped = pyJsonDumps(.object(args), ensureAscii: false)
    let shown = String(String.UnicodeScalarView(dumped.unicodeScalars.prefix(200)))
    return "\(target) \(verb) \(shown) — re-send it; the output may differ"
}

/// Hand `text` to `target` as a fresh turn, stamped with the incarnation the
/// target has NOW — python's `_deliver_soon`, which reads `_incarnation[target]`
/// as it schedules the delivery so `_deliver` can tell a reply that outlived
/// its agent from one that belongs to it.
func deliverSoon(
    kernel: Kernel, target: String, text: String, clientId: String, systemPrompt: String?
) {
    let born = AIIncarnation.of(kernel, target)
    // NOT AWAITED, AND A `ps` ROW. A send to an AI agent takes its FIFO lock and
    // does not return until that whole turn is done, so awaiting each delivery
    // inside the router's loop made reply 2 of a 4-call turn wait for the entire
    // conversation reply 1 started — and if the chain ended first it was never
    // delivered at all (measured 2026-08-24, `live_build`: 26 of 36 calls never
    // answered). Registered as a kernel task, so the work is a `ps` row and
    // `cancel` reaches it — fire-and-forget without that is the orphan bug.
    kernel.spawnTask(agent: AgentId(target), verb: "deliver_reply") {
        await deliverTurn(
            kernel: kernel, target: target, text: text, clientId: clientId,
            systemPrompt: systemPrompt, born: born)
    }
}

// ─── the route ──────────────────────────────────────────────────

/// Honour where this pass's output goes. Runs in its own task, after the FIFO
/// guard is released. Mirrors python's `_route_reply` step for step — the ORDER
/// below is load-bearing throughout.

// THE NO-CALL NUDGE — a turn that dispatched nothing is TRY AGAIN. DONE is
// EXPLICIT: the `<call name="done">` control call ends a chain silently and is
// never nudged; `await` waits. Every silent turn is reminded, up to
// `AIConstants.MAX_SILENT_TURNS` in a row (see `AISilentStreaks`). Mirrors
// python `_NO_CALL_NUDGE` (canonical; reinstated 2026-09-01 after
// repo_map:swift twice stopped mid-task).
let noCallNudge =
    "[no call was dispatched] Your turn emitted no <call> element, so "
    + "NOTHING ran — a call decided in thinking does not execute unless the "
    + "element is written out. If the task is unfinished, emit that call now. "
    + "If it is finished, end the chain explicitly: <call name=\"done\"></call>"

/// What a caller is told when a chain gives up (`n` silent turns in a row).
/// Python `_GAVE_UP`, which renders the module's `MAX_SILENT_TURNS`; `n` is the
/// cap in effect, so a test that lowers the cap reads its own number.
func gaveUp(_ n: Int) -> String {
    "The LLM was not able to answer — \(n) turns in a row produced "
        + "no call. Reiterate the inquiry."
}

/// Prefixed to the reminder when the turn it answers was DEAD (only thinking).
/// Python `_DEAD_TURN_LINE`.
let deadTurnLine =
    "[no answer] Your last turn produced only thinking and no answer — it was "
    + "discarded. "

/// THE SILENT STREAK per chain — `(agent id, client id)` — turns in a row that
/// dispatched nothing. Python `_silent`.
///
/// KEYED BY KERNEL, like `AIRunStates`: python's `_silent` is a module dict in a
/// one-kernel-per-process runtime, while swift hosts many kernels at once (every
/// test does), and a flat map would let one kernel's quiet agent count against
/// another's.
enum AISilentStreaks {
    private struct Chain: Hashable {
        let agent: String
        let client: String
    }
    private static let lock = NSLock()
    nonisolated(unsafe) private static var table: [UInt64: [Chain: Int]] = [:]
    nonisolated(unsafe) private static var caps: [UInt64: Int] = [:]

    /// One more silent turn on this chain; the new length of the streak.
    static func bump(_ kernel: Kernel, _ agent: String, _ client: String) -> Int {
        lock.lock()
        defer { lock.unlock() }
        let key = Chain(agent: agent, client: client)
        let n = (table[kernel.serial]?[key] ?? 0) + 1
        table[kernel.serial, default: [:]][key] = n
        return n
    }

    /// The chain dispatched something (or gave up): its streak is over.
    static func reset(_ kernel: Kernel, _ agent: String, _ client: String) {
        lock.lock()
        table[kernel.serial]?.removeValue(forKey: Chain(agent: agent, client: client))
        lock.unlock()
    }

    /// The cap in effect for this kernel — `AIConstants.MAX_SILENT_TURNS`
    /// unless a test lowered it for its own kernel.
    static func cap(_ kernel: Kernel) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return caps[kernel.serial] ?? AIConstants.MAX_SILENT_TURNS
    }

    /// TEST SEAM — python's `monkeypatch.setattr(core, "MAX_SILENT_TURNS", 3)`,
    /// scoped to ONE kernel so tests running in parallel keep the real cap.
    static func setCap(_ kernel: Kernel, _ n: Int) {
        lock.lock()
        caps[kernel.serial] = n
        lock.unlock()
    }

    /// Drop a dead kernel's rows — see `AISideTables`.
    static func forget(_ kernel: Kernel) {
        lock.lock()
        table.removeValue(forKey: kernel.serial)
        caps.removeValue(forKey: kernel.serial)
        lock.unlock()
    }
}

/// What `await` is told when there is nothing to wait for. DELIVERED TO SELF, as
/// a fresh turn: it must WAKE the model, not sit in a return value nobody reads.
let awaitRefused =
    "[await refused] nothing of yours is in flight and nothing is queued for you, "
    + "so there is nothing to wait for. Act — make the call — or end the chain "
    + "with <call name=\"done\"></call>."

/// ROUTING TURNS with a live batch, per agent — the first summand of `await`'s
/// `pending`. It counts TURNS, not calls: what the model is waiting for is a
/// batch's answer, and a batch is answered whole.
actor InFlightTurns {
    private var counts: [String: Int] = [:]

    func bump(_ agent: String, _ by: Int) {
        counts[agent, default: 0] = max(0, (counts[agent] ?? 0) + by)
    }

    func count(_ agent: String) -> Int { counts[agent] ?? 0 }
}

let inFlightTurns = InFlightTurns()

/// ONE COPY PER RECIPIENT. A `<to>` list is not a group address the kernel knows
/// about — there is no such thing; every send reaches exactly one agent. What
/// the list buys is that each copy carries the WHOLE list, so a recipient can
/// see who else heard it and reply to the same group instead of forking a
/// private conversation the others never see.
func fanOut(_ calls: [ToolCall], messageId: Int64? = nil) -> [ToolCall] {
    var out: [ToolCall] = []
    for call in calls {
        guard case .object(let args) = call.arguments else {
            out.append(call)
            continue
        }
        var recipients: [String] = []
        if case .some(.array(let items)) = args["to"] {
            recipients = items.compactMap(\.asString).map(pyStrip).filter { !$0.isEmpty }
        }
        if call.name != "send" || recipients.isEmpty {
            // A single `send` of a turn still carries `msg` — the row that sent
            // it — unless the model named one itself. Python `_route_reply`.
            if call.name == "send", let messageId, case .some(.object(var payload)) = args["payload"],
                payload["type"]?.asString == "send"
            {
                if payload["msg"] == nil { payload["msg"] = .integer(messageId) }
                if let re = args["re"] { payload["re"] = re }
                var copy = args
                copy["payload"] = .object(payload)
                copy.removeValue(forKey: "re")
                out.append(ToolCall(id: call.id, name: "send", arguments: .object(copy)))
                continue
            }
            out.append(call)
            continue
        }
        let re = args["re"]
        var basePayload: OrderedDictionary<String, JSON> = [:]
        if case .some(.object(let p)) = args["payload"] { basePayload = p }
        for r in recipients {
            var payload = basePayload
            // The WHOLE list, in every copy — that is what makes it one message.
            payload["to"] = .array(recipients.map { JSON.string($0) })
            // `msg` is the sending row's id: what a reply's `re` names.
            if let messageId { payload["msg"] = .integer(messageId) }
            if let re { payload["re"] = re }
            var copy: OrderedDictionary<String, JSON> = [:]
            for (k, v) in args
            // `to` became the fan-out, `re` moved into the payload, and
            // `target_id` is the recipient this copy goes to.
            where k != "to" && k != "re" && k != "target_id" && k != "payload" {
                copy[k] = v
            }
            copy["target_id"] = .string(r)
            copy["payload"] = .object(payload)
            out.append(ToolCall(id: call.id, name: "send", arguments: .object(copy)))
        }
    }
    return out
}

/// One EMITTED thing, in emission order — python's `units` tuple
/// `(dest, target, verb, item_index, body)`. `idx` links a dispatched call to its
/// outcome; `body` is pre-decided for the paths that never dispatch (a parse
/// error, a missing target, a missing payload, the `@body` guard, the binary
/// carrier).
private struct RouteUnit {
    let dest: String
    let target: String
    let verb: String
    let idx: Int?
    let body: String?
    /// The model's own one-sentence `<title>` for this call. It rides on the
    /// reply's tag and is what REMAINS once the body is stripped from the view,
    /// so it is the call's name forever.
    var title: String? = nil
}

/// One rendered reply block, owed to `dest`. `deliverGrouped` composes the tag —
/// including the batch stamp, which lives INSIDE it.
private struct RouteBlock {
    let dest: String
    let pid: String
    let target: String
    let verb: String
    let title: String?
    /// The assistant turn that MADE these calls, once the row store exists to
    /// number turns. It rides as `\u{2191}<turn>` so a delivered reply names its
    /// own parent and a chain stays attributable across interleaved work.
    var messageId: Int? = nil
    let body: String
}


/// Honour where this pass's output goes.
///
/// ONE TURN, ONE BATCH, ONE MESSAGE PER DESTINATION. Every call the model emitted
/// is dispatched in a SINGLE `kernel.send([...])` — concurrent, side-effect order
/// across calls undefined — and everything owed to one `reply_to` destination
/// (results, parse errors, shape errors) arrives as ONE delivered turn, each piece
/// its own `[reply to …]` block, stamped `N of M in batch <token>` when M > 1.
///
/// Swift routed calls ONE AT A TIME until 2026-09-02, delivering NO_TARGET /
/// no-payload errors as separate immediate turns: the model answered a lone error
/// in a full turn of its own before the batch it belonged to even ran, the calls
/// were serial rather than concurrent, and M excluded the errors — so the count the
/// model was given did not describe what it received.
///
/// `sendTimeout` is the AI layer's per-call deadline. The KERNEL defaults nothing;
/// this layer always supplies one, and the model may override it per call with a
/// `timeout` arg in the envelope.
func routeReply(
    selfId: AgentId,
    kernel: Kernel,
    text: String,
    clientId: String,
    sendTo: String?,
    systemPrompt: String?,
    incoming: String,
    sendTimeout: Double,
    runState: AIAgentRunState? = nil,
    messageId: Int64? = nil,
    deadTurn: Bool = false
) async {
    let calls = fanOut(extractToolCalls(text), messageId: messageId)
    let doneCalled = calls.contains { $0.name == "done" }
    let awaitCalled = calls.contains { $0.name == "await" }
    let anyCalls = !calls.isEmpty
    // Everything owed to ONE destination, in the order the model wrote it.
    var units: [RouteUnit] = []
    var items: [Item] = []

    // An envelope that OPENED and did not parse gets an error, exactly like a call
    // missing its `target_id` does. Reported even when other calls in the same pass
    // parsed fine — a partially-read turn dispatched half the model's intent, and
    // which half was lost is not something it can infer from the replies it got.
    // Goes to SELF: `reply_to` lives inside the body we could not read, and the
    // documented default for an absent `reply_to` is self anyway. TAGGED like
    // everything else (`[reply to - · - -]`): python tags every unit, so an error
    // block is counted in M and reads like its siblings.
    for bad in malformedCalls(text) {
        units.append(
            RouteUnit(dest: selfId.value, target: "-", verb: "-", idx: nil, body: bad.body))
    }
    // THE SILENT STREAK of this chain (one per agent + client thread): turns in
    // a row that dispatched nothing. Any call — `done` and `await` included —
    // ends the streak; see `AIConstants.MAX_SILENT_TURNS`. Python resets at the
    // same point, on `calls or units` (the malformed units are all there is yet).
    if anyCalls || !units.isEmpty {
        AISilentStreaks.reset(kernel, selfId.value, clientId)
    }

    for call in calls {
        if call.name == "done" || call.name == "await" || call.name == "inquiry_answer" {
            // CONTROL CALLS, not dispatches. `done` ends the chain;
            // `inquiry_answer` hands this turn's answer back to whoever sent the
            // inquiry (the `inquiry` verb reads it). Neither is a send, neither
            // is an error, and neither is ever nudged.
            continue
        }
        let args = call.arguments.asObject ?? [:]
        // `target_id` OR `target`. python's `or` skips a FALSY `target_id`, so a
        // model that wrote the key and left it empty still gets its `target`
        // read; `pyTruthy` keeps that fall-through honest for every JSON
        // shape rather than only for strings.
        let targetValue: JSON = {
            let primary = args["target_id"] ?? .null
            if pyTruthy(primary) { return primary }
            return args["target"] ?? .null
        }()

        // `reply_to` DEFAULTS TO SELF. An omitted key means the model gets its
        // own answer back, not that the answer is thrown away.
        //
        // The old default was fire-and-forget, and it made the one failure a
        // model can never debug: gemma4:12b once left the key out of an
        // otherwise perfect call, the reflect ran, its result was dropped, and
        // the chain died at pass 1 with no reply, no error, and nothing to see.
        // From inside the model that is indistinguishable from being finished.
        //
        // Stopping is still the default and still costs nothing — it is emitting
        // NO call. A dropped key now costs a wasted turn instead of the whole
        // task, and discarding a reply became something you must SAY
        // (`reply_to: null` / "none"), which is the right way round.
        //
        // SWIFT TRAP: presence must be read off the dictionary. `args["x"]` on a
        // `JSON` returns `.null` for a MISSING key (JSON.swift:114-126), so a
        // subscript cannot tell "absent" from "explicitly null" — and those two
        // mean OPPOSITE things here. Getting it wrong silently inverts the
        // default.
        var replyTo: String?
        if let raw = args["reply_to"] {
            // A non-string `reply_to` is not an address; python maps it to None
            // rather than coercing a guess. A string that trims to empty lands on
            // the same falsy path python's `""` takes.
            let trimmed = raw.asString?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            if trimmed.isEmpty || ["none", "null", "nobody"].contains(trimmed.lowercased()) {
                replyTo = nil
            } else {
                replyTo = trimmed
            }
        } else {
            replyTo = selfId.value
        }
        if replyTo == "self" { replyTo = selfId.value }

        let rawTarget = targetValue.asString ?? ""
        let target = rawTarget.trimmingCharacters(in: .whitespacesAndNewlines)
        if targetValue.asString == nil || target.isEmpty {
            if let replyTo = replyTo {
                units.append(
                    RouteUnit(
                        dest: replyTo, target: "-", verb: "-", idx: nil,
                        body: renderReply(
                            .object([
                                "error": .string(NO_TARGET_ERROR),
                                "you_sent": .object(argsWithoutPayload(args)),
                            ]))))
            }
            continue
        }

        // AN EMPTY PAYLOAD IS NO PAYLOAD. The grammar always produces the key
        // (every child that is not routing IS the payload), so `{}` is exactly
        // what a send with no verb looks like — and dispatching it reaches the
        // kernel as `payload.type required`, an error about OUR shape rather
        // than about the verb the model left out.
        guard var payload = args["payload"]?.asObject, !payload.isEmpty else {
            if let replyTo = replyTo {
                units.append(
                    RouteUnit(
                        dest: replyTo, target: target, verb: "-", idx: nil,
                        body: renderReply(
                            .object([
                                "error": .string(noPayloadError(target: target)),
                                "target": .string(target),
                                "you_sent": .object(argsWithoutPayload(args)),
                            ]))))
            }
            continue
        }

        // `payload.get("type", "?")`, rendered by `str()` at tag time: a
        // non-string `<type>` still names the reply (`· a 42 ·`, `· a True ·`),
        // and `?` means the key was ABSENT — not that it held something odd.
        let verb = payload["type"].map { pyStr($0) } ?? "?"

        // A SELF-CALL INHERITS ITS CONVERSATION. The model's calls are dispatched
        // here, AFTER the turn closed and the in-flight entry was cleared — so a
        // verb that resolves its conversation from that entry (`recall`, `grep`,
        // `history`) saw the DEFAULT client and searched an empty store.
        if target == selfId.value && payload["client_id"] == nil {
            payload["client_id"] = .string(clientId)
        }

        // NO `send_binary`, NO FENCE, NO BLOB LIFT. The document rides in the
        // payload as `bytes`, verbatim, because the grammar carries it verbatim
        // — and file_bridge's text verbs take exactly that (`write`/`edit`/
        // `append` REFUSE a `bytes` that is not a string). Lifting it
        // into the item's binary channel would delete the key the target reads
        // and route to the binary handler, which is not where a document goes.

        // A handler that RAISES must not take the chain down with it. Python ran
        // bare here until 2026-08-02, when a `file_bridge` whose ingress rule
        // could not be resolved raised out of its handler: the exception escaped
        // routing, asyncio logged "Task exception was never retrieved", and the
        // model got no reply, no error, and no further turn — the run simply
        // stopped mid-verification and looked finished. An exception is a result
        // too; the model can act on `{"error": ...}` and can act on nothing at
        // all.
        //
        // SWIFT: `kernel.send` never throws — the substrate's dispatch already
        // catches a bundle's error and answers `{error, raised:true}`
        // (KernelDispatch.swift). So the try/except python needs is the kernel's,
        // and all routing adds is the `target`, which the kernel does not know is
        // interesting.
        //
        // THE BATCH SEND IS THE ONLY SEND. Every dispatched call carries the
        // layer's deadline (the kernel defaults nothing, so a handler that never
        // returns used to hang the batch forever), set BEFORE the model's own
        // `timeout` arg overrides it. The model may also NAME the execution
        // (dedup/join across retries) and declare a retry budget; the kernel
        // validates all three and a bad value settles that one call as a refusal
        // the model can read.
        // THE `bytes` ARGUMENT IS RAW BYTES, NOT A JSON STRING (A13.19/D15).
        // Python's parser returns `raw.encode("utf-8")` for the one argument
        // named `bytes`, so the value that reaches a handler is TYPED — which is
        // why `file_bridge.write_stream` can refuse `bytes: "abc"` from a wire
        // client while a model's `<bytes>` document still writes. Swift's `JSON`
        // has no bytes case, so the item's BINARY CHANNEL is that type: the key
        // leaves the JSON header and the document rides beside it, raw. Carrying
        // it as a JSON string made the two indistinguishable, and swift wrote
        // `abc` where python refused.
        var document: Data? = nil
        if case .string(let text)? = payload["bytes"] {
            document = Data(text.utf8)
            payload.removeValue(forKey: "bytes")
        }
        var item = Item(AgentId(target), .object(payload), binary: document)
        item.timeout = sendTimeout
        // `exec_id`, not `id`: arguments are INLINE now, so a bare `<id>` would
        // collide with the many verbs whose payload has one (`delete_agent`,
        // `update_agent`, …). The parser keeps the name at the call level; here
        // it becomes the item's execution name — forwarded RAW (C38), tag-typed
        // and untouched, so the KERNEL validates it: an int `exec_id` reaches
        // python's join-by-int semantics, and a malformed `timeout`/`retries`
        // reaches the kernel's exact `bad_arg` refusal. This used to coerce with
        // `.asString`/`.asDouble`/`.asInt`, which silently DROPPED a value of
        // the wrong shape (never attempting the join python performs for an int
        // exec_id; defaulting a bad timeout to "wait forever" instead of
        // refusing) — a caller's mistake vanished instead of being reported.
        if let execId = args["exec_id"] { item.rawId = execId }
        if let t = args["timeout"] { item.rawTimeout = t }
        if let r = args["retries"] { item.rawRetries = r }
        items.append(item)
        // Fire-and-forget: it acted, nobody awaits the answer. Deliberately NOT
        // counted in M — the model is owed a count of what it will receive.
        if let replyTo = replyTo {
            units.append(
                RouteUnit(
                    dest: replyTo, target: target, verb: verb, idx: items.count - 1, body: nil,
                    // The model's own `<title>` rides the reply's tag — python
                    // `_clean_title(args.get("title"))`. Swift dropped it here, so
                    // no delivered reply ever named what it answered: a probe
                    // matching member rows by title found the call turn and not
                    // its reply (E73 misleading_fold, 3/3 on swift, 2026-09-13).
                    title: cleanTitle(args["title"])))
        }
    }

    // BRACKETED so `await` can see it: while this batch is out, the agent has
    // something in flight, and that is the honest answer to "is anything
    // coming?" — the alternative the model reaches for otherwise is re-issuing
    // the call, which only lengthens the queue that delays it.
    let hadItems = !items.isEmpty
    if hadItems { await inFlightTurns.bump(selfId.value, 1) }
    let outcomes = items.isEmpty ? [] : await kernel.send(items)
    if hadItems { await inFlightTurns.bump(selfId.value, -1) }
    // THE GRANTS CACHE IS WRITE-THROUGH: a share/unshare this turn made on its
    // own store changes who may read row bodies — reload before the next row
    // event. A store that fails the reload leaves the cache empty: the next body
    // event simply carries none — no grant is assumed. Python `_route_reply`.
    let myStore = kernel.agent(selfId)?.metaValue(forKey: "memory_store_id")
    if let myStore,
        items.contains(where: {
            JSON.string($0.target.value) == myStore
                && ["share", "unshare"].contains($0.payload["type"].asString ?? "")
        })
    {
        forgetGrants(selfId.value, kernel)
        _ = try? await myGrants(selfId.value, kernel)
    }

    // The result — INCLUDING an error — goes back. A call that failed silently is
    // the one thing a model can never debug.
    //
    // PID, then target AND verb. Target+verb alone is NOT unique: with two calls in
    // flight to one agent `[reply to kernel_state set]` is unmatchable — measured
    // 2026-08-07 (opus re-issued both deletes, then theorised "duplicate delivery"
    // about its own retries) and again 2026-08-20 (the model: "the reply queue is
    // running ahead of me", then re-polled forever). The PID is the same id
    // `ps`/`cancel` use, so a reply names a row the model can act on.
    // A bare test kernel has no record: python reads `{}` there and `context_async`
    // is simply off.
    let me = kernel.agent(selfId)
    let cap = me.map(AIContext.inlineCap) ?? 0
    var blocks: [RouteBlock] = []
    for u in units {
        var pid = "-"
        var body = u.body ?? ""
        if let i = u.idx {
            let outcome = outcomes[i]
            pid = outcome.pid.map { String($0.value) } ?? "-"
            // THE RESULT, UNCHANGED. Swift injected a `target` key into a raised
            // handler's body; python renders what the kernel settled and nothing
            // else, and the tag already names the target.
            body = renderReply(outcome.result)
            // Only reflect bodies are dropped later, so only they carry the
            // distillation notice.
            if u.verb == "reflect" { body += REFLECT_NUDGE }
            // A reply is DATA, not a question — the model needs to be able to see
            // it, not forced to swallow it whole as its next turn. Anything over
            // the inline cap is held in RAM under its pid and replaced by its
            // digest; this is what makes an unbounded reply (a 240KB ledger dump)
            // non-fatal. SYNC OR ASYNC ALIKE (decided 2026-09-26): a body is never
            // persisted. Over the inline cap it waits in RAM and the stored turn
            // carries its origin line; under it, it is inline. Measured AFTER the
            // nudge rode on, exactly where python counts it.
            if let p = outcome.pid,
                AIContext.estimateOne(.object(["role": .string("user"), "content": .string(body)])) > cap
            {
                // `len(body)` — code points, as python counts a str.
                let size = body.unicodeScalars.count
                if storeReply(selfId.value, kernel, pid: p.value, body: body) {
                    // THE STUB IS THE ORIGIN. The body is held in RAM for this
                    // incarnation only and never persisted; what survives is the
                    // one line saying how to get it again — the reply's own
                    // `origin` when its agent wrote one, else the call itself.
                    body =
                        "(settled, \(size) bytes \u{2014} NOT inlined, held in memory only "
                        + "until this agent restarts. origin: "
                        + originLine(outcome.result, target: u.target, verb: u.verb, payload: items[i].payload)
                        + ". To find a line in it: send yourself <type>grep</type> "
                        + "<query>substring</query> <ids><item>\(p.value)</item></ids>)"
                }
            }
        }
        blocks.append(
            RouteBlock(
                dest: u.dest, pid: pid, target: u.target, verb: u.verb, title: u.title,
                body: body))
    }

    // THE TAIL BELOW IS PYTHON'S ORDER, step for step (core.py:1160-1176), and
    // every step of it is load-bearing.
    //
    // A CHAIN turn (its incoming text was a delivered reply, or a nudge) that
    // dispatched NOTHING is REMINDED AGAIN — the model is queried until it emits a
    // call, `done`, or a question. Until 2026-09-01 the reminder fired ONCE and a
    // second silent turn ended the chain silently (python-canonical fix: a
    // planning-heavy model wrote prose for two turns and the run read as finished
    // with the task untouched). A chat turn is not a chain: prose to a person is
    // the product. Bounded by `nudge_limit` on the record (default `nudgeLimit`) so
    // an unattended kernel cannot spin forever on a model that never calls.
    //
    // THE GATE IS `not calls and not units`, not `no blocks`: a turn whose calls
    // all carried `reply_to:null` dispatched real work and is owed no reply.
    //
    // An inquiry turn was the OTHER legitimate silent end (emit + stop; the human's
    // answer resumes the chain) — nudging it would stomp the wait.
    let anyBlocks = !blocks.isEmpty
    // EVERY CALL-LESS TURN IS NUDGED, chain or not (user, 2026-09-09: "all is
    // tool calls"). This used to fire only when the incoming text was itself a
    // reply or a nudge, so a FIRST turn of pure prose from a person ended the
    // work silently — the model had decided on a call, written it in thought,
    // and never emitted it.
    //
    // AN INQUIRY IS A CALL NOW, so there is no silent-turn exemption: a turn that
    // consults someone DISPATCHES and is never silent. This used to scan the
    // model's own prose for the old `{"type":"question"}` line — a text convention
    // had to recognise, with the human living outside it entirely.
    let nudging = !anyCalls && !anyBlocks
    if anyBlocks {
        await deliverGrouped(
            kernel: kernel, blocks: blocks, clientId: clientId, systemPrompt: systemPrompt, messageId: messageId)
    }
    if doneCalled {
        // Explicitly finished — nothing more runs, nothing reminds. A caller's
        // `send_to` hand-off is the one thing that still applies: the finished
        // text is what it asked to have delivered. It is ALSO the only turn on
        // which it can still apply, now that a call-less turn is nudged instead
        // of ending.
        if let sendTo = sendTo, !anyBlocks {
            deliverSoon(
                kernel: kernel, target: sendTo, text: text,
                clientId: clientId, systemPrompt: systemPrompt)
        }
        return
    }
    if awaitCalled {
        // EXPLICIT WAITING. The model stops and nothing reminds it: the next
        // message that lands wakes it. That is the honest alternative to
        // re-issuing a call it has not seen answered — this is an ASYNC system,
        // and a reply in flight is on its way, not lost.
        //
        // PENDING is three summands, and all three are needed: this agent's own
        // in-flight routing turns, the messages already queued for it, and the
        // turn another message may ALREADY be running under the FIFO lock.
        // Routing happens AFTER the turn released that lock, so "queued" alone
        // would read empty at exactly the moment something is about to arrive.
        let queued = runState?.queueDepth() ?? 0
        let running = (await runState?.fifo.busy() ?? false) ? 1 : 0
        let pending = await inFlightTurns.count(selfId.value) + queued + running
        if pending > 0 || hadItems {
            await emitStatus(
                kernel: kernel, agentId: selfId, clientId: clientId, phase: "done",
                detail: [
                    ("reason", .string("awaiting")), ("pending", .integer(Int64(pending))),
                ])
            return
        }
        // Nothing to wait for: say so, as a turn, so the model acts instead of
        // waiting forever on an empty inbox.
        deliverSoon(
            kernel: kernel, target: selfId.value, text: awaitRefused,
            clientId: clientId, systemPrompt: systemPrompt)
        return
    }
    if nudging {
        // TRY AGAIN. A call-less turn means "you emitted no call, try again". A
        // DEAD turn (only thinking, discarded unstored) is the same silence and
        // is rerolled the same way, told why.
        //
        // BOUNDED (owner, 2026-09-30): MAX_SILENT_TURNS in a row and the chain
        // GIVES UP instead of reminding again — the caller is told, loudly, that
        // the model could not answer. A person sees it in their view; a parent
        // that asked for the result by `send_to` gets it as a turn; a clock's
        // tick has nobody reading, so the status event is the record, and the
        // next tick starts a fresh chain with a fresh count.
        let streak = AISilentStreaks.bump(kernel, selfId.value, clientId)
        let cap = AISilentStreaks.cap(kernel)
        if streak >= cap {
            AISilentStreaks.reset(kernel, selfId.value, clientId)
            let text = gaveUp(cap)
            await emitStatus(
                kernel: kernel, agentId: selfId, clientId: clientId, phase: "done",
                detail: [
                    ("reason", .string("gave_up")), ("error", .string(text)),
                    ("silent_turns", .integer(Int64(streak))),
                ])
            if let sendTo = sendTo {
                deliverSoon(
                    kernel: kernel, target: sendTo, text: text,
                    clientId: clientId, systemPrompt: systemPrompt)
            }
            return
        }
        deliverSoon(
            kernel: kernel, target: selfId.value,
            text: deadTurn ? deadTurnLine + noCallNudge : noCallNudge,
            clientId: clientId, systemPrompt: systemPrompt)
        return
    }
    if anyCalls { return }
    // No call of the model's own, so the CALLER's one-shot handoff applies: hand
    // this pass's TEXT to `sendTo`, once. It must not propagate — a caller cannot
    // spend the model's turns for it.
    if let sendTo = sendTo {
        deliverSoon(
            kernel: kernel, target: sendTo, text: text,
            clientId: clientId, systemPrompt: systemPrompt)
    }
}

/// ONE delivered message per destination: blocks joined by a blank line, each
/// tagged `[reply to <pid> · <target> <verb>]` — with ` · N of M in batch <token>`
/// composed INSIDE that tag when the destination is owed more than one.
///
/// INSIDE, not a trailing line. The stamp rode on its own `[N of M in batch tok]`
/// line after the body until 2026-09-02, which is neither what the shared prompt
/// teaches nor what python's history trimmers can read (`_REPLY_TAG` /
/// `_BLOCK_SPLIT` match the tag at the START of a block).
///
/// The stamp is for CORRELATION and COMPLETENESS, not "the rest are still in
/// flight" — the batch arrives whole. A model that made 2 calls and got 1 back
/// could not tell "still running" from "answered the wrong one".
private func deliverGrouped(
    kernel: Kernel, blocks: [RouteBlock], clientId: String, systemPrompt: String?,
    messageId: Int64? = nil
) async {
    guard !blocks.isEmpty else { return }
    // Destination order = first appearance, so the model reads them in the order
    // it wrote the calls.
    var order: [String] = []
    var byDest: [String: [RouteBlock]] = [:]
    for b in blocks {
        if byDest[b.dest] == nil { order.append(b.dest) }
        byDest[b.dest, default: []].append(b)
    }
    // The batch token IS the assistant row's id, so `in batch <b>` and `↑<turn>`
    // name the same thing and the model can see the row every reply points at.
    // (A fresh random token only when no turn made these calls.)
    let token = messageId.map { String($0) } ?? String(format: "%04x", UInt16.random(in: .min ... .max))
    for dest in order {
        let parts = byDest[dest] ?? []
        let m = parts.count
        let body = parts.enumerated().map { (i, b) -> String in
            // The ONE line a reply is known by, forever — in the durable store,
            // in `recall` pages, and as the stub a stripped body leaves behind.
            // Optional segments append in THIS order:
            //     [reply to <pid> · <target> <verb> · "<title>" · N of M in batch <b>]
            //     [reply to <pid> · <target> <verb> · "<title>" · ↑<turn> · N of M in batch <b>]
            var tag = "[reply to \(b.pid) \u{b7} \(b.target) \(b.verb)"
            if let t = b.title { tag += " \u{b7} \"\(t)\"" }
            if let messageId { tag += " \u{b7} \u{2191}\(messageId)" }
            tag += m > 1 ? " \u{b7} \(i + 1) of \(m) in batch \(token)]" : "]"
            return tag + "\n" + b.body
        }.joined(separator: "\n\n")
        deliverSoon(
            kernel: kernel, target: dest, text: body,
            clientId: clientId, systemPrompt: systemPrompt)
    }
}

/// A title lives INSIDE the reply tag, so it may not carry the one character
/// that closes it, nor a newline that would end the tag line. Capped at 140
/// characters: it is a sentence, not a body.
/// CODE POINTS, and python's strip: swift read `\r\n` as ONE Character (so a CRLF
/// title kept its line break and ended the tag line), stripped with
/// `.whitespacesAndNewlines` (which leaves U+001C..U+001F), and cut 140
/// GRAPHEMES — a family-emoji title of 30 clusters is 150 scalars and python
/// cuts it where swift did not.
func cleanTitle(_ raw: JSON?) -> String? {
    guard case .some(.string(let text)) = raw, !pyStrip(text).isEmpty else { return nil }
    var out = String.UnicodeScalarView()
    var gap = false
    for c in text.unicodeScalars {
        if c == "]" || c == "\r" || c == "\n" {
            gap = true
        } else {
            // A RUN becomes ONE space, wherever it sits — python's
            // `re.sub(r"[\]\r\n]+", " ", t)` writes it at the front and the end
            // too, and the strip below is what removes those.
            if gap { out.append(" ") }
            gap = false
            out.append(c)
        }
    }
    if gap { out.append(" ") }
    let capped = pyStrip(String(out)).unicodeScalars.prefix(140)
    let title = String(String.UnicodeScalarView(capped))
    return title.isEmpty ? nil : title
}

/// The call's args minus `payload`, for the `you_sent` echo — showing the model
/// what it actually wrote. The payload is dropped because it can be arbitrarily
/// large (a whole program) and it is never the thing that was missing.
private func argsWithoutPayload(
    _ args: OrderedDictionary<String, JSON>
) -> OrderedDictionary<String, JSON> {
    var out: OrderedDictionary<String, JSON> = [:]
    for (k, v) in args where k != "payload" { out[k] = v }
    return out
}
