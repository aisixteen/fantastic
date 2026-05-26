// Substrate-native verbs: create_agent, delete_agent, update_agent,
// list_agents, get.
//
// Mirrors Rust's `send.rs::handle_system_verb` + `lifecycle.rs`.

import FantasticJSON
import Foundation
import OrderedCollections

extension Kernel {
    /// Dispatch a substrate verb. Caller has already established
    /// the `currentSender` task-local scope.
    func handleSystemVerb(target: Agent, verb: String, payload: JSON) async -> JSON {
        switch verb {
        case "list_agents":
            return listAgentsReply()
        case "create_agent":
            return await createFromPayload(parent: target, payload: payload)
        case "delete_agent":
            return await deleteFromPayload(caller: target, payload: payload)
        case "update_agent":
            return await updateFromPayload(caller: target, payload: payload)
        case "get":
            return getAgentReply(payload: payload)
        default:
            return .object(["error": .string("unhandled system verb \(verb)")])
        }
    }

    // ── list_agents ─────────────────────────────────────────────

    private func listAgentsReply() -> JSON {
        var rows: [JSON] = []
        for a in allAgents() {
            do {
                let data = try JSONEncoder().encode(a.record())
                rows.append(try JSON.parse(data))
            } catch {
                continue
            }
        }
        rows.sort {
            ($0["id"].asString ?? "") < ($1["id"].asString ?? "")
        }
        return .object(["agents": .array(rows)])
    }

    // ── get ─────────────────────────────────────────────────────

    private func getAgentReply(payload: JSON) -> JSON {
        guard let idStr = payload["id"].asString else {
            return .null
        }
        guard let a = agent(AgentId(idStr)) else {
            return .null
        }
        guard let data = try? JSONEncoder().encode(a.record()),
            let json = try? JSON.parse(data)
        else {
            return .null
        }
        return json
    }

    // ── create_agent ────────────────────────────────────────────

    func createFromPayload(parent: Agent, payload: JSON) async -> JSON {
        guard let hm = payload["handler_module"].asString else {
            return .object(["error": .string("create_agent requires handler_module")])
        }
        // Mint an id if caller didn't supply one. Convention from
        // Rust: <bundle>_<6 hex chars>.
        let id: String
        if let supplied = payload["id"].asString, !supplied.isEmpty {
            id = supplied
        } else {
            id = mintId(hm)
        }
        let newId = AgentId(id)
        if agent(newId) != nil {
            return .object(["error": .string("agent \"\(id)\" already exists")])
        }

        // Compose meta from extra payload fields.
        var meta: OrderedDictionary<String, JSON> = [:]
        if case let .object(dict) = payload {
            for (k, v) in dict {
                if k == "type" || k == "id" || k == "handler_module" || k == "parent_id" {
                    continue
                }
                meta[k] = v
            }
        }

        let rootPath = parent.childrenDir.appendingPathComponent(id)
        let newAgent = Agent(
            id: newId,
            handlerModule: hm,
            parentId: parent.id,
            meta: meta,
            rootPath: rootPath
        )

        // Disk-mode persist; in-memory no-op.
        do {
            try Persistence.persist(agent: newAgent, storage: storage)
        } catch {
            return .object(["error": .string("persist: \(error)")])
        }
        if let bundle = bundles.get(hm), let readme = bundle.readme {
            try? Persistence.seedReadme(agent: newAgent, content: readme, storage: storage)
        }

        register(newAgent)
        parent.insertChild(newAgent)

        let event: JSON = .object([
            "type": .string("created"),
            "id": .string(newAgent.id.value),
            "parent_id": .string(parent.id.value),
            "handler_module": .string(hm),
        ])
        publishState(event)

        let recordJson = recordToJSON(newAgent.record())

        // Fire `boot` on the new agent. Failures are logged via the
        // reply but don't undo the create — matches Rust's behaviour.
        let bootReply = await send(newAgent.id, .object(["type": .string("boot")]))
        if let err = bootReply["error"].asString {
            // Telemetry-only — boot errors don't cancel the create.
            _ = err
        }

        // agent_created lifecycle event on the parent's inbox.
        await emit(
            parent.id,
            .object([
                "type": .string("agent_created"),
                "id": .string(newAgent.id.value),
                "agent": recordJson,
            ]))

        return recordJson
    }

    // ── delete_agent ────────────────────────────────────────────

