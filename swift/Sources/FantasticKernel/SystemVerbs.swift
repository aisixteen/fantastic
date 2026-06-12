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

        // Wire into the kernel + parent FIRST, then persist. Order matters now
        // that persistence routes THROUGH a discovered file_bridge provider (a
        // child of root): a freshly-created store must already be a registered
        // child so it can persist its OWN record through itself (findStore sees
        // it). No-op in InMemory mode or when no provider is wired (RAM).
        register(newAgent)
        parent.insertChild(newAgent)
        await persistRecord(newAgent)
        if let bundle = bundles.get(hm), let readme = bundle.readme {
            await seedReadmeViaStore(newAgent, content: readme)
        }

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
        // Unregister from kernel + disk cleanup THROUGH the discovered provider
        // (its recursive `delete`) — the substrate owns no `fs` surface here.
        unregister(target.id)
        await forgetRecord(target)

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
        await persistRecord(target)
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
    /// Uniform reflect identity for a bare agent (the root, or any node
    /// with no handler_module): id + sentence + record fields + flat
    /// meta. Bundle agents build their own identity in their handler; the
    /// substrate appends tree/bundles/readme to BOTH via
    /// `applyReflectFlags`, so the root is not special-cased. Mirrors
    /// Rust's `reflect::reflect_identity` / Python's `_reflect_identity`.
    /// The wiring/posture meta keys surfaced on every reflect tree node — a
    /// leg's lock (`ingress_rule`/`egress_rule`/`auth`: allow_all = open,
    /// password = gated, deny_inbound/absent on an io leg = sealed), a
    /// file_bridge's served `root`, and what a consumer persists THROUGH
    /// (`file_bridge_id`). Mirrors py/rust `_POSTURE_KEYS`.
    static let POSTURE_KEYS = ["ingress_rule", "egress_rule", "auth", "root", "file_bridge_id"]

    func reflectIdentity(_ agent: Agent) -> JSON {
        var obj: OrderedDictionary<String, JSON> = [:]
        obj["id"] = .string(agent.id.value)
        obj["sentence"] = .string(sentenceFor(agent))
        obj["parent_id"] = agent.parentId.map { .string($0.value) } ?? .null
        obj["handler_module"] = agent.handlerModule.map { .string($0) } ?? .null
        obj["display_name"] = .string(agent.displayName ?? agent.id.value)
        if let d = agent.descriptionMeta {
            obj["description"] = .string(d)
        }
        // Flatten current meta into the reply for visibility.
        for (k, v) in agent.meta where obj[k] == nil {
            obj[k] = v
        }
        return .object(obj)
    }

    func sentenceFor(_ agent: Agent) -> String {
        agent.parentId == nil
            ? "Fantastic kernel. Everything is reachable by sending messages to agents."
            : "Bare agent (no handler_module) — answers substrate verbs only."
    }

    /// Nested distilled subtree `{id, parent_id, handler_module,
    /// display_name, description?, children}` (the `tree=all` shape;
    /// children sorted by id).
    func treeNode(_ agent: Agent) -> JSON {
        var obj: OrderedDictionary<String, JSON> = [:]
        obj["id"] = .string(agent.id.value)
        obj["parent_id"] = agent.parentId.map { .string($0.value) } ?? .null
        obj["handler_module"] = agent.handlerModule.map { .string($0) } ?? .null
        obj["display_name"] = .string(agent.displayName ?? agent.id.value)
        if let d = agent.descriptionMeta {
            obj["description"] = .string(d)
        }
        // Per-node POSTURE — surface the wiring/lock meta a node carries so the
        // whole IO landscape (which legs are open vs sealed, what's wired to
        // what) reads from ONE root reflect. Mirrors py/rust `_POSTURE_KEYS`.
        for key in Self.POSTURE_KEYS {
            if let v = agent.metaValue(forKey: key) {
                obj[key] = v
            }
        }
        let kids = agent.childIds()
            .compactMap { self.agent($0) }
            .sorted { $0.id.value < $1.id.value }
        obj["children"] = .array(kids.map { treeNode($0) })
        return .object(obj)
    }

    /// Flat id index of an agent + all descendants (DFS, self first,
    /// children sorted by id). The cheap `tree=ids` tier.
    func descendantIds(_ agent: Agent) -> [String] {
        var out = [agent.id.value]
        let kids = agent.childIds()
            .compactMap { self.agent($0) }
            .sorted { $0.id.value < $1.id.value }
        for c in kids { out.append(contentsOf: descendantIds(c)) }
        return out
    }

    /// The installable-bundle catalog as `(name, handlerModule)` pairs,
    /// sorted by name. The `bundles=all`/`bundles=ids` source.
    func availableBundles() -> [(name: String, handlerModule: String)] {
        bundles.snapshot()
            .map { (handlerModule, bundle) in (bundle.name, handlerModule) }
            .sorted { $0.name < $1.name }
    }
}
