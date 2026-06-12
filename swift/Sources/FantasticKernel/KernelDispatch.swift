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

        // Universal reflect post-process: compose every reflect reply
        // with the tree/bundles/readme flags + the `description` field.
        // Applied to bundle reflects and bare-agent reflects alike, so
        // the surface is uniform. Mirrors Python's `_apply_reflect_flags`
        // / Rust's `apply_reflect_flags`.
        if verb == "reflect" {
            applyReflectFlags(target, payload, &reply)
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

    /// Send a binary-framed verb — the symmetric binary channel (raw bytes BOTH
    /// ways, never base64; mirrors py/rust). `header` carries the verb envelope,
    /// `blob` the raw request body; returns `(reply, body)` where `body` is the
    /// raw reply chunk (empty for a text-shaped reply, e.g. `write_stream`).
    public func sendWithBinary(
        _ targetId: AgentId, _ header: JSON, _ blob: Data
    ) async -> (JSON, Data) {
        let target: Agent? = targetId.value == "kernel" ? root : agent(targetId)
        guard let target = target else {
            return (.object(["error": .string("no agent \(targetId)")]), Data())
        }
        let verb = header["type"].asString ?? ""
        let (reply, body): (JSON?, Data) = await KernelTaskLocals.$currentSender.withValue(
            target.id
        ) {
            await dispatchBinary(target: target, header: header, blob: blob)
        }
        // Telemetry parity with text dispatch.
        publishState(
            .object([
                "type": .string("send_binary"),
                "sender": .string((KernelTaskLocals.currentSender ?? target.id).value),
                "target": .string(target.id.value),
                "verb": .string(verb),
                "bytes": .integer(Int64(max(blob.count, body.count))),
            ]))
        fanoutToWatchers(target, header)
        return (reply ?? .null, body)
    }

    private func dispatchBinary(target: Agent, header: JSON, blob: Data) async -> (JSON?, Data) {
        guard let hm = target.handlerModule else {
            return (
                .object([
                    "error": .string("agent \(target.id) has no handler_module; cannot answer binary")
                ]), Data()
            )
        }
        guard let bundle = bundles.get(hm) else {
            return (.object(["error": .string("no bundle for handler_module \"\(hm)\"")]), Data())
        }
        do {
            return try await bundle.handleBinary(
                agentId: target.id, header: header, blob: blob, kernel: self)
        } catch {
            return (.object(["error": .string("\(error)")]), Data())
        }
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

    /// Append the composable reflect flags to any reflect reply —
    /// applied uniformly to bare-agent and bundle reflects. `tree`
    /// defaults to `all`, `bundles` to `none`, `readme` to false.
    /// Non-object replies pass through.
    func applyReflectFlags(_ target: Agent, _ payload: JSON, _ reply: inout JSON) {
        guard case .object(var obj) = reply else { return }
        // `description` is a substrate meta field — surface it on every
        // reflect (bundle handlers don't know about it) unless already set.
        if obj["description"] == nil, let d = target.descriptionMeta {
            obj["description"] = .string(d)
        }
        // Kernel runtime identity + deployment context — surfaced on the ROOT
        // reflect so a client that hops to this kernel learns, in one
        // round-trip: which runtime (`runtime`), WHERE it runs (`env` —
        // "container" when launched from the image, else "host"), and which
        // build (`version`). env/version come from the optional FANTASTIC_ENV /
        // FANTASTIC_VERSION envs the container bakes in; RUN-scoped (never
        // persisted to the portable .fantastic workdir). Same field names + key
        // order (runtime → env → version) across all four runtimes.
        if target.parentId == nil {
            obj["runtime"] = .string("swift")
            obj["env"] = .string(ProcessInfo.processInfo.environment["FANTASTIC_ENV"] ?? "host")
            obj["version"] =
                ProcessInfo.processInfo.environment["FANTASTIC_VERSION"].map { JSON.string($0) }
                ?? .null
            // WHICH file_bridge the loader auto-persists records THROUGH (the
            // discovered store), or null = nothing wired (state in RAM). With the
            // provider's posture inline in the tree, a client sees whether
            // persistence is wired AND whether the wired leg is open.
            obj["persistence"] = .object([
                "provider": findStore().map { JSON.string($0.value) } ?? .null
            ])
        }
        switch payload["tree"].asString ?? "all" {
        case "all": obj["tree"] = treeNode(target)
        case "ids": obj["tree"] = .array(descendantIds(target).map { .string($0) })
        default: break  // "none" → omit
        }
        switch payload["bundles"].asString ?? "none" {
        case "all":
            obj["bundles"] = .array(
                availableBundles().map {
                    .object([
                        "name": .string($0.name),
                        "handler_module": .string($0.handlerModule),
                    ])
                })
        case "ids":
            obj["bundles"] = .array(availableBundles().map { .string($0.name) })
        default: break  // "none" → omit
        }
        if payload["readme"].asBool == true {
            obj["readme"] = readReadme(target)
        }
        reply = .object(obj)
    }

    /// The addressed agent's readme.md (string) or null. On disk, read
    /// the seeded file; for the root with no on-disk file (in-memory
    /// mode), fall back to the embedded canonical readme so
    /// `reflect readme=true` returns the same bytes the seed would.
    private func readReadme(_ target: Agent) -> JSON {
        if storage.isDisk,
            let s = try? String(contentsOf: target.readmeFile, encoding: .utf8),
            !s.isEmpty
        {
            return .string(s)
        }
        if target.parentId == nil {
            return .string(RootReadme.text)
        }
        return .null
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
                return reflectIdentity(target)
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