    func deleteFromPayload(caller: Agent, payload: JSON) async -> JSON {
        guard let idStr = payload["id"].asString else {
            return .object(["error": .string("delete_agent requires id")])
        }
        let id = AgentId(idStr)
        guard let target = agent(id) else {
            return .object(["error": .string("no agent \"\(idStr)\"")])
        }
        if let blocker = findLockedDescendant(of: target) {
            return .object([
                "error":
                    .string(
                        "delete_agent: \(id.value) blocked by delete_lock on descendant \(blocker.value)"
                    ),
                "locked": .bool(true),
                "id": .string(id.value),
                "blocked_by": .string(blocker.value),
            ])
        }
        await cascadeDelete(target: target)
        await emit(
            caller.id,
            .object([
                "type": .string("agent_deleted"),
                "id": .string(id.value),
            ]))
        return .object([
            "deleted": .bool(true),
            "id": .string(id.value),
        ])
    }

    private func findLockedDescendant(of target: Agent) -> AgentId? {
        if target.isDeleteLocked { return target.id }
        for cid in target.childIds() {
            if let child = agent(cid) {
                if let blocker = findLockedDescendant(of: child) {
                    return blocker
                }
            }
        }
        return nil
    }

    func cascadeDelete(target: Agent) async {
        // Children first (post-order traversal so on_delete runs
        // bottom-up — same as Rust's lifecycle.rs cascade_delete).
        for cid in target.childIds() {
            if let child = agent(cid) {
                await cascadeDelete(target: child)
            }
        }
        // Fire bundle.on_delete hook (best-effort).
        if let hm = target.handlerModule, let bundle = bundles.get(hm) {
            try? await bundle.onDelete(agentId: target.id, kernel: self)
        }
        // Detach from parent's children map.
        if let parentId = target.parentId, let parent = agent(parentId) {
            parent.removeChild(target.id)
        }
        // Unregister from kernel + disk.
        unregister(target.id)
        try? Persistence.remove(agent: target, storage: storage)

        let event: JSON = .object([
            "type": .string("removed"),
            "id": .string(target.id.value),
        ])
        publishState(event)
    }

    // ── update_agent ────────────────────────────────────────────

    func updateFromPayload(caller: Agent, payload: JSON) async -> JSON {
        guard let idStr = payload["id"].asString else {
            return .object(["error": .string("update_agent requires id")])
        }
        let id = AgentId(idStr)
        guard let target = agent(id) else {
            return .object(["error": .string("no agent \"\(idStr)\"")])
        }
        // Patch = every field except type + id.
        var patch: OrderedDictionary<String, JSON> = [:]
        var changed: [String] = []
        if case let .object(dict) = payload {
            for (k, v) in dict where k != "type" && k != "id" {
                patch[k] = v
                changed.append(k)
            }
        }
        let rec = target.updateMeta(patch)
        try? Persistence.persist(agent: target, storage: storage)
        let recJson = recordToJSON(rec)

        let event: JSON = .object([
            "type": .string("updated"),
            "id": .string(target.id.value),
            "changed": .array(changed.map { .string($0) }),
            "agent": recJson,
        ])
        publishState(event)

        // Fire agent_updated on caller's inbox so watchers refresh.
        await emit(
            caller.id,
            .object([
                "type": .string("agent_updated"),
                "id": .string(target.id.value),
                "changed": .array(changed.map { .string($0) }),
                "agent": recJson,
            ]))

        return recJson
    }
}

// ── Helpers ───────────────────────────────────────────────────────

func recordToJSON(_ rec: AgentRecord) -> JSON {
    guard let data = try? JSONEncoder().encode(rec),
        let json = try? JSON.parse(data)
    else {
        return .null
    }
    return json
}

/// Mint an id like `<bundle>_<6 hex>`. Mirrors Rust's `mint_id`.
private func mintId(_ bundle: String) -> String {
    let prefix = bundle.replacingOccurrences(of: ".tools", with: "")
    let hex = String(format: "%06x", Int.random(in: 0..<0x100_0000))
    return "\(prefix)_\(hex)"
}

extension Kernel {
    /// `reflect` reply for an agent with no `handler_module` (the
    /// root and other bare agents). Mirrors Rust's
    /// `crate::reflect::reflect` for the bare path.
    func reflectBare(_ agent: Agent) -> JSON {
        var children: [JSON] = []
        for cid in agent.childIds() {
            if let c = self.agent(cid) {
                children.append(
                    .object([
                        "id": .string(c.id.value),
                        "handler_module": c.handlerModule.map { .string($0) } ?? .null,
                    ]))
            }
        }
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string("Bare agent (no handler_module) — answers substrate verbs only."),
            "handler_module": .null,
            "children": .array(children),
        ])
    }
}
