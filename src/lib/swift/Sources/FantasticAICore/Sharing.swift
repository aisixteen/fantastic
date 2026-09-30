// CONTEXT BY GRANT, and the ROW EVENT — the ai_core verbs of the memory-store
// era: `fork_from`, `request_share`, `watch`, `unwatch`.
//
// Port of python `ai_core/core.py` (canonical): `_fork_from`, `_request_share`,
// `_watch`, `_unwatch`.
//
// A model shares a scope of its OWN rows with another agent by calling its
// store's `share` (an explicit choice, recorded); the reader starts from it with
// `fork_from`, which appends ONE `fork` row naming the grant and KICKSTARTS the
// fork with the caller's task message. The kernel holds no rows, so an agent
// announces its own rows to its watchers (`watch`, gated by the agent's own call
// gate).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

extension AIBackend {
    /// args: owner:str (req), grant:int (req — the grant `owner` gave you),
    /// message:str (req — the task this fork starts on), client_id:str?. Makes
    /// THIS agent a fork of `owner`'s thread: the granted rows become read-only
    /// context in your view — read back under the grant every turn, so a revoked
    /// grant fails your next turn — followed by the framing line and `message`,
    /// and the turn STARTS AT ONCE. Returns {forked:true, fork:<row id>, rows:N}.
    /// Python `_fork_from`.
    func forkFrom(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
        let id = agent.id.value
        let ownerRaw = payload["owner"]
        let message = payload["message"]
        for (name, val) in [("owner", ownerRaw), ("message", message)] {
            guard case .string(let v) = val, !pyStrip(v).isEmpty else {
                return .object([
                    "error": .string("fork_from: `\(name)` is required"),
                    "reason": .string("missing_arg"), "arg": .string(name),
                ])
            }
        }
        guard let grant = aiAsId(payload["grant"]) else {
            return .object([
                "error": .string("fork_from: `grant` (an int) is required"),
                "reason": .string("missing_arg"), "arg": .string("grant"),
            ])
        }
        let owner = pyStrip(ownerRaw.asString ?? "")
        // THE OWNER IS THE GRANT'S, NOT THE CALLER'S WORD: the label every shared
        // row renders under ("[shared from <owner>]") must name who really gave
        // the grant.
        let given: JSON
        do {
            given = try await storeCall(
                id, kernel, ["type": .string("grants"), "side": .string("received")])
        } catch let e as MemoryStoreError {
            return .object([
                "error": .string(e.message), "reason": .string("not_found"),
                "arg": .string("grant"),
            ])
        }
        guard let g = (given["grants"].asArray ?? []).first(where: { $0["grant"].asInt == grant })
        else {
            return .object([
                "error": .string("fork_from: no grant \(grant) was given to you"),
                "reason": .string("not_found"), "arg": .string("grant"),
            ])
        }
        if g["owner"].asString != owner {
            return .object([
                "error": .string(
                    "fork_from: grant \(grant) was given by \(pyStr(g["owner"])), not \(owner)"),
                "reason": .string("bad_arg"), "arg": .string("owner"),
            ])
        }
        let res: JSON
        do {
            res = try await storeCall(
                id, kernel, ["type": .string("read"), "grant": .integer(grant)])
        } catch let e as MemoryStoreError {
            return .object([
                "error": .string(e.message), "reason": .string("not_found"),
                "arg": .string("grant"),
            ])
        }
        let shared = res["rows"].asArray ?? []
        let asOf = shared.map { $0["id"].asInt ?? 0 }.max() ?? 0
        let rid = kernel.mintId()
        try await saveHistory(
            id, kernel,
            [
                .object([
                    "role": .string("user"),
                    "id": .integer(rid),
                    "parent": .null,
                    "kind": .string("fork"),
                    "owner": .string(owner),
                    "grant": .integer(grant),
                    "as_of": .integer(asOf),
                    "content": .string(
                        "[fork of \(owner) \u{b7} grant \(grant) \u{b7} as of row \(asOf) "
                            + "\u{b7} \(shared.count) rows shared read-only]"),
                ])
            ])
        let clientId = safeClient(pyTruthy(payload["client_id"]) ? pyStr(payload["client_id"]) : "")
        let text =
            forkFraming(owner: owner, asOf: String(asOf)) + "\n\n" + (message.asString ?? "")
        deliverSoon(kernel: kernel, target: id, text: text, clientId: clientId, systemPrompt: nil)
        return .object([
            "forked": .bool(true), "fork": .integer(rid), "rows": .integer(Int64(shared.count)),
        ])
    }

