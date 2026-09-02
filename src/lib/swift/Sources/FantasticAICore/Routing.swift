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
//   the model's own <tool_call>s — each names a target, a payload and `reply_to`.
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
    + #"Shape: {"name":"send","arguments":{"target_id":"<agent>","payload":{"type":"<verb>"}}}. "#
    + "Send `{type:'reflect'}` to the kernel for the list of agent ids."

/// NO PAYLOAD, NO SEND.
///
/// Dispatching nothing reaches the handler as nothing and raises an error about
/// OUR internals — python measured `'NoneType' object has no attribute 'get'`
/// three times in one run on 2026-08-06, naming neither the verb the model
/// omitted nor the agent it addressed. `ToolParse` translates every compressed
/// shape it can recognise; what arrives here with no payload really has no verb
/// in it, and saying so is the useful answer. Same shape as `NO_TARGET_ERROR`.
///
/// The target is quoted the way python's `{target!r}` renders an ordinary string
/// — single quotes — so the two runtimes' prompts stay the same sentence.
func noPayloadError(target: String) -> String {
    "send to '\(target)' carried no payload, so nothing was dispatched. "
        + "A payload is an object whose `type` names the verb, e.g. "
        + #"{"type":"reflect"}."#
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
    systemPrompt: String?
) async {
    var payload: OrderedDictionary<String, JSON> = [:]
    payload["type"] = .string("send")
    payload["text"] = .string(text)
    payload["client_id"] = .string(clientId)
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
    let failed = !out.isCompleted || out.result["error"].asString != nil
    if failed {
        let why = out.result["error"].asString ?? "delivery \(out.status.rawValue)"
        await kernel.emit(
            AgentId(target),
            .object([
                "type": .string("status"),
                "phase": .string("done"),
                "reason": .string("error"),
                "error": .string(why),
                "client_id": .string(clientId),
            ]))
    }
}

// ─── the route ──────────────────────────────────────────────────

/// Honour where this pass's output goes. Runs in its own task, after the FIFO
/// guard is released. Mirrors python's `_route_reply` step for step — the ORDER
/// below is load-bearing throughout.

// THE NO-CALL NUDGE — one runtime reminder when a CHAIN turn dispatched
// nothing. DONE is EXPLICIT: the `{"name":"done"}` tool call ends a chain
// silently and is never nudged. Why once: the nudge is itself a chain turn, so
// a second consecutive no-call turn arrives with the nudge as its incoming
// text — and is NOT nudged again. Mirrors python `_NO_CALL_NUDGE` (canonical;
// reinstated 2026-09-01 after repo_map:swift twice stopped mid-task).
let noCallNudge =
    "[no call was dispatched] Your turn emitted no <tool_call> envelope, so "
    + "NOTHING ran — a call decided in thinking does not execute unless the "
    + "envelope is written out. If the task is unfinished, emit that call now. "
    + "If it is finished, end the chain explicitly: <tool_call>{\"name\":\"done\"}"
    + "</tool_call>"

/// Consecutive silent chain turns the reminder tolerates before the chain is
/// let go (record meta `nudge_limit` overrides). A safety bound on an
/// unattended kernel, not a budget. Mirrors python `_NUDGE_LIMIT`.
let nudgeLimit = 24

/// (kernel instance, agent id, client id) → consecutive silent chain turns so
/// far. Scoped to the KERNEL, not the process: two kernels in one process (the
/// test suite, an app embedding several) may carry the same agent/client ids.
actor SilentStreaks {
    private var map: [String: Int] = [:]
    private func key(_ kernel: Kernel, _ agent: String, _ client: String) -> String {
        "\(ObjectIdentifier(kernel).hashValue)\u{1F}\(agent)\u{1F}\(client)"
    }
    func bump(_ kernel: Kernel, _ agent: String, _ client: String) -> Int {
        let k = key(kernel, agent, client)
        let n = (map[k] ?? 0) + 1
        map[k] = n
        return n
    }
    func reset(_ kernel: Kernel, _ agent: String, _ client: String) {
        map[key(kernel, agent, client)] = nil
    }
}

