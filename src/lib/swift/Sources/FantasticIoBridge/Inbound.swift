// The INBOUND verbs of a wire leg (`web_ws`, `web_rest`) — how an admitted frame
// becomes the leg's own call. Mirror of python `io_bridge/_inbound.py`.
//
// IDENTITY COMES ONLY FROM DISPATCH (owner 2026-10-04). A transport's loop (the
// HTTP server, a websocket's frame lane) runs outside every dispatch, so what it
// sends is the operator's. It never sends a frame onward itself: it hands the
// admitted frame to its LEG as one of these verbs (`handIn`), and the leg's
// handler performs the onward call — so that call is the leg's, gated as the leg
// (its `can_call`, the target's `callable_by`). Whoever is on the other end of
// the wire is not an identity in this kernel; the leg is.
//
// The leg's `ingress_rule` has already judged the frame (`gateWebLeg`) before the
// transport hands it in. These verbs are gated like any other: the transport's
// hand-in is the operator's (ungated); an agent calling them needs the grant on
// the leg like any verb.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public enum Inbound {
    /// The four verbs, python's docstrings (`reflect`'s `verbs` lines).
    public static let verbDocs: OrderedDictionary<String, JSON> = [
        "inbound_call": .string(
            "args: item:{target, payload, id?} (req). Sends `item` onward AS THIS LEG and returns the onward call's reply — how an admitted inbound call frame becomes the leg's own call, gated as the leg."
        ),
        "inbound_emit": .string(
            "args: target:str (req), payload:dict (req). Emits `payload` on `target` AS THIS LEG (gated as the leg, verb `emit`) — how an admitted inbound emit frame becomes the leg's own emit."
        ),
        "inbound_watch": .string(
            "args: src:str (req — an agent id), reader:str (req — a registered inbox). Mirrors `src`'s traffic into `reader` AS THIS LEG (gated as the leg, verb `watch`). Returns {watching:src} or the refusal."
        ),
        "inbound_state_subscribe": .string(
            "No args. May THIS LEG tap the kernel-wide state stream (gated on the root, verb `state_subscribe`)? Returns {subscribed:true} or the refusal; the transport then wires the tap."
        ),
    ]

    /// Is `verb` one of the inbound verbs?
    public static func handles(_ verb: String) -> Bool { verbDocs[verb] != nil }

    /// Answer one inbound verb, run INSIDE the leg's own dispatch (so every send
    /// and emit below is the leg's). `body` is the request's raw body at its
    /// payload path (`["item", "payload", …]` for a call, `["payload", …]` for an
    /// emit); `rawReply` the binary channel with no request body — the onward
    /// reply's chunk comes back raw. The reply's chunk rides beside the reply.
    public static func handle(
        agentId: AgentId, payload: JSON, body: Blob?, rawReply: Bool, kernel: Kernel
    ) async -> (JSON?, Blob?) {
        switch payload["type"].asString ?? "" {
        case "inbound_call":
            return await call(payload, body: body, rawReply: rawReply, kernel: kernel)
        case "inbound_emit":
            return (emit(payload, body: body, kernel: kernel), nil)
        case "inbound_watch":
            return (watch(payload, kernel: kernel), nil)
        case "inbound_state_subscribe":
            let denial = kernel.authorizeStateSubscribe(agentId)
            return (denial ?? .object(["subscribed": .bool(true)]), nil)
        default:
            return (nil, nil)
        }
    }

    /// `inbound_call`: send `item` onward as this leg; the onward outcome's reply
    /// (and chunk) is the answer.
    private static func call(_ payload: JSON, body: Blob?, rawReply: Bool, kernel: Kernel) async
        -> (JSON?, Blob?)
    {
        guard case .object(let it)? = payload.asObject?["item"] else {
            return (
                Refusal.bad("inbound_call", "item", "a batch item {target, payload, id?}"), nil
            )
        }
        // The item exactly as the transport built it — the kernel judges every
        // field of it, as python's `kernel.send([item])` does.
        var inner = it["payload"] ?? .null
        let innerBody = body?.under("item")?.under("payload")
        // The body's slot keeps `null` in the payload (the transport placed it
        // there; the leg's own dispatch handed this handler the header without
        // it), so the onward call row and watcher frame keep it.
        if let b = innerBody { try? pySetPath(&inner, parts: b.parts, .null) }
        var item = Item(AgentId(it["target"]?.asString ?? ""), inner)
        if let id = it["id"], id != .null { item.rawId = id }
        if let t = it["timeout"], t != .null { item.rawTimeout = t }
        if let r = it["retries"], r != .null { item.rawRetries = r }
        if let b = innerBody {
            item.binary = b
        } else if rawReply || body != nil {
            item.rawReply = true
        }
        let out = await kernel.send([item])[0]
        return (out.result, out.body)
    }

    /// `inbound_emit`: emit `payload` on `target` as this leg (gated, verb
    /// `emit`); a raw body rides at its slot of the payload.
    private static func emit(_ payload: JSON, body: Blob?, kernel: Kernel) -> JSON? {
        guard let target = payload["target"].asString, !target.isEmpty else {
            return Refusal.bad("inbound_emit", "target", "an agent id")
        }
        guard case .object = payload.asObject?["payload"] else {
            return Refusal.bad("inbound_emit", "payload", "an object")
        }
        var inner = payload["payload"]
        if let b = body?.under("payload") {
            try? pySetPath(&inner, parts: b.parts, .null)
            kernel.emitItem(AgentId(target), .binary(inner, b))
        } else {
            kernel.emitItem(AgentId(target), .text(inner))
        }
        return nil
    }

    /// `inbound_watch`: mirror `src` into the registered `reader` as this leg
    /// (gated, verb `watch`).
    private static func watch(_ payload: JSON, kernel: Kernel) -> JSON {
        guard let src = payload["src"].asString, !src.isEmpty else {
            return Refusal.bad("inbound_watch", "src", "an agent id")
        }
        guard let reader = payload["reader"].asString, !reader.isEmpty else {
            return Refusal.bad("inbound_watch", "reader", "a registered inbox id")
        }
        if let denial = kernel.watch(src: AgentId(src), watcher: AgentId(reader)) {
            return denial
        }
        return .object(["watching": .string(src)])
    }

    /// THE TRANSPORT'S ONE SEND: hand one admitted frame to the leg `legId` as an
    /// inbound verb and return the leg's outcome. Made from the transport's loop —
    /// outside every dispatch, so it is the operator's. `binary` is the request's
    /// raw body at its payload path; with none, the reply's chunk comes back raw.
    public static func handIn(
        _ kernel: Kernel, legId: AgentId, _ payload: JSON, binary: Blob? = nil
    ) async -> Outcome {
        await kernel.send([Item(legId, payload, binary: binary, rawReply: binary == nil)])[0]
    }
}