    /// args: row:int (req), why:str?. Asks THIS agent — the OWNER of `row` — to
    /// share it with the caller. The owner's model decides: it calls its store's
    /// `share`, or declines. Arrives as a turn; returns {requested:true} at once.
    /// Python `_request_share`.
    func requestShare(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        guard let row = aiAsId(payload["row"]) else {
            return .object([
                "error": .string("request_share: `row` (an int) is required"),
                "reason": .string("missing_arg"), "arg": .string("row"),
            ])
        }
        let caller = KernelTaskLocals.currentOrigin?.value ?? "kernel"
        let whyRaw = payload["why"]
        let collapsed = pySplitWhitespace(pyTruthy(whyRaw) ? pyStr(whyRaw) : "").joined(separator: " ")
        let why = String(String.UnicodeScalarView(collapsed.unicodeScalars.prefix(300)))
        let store = pyStr(agent.metaValue(forKey: "memory_store_id") ?? .null)
        let text =
            "[request_share from \(caller) \u{b7} row \(row)] \(why)\n"
            + "\(caller) asks you to share row \(row) of YOUR conversation. To share it: send "
            + "your memory store `\(store)` <type>share</type> <with>\(caller)</with> "
            + "<scope><row>\(row)</row></scope> <why>…</why>, then tell \(caller) the grant "
            + "id. To decline, tell \(caller) so."
        deliverSoon(
            kernel: kernel, target: agent.id.value, text: text, clientId: AIConstants.DEFAULT_CLIENT_ID, systemPrompt: nil)
        return .object([
            "requested": .bool(true), "owner": .string(agent.id.value), "row": .integer(row),
        ])
    }

    /// args: body:bool? (default false), watcher:str? (operator only — an inbox id
    /// to deliver to). Announce every row this agent appends to YOUR inbox as
    /// {type:'row', source, row:{id, kind, role, bytes}} — content-free; with
    /// body:true the row's `content` too, but only for rows a live grant from this
    /// agent gives you (share). Needs a registered inbox. Returns {watching:true,
    /// watcher, body}. Python `_watch`.
    func watchVerb(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
        let caller = KernelTaskLocals.currentOrigin?.value
        var watcher: String
        if let caller {
            let w = payload["watcher"]
            if w != .null && w != .string(caller) {
                return .object([
                    "error": .string("watch: an agent watches for ITSELF — `watcher` is the operator's"),
                    "reason": .string("unauthorized"),
                ])
            }
            watcher = caller
        } else {
            guard case .string(let w) = payload["watcher"], !w.isEmpty else {
                return .object([
                    "error": .string("watch: the operator must name a `watcher` inbox"),
                    "reason": .string("missing_arg"), "arg": .string("watcher"),
                ])
            }
            watcher = w
        }
        guard kernel.hasInbox(AgentId(watcher)) else {
            return .object([
                "error": .string(
                    "watch: \(pyRepr(watcher)) has no registered inbox, so nothing would read the row events"),
                "reason": .string("no_consumer"),
                "hint": .string(
                    "register the reader first — ctx.ensure_inbox(\(pyRepr(watcher))) — then watch"),
            ])
        }
        let body = pyTruthy(payload["body"])
        setWatcher(agent.id.value, kernel, watcher, body: body)
        if body {
            try await myGrants(agent.id.value, kernel)  // warm: the body check reads the cache
        }
        return .object([
            "watching": .bool(true), "watcher": .string(watcher), "body": .bool(body),
        ])
    }

    /// args: watcher:str? (operator only). Stop announcing rows to you. Returns
    /// {watching:false}. Python `_unwatch`.
    func unwatchVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        let watcher: JSON =
            KernelTaskLocals.currentOrigin.map { JSON.string($0.value) } ?? payload["watcher"]
        if case .string(let w) = watcher { setWatcher(agent.id.value, kernel, w, body: nil) }
        return .object(["watching": .bool(false), "watcher": watcher])
    }
}