let silentStreaks = SilentStreaks()

func routeReply(
    selfId: AgentId,
    kernel: Kernel,
    text: String,
    clientId: String,
    sendTo: String?,
    systemPrompt: String?,
    incoming: String
) async {
    let calls = extractToolCalls(text)
    let doneCalled = calls.contains { $0.name == "done" }
    // An envelope that OPENED and did not parse gets an error, exactly like a
    // call missing its `target_id` does. Reported even when other calls in the
    // same pass parsed fine — a partially-read turn dispatched half the model's
    // intent, and which half was lost is not something it can infer from the
    // replies it got. Goes to SELF: `reply_to` lives inside the body we could not
    // read, and the documented default for an absent `reply_to` is self anyway.
    // Everything owed to ONE destination, in the order the model wrote it.
    var blocks: [(String, String)] = []
    for bad in malformedCalls(text) {
        blocks.append((selfId.value, bad.serialize()))
    }
    if calls.isEmpty {
        // A malformed-only turn still owes its report.
        if !blocks.isEmpty {
            await silentStreaks.reset(kernel, selfId.value, clientId)
            await deliverGrouped(
                kernel: kernel, blocks: blocks, clientId: clientId,
                systemPrompt: systemPrompt)
            return
        }
        // A CHAIN turn (its incoming text was a delivered reply, or a nudge) that
        // dispatched nothing is REMINDED AGAIN — the model is queried until it
        // emits a call, `done`, or a question. Until 2026-09-01 the reminder
        // fired ONCE and a second silent turn ended the chain silently
        // (python-canonical fix: a planning-heavy model wrote prose for two
        // turns and the run read as finished with the task untouched). A chat
        // turn is not a chain. Bounded by `nudge_limit` on the record (default
        // `nudgeLimit`) so an unattended kernel cannot spin forever.
        // A question turn is the OTHER legitimate silent end (ask + stop; the
        // human's answer resumes the chain) — nudging it would stomp the wait.
        let asked = text.contains("\"type\"") && text.contains("\"question\"")
        let chained =
            incoming.hasPrefix("[reply to ") || incoming.hasPrefix("[no call was dispatched]")
        if chained, !asked {
            let streak = await silentStreaks.bump(kernel, selfId.value, clientId)
            let limit =
                kernel.agent(selfId)?.metaValue(forKey: "nudge_limit")?.asInt.map { Int($0) }
                ?? nudgeLimit
            if streak <= limit {
                await deliverTurn(
                    kernel: kernel, target: selfId.value, text: noCallNudge,
                    clientId: clientId, systemPrompt: systemPrompt)
            } else {
                await silentStreaks.reset(kernel, selfId.value, clientId)
            }
            return
        }
        await silentStreaks.reset(kernel, selfId.value, clientId)
        // No call of the model's own, so the CALLER's one-shot handoff applies:
        // hand this pass's TEXT to `send_to`, once. It must not propagate — a
        // caller cannot spend the model's turns for it.
        if let sendTo = sendTo {
            await deliverTurn(
                kernel: kernel, target: sendTo, text: text,
                clientId: clientId, systemPrompt: systemPrompt)
            return
        }

        return
    }

    for call in calls {
        if call.name == "done" {
            // The EXPLICIT end — not a dispatch, not an error, never nudged.
            continue
        }
        let args = call.arguments.asObject ?? [:]
        // `target_id` OR `target`. python's `or` skips a FALSY `target_id`, so a
        // model that wrote the key and left it empty still gets its `target`
        // read; `pythonTruthy` keeps that fall-through honest for every JSON
        // shape rather than only for strings.
        let targetValue: JSON = {
            let primary = args["target_id"] ?? .null
            if pythonTruthy(primary) { return primary }
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
                await deliverTurn(
                    kernel: kernel, target: replyTo,
                    text: JSON.object([
                        "error": .string(NO_TARGET_ERROR),
                        "you_sent": .object(argsWithoutPayload(args)),
                    ]).serialize(),
                    clientId: clientId, systemPrompt: systemPrompt)
            }
            continue
        }

        guard let payload = args["payload"]?.asObject else {
            if let replyTo = replyTo {
                await deliverTurn(
                    kernel: kernel, target: replyTo,
                    text: JSON.object([
                        "error": .string(noPayloadError(target: target)),
                        "target": .string(target),
                        "you_sent": .object(argsWithoutPayload(args)),
                    ]).serialize(),
                    clientId: clientId, systemPrompt: systemPrompt)
            }
            continue
        }

        let verb = payload["type"]?.asString ?? "?"

        // send_binary: ONE named payload field is a file's CONTENTS, and a JSON
        // payload has no bytes type — so a stream sink (`write_stream`) that wants
        // raw bytes is otherwise unreachable. The model wrote the program as an
        // ordinary string in `into` (default `bytes`); lift that field out of the
        // header and dispatch it over the kernel's BINARY carrier (the same
        // `sendWithBinary` persistence rides), not the plain JSON send. Mirrors
        // python `core.py`'s send_binary branch, routed through swift's separate
        // binary channel. A non-string `into` field falls through to an ordinary
        // send (python leaves it as-is too).
        if call.name == "send_binary" {
            let field = args["into"]?.asString ?? "bytes"
            if case .string(let prog)? = payload[field] {
                if prog == "@body" {
                    // A LITERAL "@body" is the fence placeholder with NO fenced
                    // block — valid JSON, so it parses and would be WRITTEN AS
                    // THE FILE (measured 2026-09-01, gym: `illegal character
                    // '@'`). Refuse loud instead.
                    if let replyTo = replyTo {
                        let err: JSON = .object([
                            "error": .string(
                                "send_binary NOT dispatched: `bytes` is the literal "
                                    + "placeholder \"@body\" but no fenced block followed. "
                                    + "Re-send the SAME call with the program between "
                                    + "column-0 marker lines inside the same <tool_call>: "
                                    + "the JSON line, then `*** Begin Bytes`, the program "
                                    + "verbatim, then `*** End Bytes`."),
                            "target": .string(target),
                        ])
                        blocks.append(
                            (replyTo, "[reply to - · \(target) \(verb)]\n" + err.serialize()))
                    }
                    continue
                }
                var header = payload
                header[field] = nil  // rides as the blob, not the JSON header
                let (reply, _) = await kernel.sendWithBinary(
                    AgentId(target), .object(header), Data(prog.utf8))
                if let replyTo = replyTo {
                    blocks.append(
                        (replyTo, "[reply to - · \(target) \(verb)]\n" + reply.serialize()))
                }
                continue
            }
        }

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
        // THE BATCH SEND IS THE ONLY SEND. The model may NAME the execution
        // (dedup/join across retries), bound its own call, and declare a retry
        // budget; the kernel validates all three and a bad value settles that
        // one call as a refusal the model can read.
        var item = Item(AgentId(target), .object(payload))
        if let execId = args["id"]?.asString, !execId.isEmpty { item.id = execId }
        if let t = args["timeout"]?.asDouble { item.timeout = t }
        if let r = args["retries"]?.asInt { item.retries = UInt32(r) }
        let outcome = await kernel.send([item])[0]
        var result = outcome.result
        if result["raised"].asBool == true, var obj = result.asObject, obj["target"] == nil {
            obj["target"] = .string(target)
            result = .object(obj)
        }

        // Fire-and-forget: it acted, nobody awaits the answer. Deliberately NOT
        // counted in M — the model is owed a count of what it will receive.
        guard let replyTo = replyTo else { continue }
        // The result — INCLUDING an error — goes back. A call that failed
        // silently is the one thing a model can never debug.
        //
        // PID, then target AND verb. Target+verb alone is NOT unique either: with two
        // calls in flight to one agent `[reply to kernel_state set]` is unmatchable —
        // measured 2026-08-07 (opus re-issued both deletes, then theorised "duplicate
        // delivery" about its own retries) and again 2026-08-20 (the model: "the reply queue
        // is running ahead of me", then re-polled forever). The PID is the same id
        // `ps`/`cancel` use, so a reply names a row the model can act on.
        let pid = outcome.pid?.value ?? "-"
        blocks.append(
            (replyTo, "[reply to \(pid) · \(target) \(verb)]\n" + result.serialize()))
    }

    // ONE MESSAGE PER DESTINATION. Everything owed to one destination arrives as
    // ONE delivered turn, each piece its own block, stamped `N of M in batch
    // <tok>` when that destination is owed more than one.
    //
    // Why whole, never split: replies used to land one per turn, so a turn that
    // fanned out 7 calls spent the next 7 turns collecting its own answers.
    // Measured 2026-08-25: the model re-asked `list_agents` five times and the
    // run died with nothing built. The stamp is for CORRELATION and
    // COMPLETENESS — a model that made 2 calls and got 1 back could not tell
    // "still running" from "answered the wrong one".
    if doneCalled && blocks.isEmpty {
        await silentStreaks.reset(kernel, selfId.value, clientId)
        return  // explicitly finished — nothing more runs, nothing reminds
    }
    await deliverGrouped(
        kernel: kernel, blocks: blocks, clientId: clientId, systemPrompt: systemPrompt)
}

