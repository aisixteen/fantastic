// `Kernel.send` / `Kernel.emit` + verb routing.
//
// Mirrors Rust's `send.rs` — `kernel.send(target, payload)` resolves
// the target via the flat agents map, dispatches through the system
// verb table OR the agent's bundle, publishes a state event, fans
// the payload out to watchers, returns the reply.

import FantasticJSON
import Foundation
import OrderedCollections

extension Kernel {
    /// Send a verb. Looks up `targetId`, dispatches via system verb
    /// table OR the agent's bundle. Returns the reply (or an
    /// `{"error": "..."}` JSON if the target / handler is missing).
    ///
    /// Sets `currentSender` task-local to `targetId` for the
    /// duration of dispatch, so nested sends attribute to their
    /// target (matches Rust's contextvars-style send.rs).
    public func send(_ targetId: AgentId, _ payload: JSON) async -> JSON {
        // Resolve target. Special id "kernel" aliases to the root.
        let target: Agent?
        if targetId.value == "kernel" {
            target = root
        } else {
            target = agent(targetId)
        }
        guard let target = target else {
            return .object(["error": .string("no agent \(targetId)")])
        }

        let outerSender = KernelTaskLocals.currentSender ?? target.id
        let verb = payload["type"].asString ?? ""

        // Dispatch under a fresh currentSender(target.id) scope.
        var reply = await KernelTaskLocals.$currentSender.withValue(target.id) {
            await dispatch(target: target, payload: payload)
        }

        // Reflect post-process: when payload.return_readme == true,
        // attach the target's readme.md content as reply.readme.
        if verb == "reflect", payload["return_readme"].asBool == true {
            if case .object = reply, storage.isDisk {
                let readme = (try? String(
                    contentsOf: target.readmeFile, encoding: .utf8)) ?? ""
                reply["readme"] = readme.isEmpty ? .null : .string(readme)
            }
        }

        // State event + watcher fanout.
        let event: JSON = .object([
            "type": .string("send"),
            "sender": .string(outerSender.value),
            "target": .string(target.id.value),
            "verb": .string(verb),
            "summary": .string(summarize(payload)),
        ])
        publishState(event)
        fanoutToWatchers(target, payload)

        return reply
    }

    /// Send WITH explicit sender attribution. State events tag this
    /// dispatch as originating from `senderId`. Mirrors the
    /// `send_json_as` UniFFI helper used by web_ws / proxy_agent.
    public func sendAs(sender: AgentId, target: AgentId, payload: JSON) async -> JSON
    {
        await KernelTaskLocals.$currentSender.withValue(sender) {
            await send(target, payload)
        }
    }

    /// Fire an event into `targetId`'s inbox without dispatching.
    /// Auto-vivifies the inbox for synthetic ids. Publishes a
    /// `{"type":"emit", ...}` state event + fans out to watchers.
    public func emit(_ targetId: AgentId, _ payload: JSON) async {
        deliverToInbox(targetId, payload)

        let sender = KernelTaskLocals.currentSender ?? targetId
        let verb = payload["type"].asString ?? ""
        let event: JSON = .object([
            "type": .string("emit"),
            "sender": .string(sender.value),
            "target": .string(targetId.value),
            "verb": .string(verb),
            "summary": .string(summarize(payload)),
        ])
        publishState(event)
        if let target = agent(targetId) {
            fanoutToWatchers(target, payload)
        }
    }

    // ── internals ───────────────────────────────────────────────

    private func dispatch(target: Agent, payload: JSON) async -> JSON {
        let verb = payload["type"].asString ?? ""

        if Kernel.isSystemVerb(verb) {
            return await handleSystemVerb(target: target, verb: verb, payload: payload)
        }

        // Bare agents (no handler_module): only the universal verbs
        // can answer. `reflect` is handled here for parity with Rust.
        guard let hm = target.handlerModule else {
            switch verb {
            case "boot", "shutdown":
                return .null
            case "reflect":
                return reflectBare(target)
            default:
                return .object([
                    "error":
                        .string(
                            "agent \(target.id) has no handler_module; cannot answer verb \(verb)"
                        )
                ])
            }
        }

        guard let bundle = bundles.get(hm) else {
            return .object([
                "error": .string("no bundle for handler_module \"\(hm)\"")
            ])
        }

        do {
            let reply = try await bundle.handle(
                agentId: target.id, payload: payload, kernel: self)
            return reply ?? .null
        } catch {
            return .object(["error": .string("\(error)")])
        }
    }

    /// Substrate verbs answered natively on every agent.
    /// `reflect` is intentionally NOT in this list — it's per-bundle
    /// (the bare agent path inside `dispatch` handles bare reflect).
    static func isSystemVerb(_ verb: String) -> Bool {
        switch verb {
        case "create_agent", "delete_agent", "update_agent", "list_agents", "get":
            return true
        default:
            return false
        }
    }
}

// MARK: - Telemetry helper

/// Compact, character-bounded summary for state events. Mirrors
/// Rust's `summarize_payload` (max 160 chars; UTF-8 safe truncation).
func summarize(_ payload: JSON) -> String {
    let s = payload.serialize()
    guard s.count > 160 else { return s }
    var truncated = String(s.prefix(157))
    truncated.append("...")
    return truncated
}
