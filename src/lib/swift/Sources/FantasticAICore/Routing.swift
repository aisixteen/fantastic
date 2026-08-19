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
    _ = await kernel.send(AgentId(target), .object(payload))
}

// ─── the route ──────────────────────────────────────────────────

/// Honour where this pass's output goes. Runs in its own task, after the FIFO
/// guard is released. Mirrors python's `_route_reply` step for step — the ORDER
/// below is load-bearing throughout.
func routeReply(
    selfId: AgentId,
    kernel: Kernel,
    text: String,
    clientId: String,
    sendTo: String?,
    systemPrompt: String?
) async {
    let calls = extractToolCalls(text)
    // An envelope that OPENED and did not parse gets an error, exactly like a
    // call missing its `target_id` does. Reported even when other calls in the
    // same pass parsed fine — a partially-read turn dispatched half the model's
    // intent, and which half was lost is not something it can infer from the
    // replies it got. Goes to SELF: `reply_to` lives inside the body we could not
    // read, and the documented default for an absent `reply_to` is self anyway.
    for bad in malformedCalls(text) {
        await deliverTurn(
            kernel: kernel, target: selfId.value, text: bad.serialize(),
            clientId: clientId, systemPrompt: systemPrompt)
    }
    if calls.isEmpty {
        // No call of the model's own, so the CALLER's one-shot handoff applies:
        // hand this pass's TEXT to `send_to`, once. It must not propagate — a
        // caller cannot spend the model's turns for it.
        if let sendTo = sendTo {
            await deliverTurn(
                kernel: kernel, target: sendTo, text: text,
                clientId: clientId, systemPrompt: systemPrompt)
        }
        return
    }

    for call in calls {
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
        var result = await kernel.send(AgentId(target), .object(payload))
        if result["raised"].asBool == true, var obj = result.asObject, obj["target"] == nil {
            obj["target"] = .string(target)
            result = .object(obj)
        }

        // Fire-and-forget: it acted, nobody awaits the answer.
        guard let replyTo = replyTo else { continue }
        // The result — INCLUDING an error — goes back. A call that failed
        // silently is the one thing a model can never debug.
        //
        // Target AND verb in the tag (py-canonical shape). With two calls in flight
        // to one agent a bare `[reply to kernel_state]` is unmatchable — measured
        // 2026-08-07 on py: acks lag a turn, the model re-issued both deletes and
        // read its own retries as "duplicate delivery".
        let verb = payload["type"]?.asString ?? "?"
        await deliverTurn(
            kernel: kernel, target: replyTo,
            text: "[reply to \(target) \(verb)]\n" + result.serialize(),
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