/// Group blocks by destination and deliver one message each.
private func deliverGrouped(
    kernel: Kernel, blocks: [(String, String)], clientId: String, systemPrompt: String?
) async {
    guard !blocks.isEmpty else { return }
    // Destination order = first appearance, so the model reads them in the order
    // it wrote the calls.
    var order: [String] = []
    var byDest: [String: [String]] = [:]
    for (dest, body) in blocks {
        if byDest[dest] == nil { order.append(dest) }
        byDest[dest, default: []].append(body)
    }
    // ONE token per TURN, shared across destinations: it names the batch, and a
    // model reading two destinations' messages can see they came from one turn.
    // Derived from the batch's own content so it needs no clock and no counter —
    // both would differ across runtimes and make a byte-diff fail on noise.
    var h = Hasher()
    for d in order {
        h.combine(d)
        h.combine(byDest[d]?.count ?? 0)
        for b in byDest[d] ?? [] { h.combine(b.count) }
    }
    let token = String(format: "%04x", UInt16(truncatingIfNeeded: h.finalize()))

    for dest in order {
        let parts = byDest[dest] ?? []
        let m = parts.count
        let body =
            m > 1
            ? parts.enumerated()
                .map { "\($0.element)\n[\($0.offset + 1) of \(m) in batch \(token)]" }
                .joined(separator: "\n\n")
            : parts.joined(separator: "\n\n")
        await deliverTurn(
            kernel: kernel, target: dest, text: body,
            clientId: clientId, systemPrompt: systemPrompt)
    }
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

/// Python's truthiness, so a ported `a or b` keeps meaning what it meant.
///
/// Only reachable from the `target_id` / `target` fall-through, where the source
/// is `args.get("target_id") or args.get("target")`: a model that wrote
/// `target_id: ""` (or `0`, or `{}`) must still have its `target` read, and a
/// model that wrote a non-empty non-string `target_id` must still land on the
/// shape-naming error rather than silently dispatching to the other key.
private func pythonTruthy(_ v: JSON) -> Bool {
    switch v {
    case .null: return false
    case .bool(let b): return b
    case .integer(let i): return i != 0
    case .double(let d): return d != 0
    case .string(let s): return !s.isEmpty
    case .array(let a): return !a.isEmpty
    case .object(let o): return !o.isEmpty
    }
}
