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
        // The live TASK TABLE, kernel-wide (like `list_agents`), not per-agent:
        // one row per in-flight dispatch, `parent` chaining them into the send
        // tree. `ps` shows its own row too — it IS a dispatch. The table is honest.
        case "ps":
            return .object(["tasks": .array(tasks.rows())])
        // Kill a task AND ITS SUBTREE (children first, like killing a process
        // group). Each bundle's `onCancel` hook runs BEFORE the abort so real
        // resources (OS subprocesses, PTYs, HTTP streams) die with it. An unknown
        // or already-finished id returns {cancelled:false} — it does not pretend.
        case "cancel":
            guard let t = payload["task_id"].asString, !t.isEmpty else {
                return .object(["error": .string("cancel: task_id (str) required")])
            }
            return await cancelTask(TaskId(t))
        default:
            return .object(["error": .string("unhandled system verb \(verb)")])
        }
    }

    /// One line per system verb — the honest reflect surface. Bundle verbs are
    /// derived from each bundle's own reflect; these are the SUBSTRATE's own, so
    /// they appear in no bundle's map and are advertised on the ROOT reflect (see
    /// `applyReflectFlags`). Mirrors Python's `_SYSTEM_VERB_DOCS` / Rust's
    /// `system_verb_docs`.
    static func systemVerbDocs() -> JSON {
        .object([
            "create_agent": .string(
                "args: handler_module:str, id:str?, **meta — spawn a child agent + boot it."),
            "update_agent": .string("args: id:str, **meta — patch a record's meta."),
            "delete_agent": .string(
                "args: id:str — cascade-delete an agent + its subtree (delete_lock refuses)."),
            "list_agents": .string("No args. Every agent's record, flat."),
            "get": .string("args: id:str — one agent's record (null if unknown)."),
            "ps": .string(
                "No args. The live TASK TABLE — {tasks:[{id, agent, parent, verb, started_at}]}. "
                    + "One row per in-flight dispatch; `parent` chains them into the send tree."),
            "cancel": .string(
                "args: task_id:str — kill that task AND ITS SUBTREE (children first). "
                    + "Returns {cancelled:bool, killed:[task_id]}; the blocked caller's `send` "
                    + "resolves to {cancelled:true}. Unknown/finished id ⇒ {cancelled:false}."),
        ])
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
        // `more` is not decoration (py-canonical, measured 2026-08-07): a model that
        // opened with `list_agents` got bare records and never learned authoring was
        // the mechanism; the arm that opened with `reflect` got `self_extension` and
        // built. This verb rides every reflect's `system_verbs` — it needs a
        // signpost back to the briefing.
        return .object([
            "agents": .array(rows),
            "more": .string(
                "Raw records — how agents are WIRED, not what they are FOR, and no "
                    + "manual. {type:'reflect'} on the root is the briefing: each agent's "
                    + "purpose, plus `self_extension` (how to author your own durable tools "
                    + "in THIS kernel — which executor, and the fields without which the "
                    + "agent is created and never runs) and `readmes` (the manual index). "
                    + "Reflect the root before building anything."),
        ])
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
        if case .object(let dict) = payload {
            for (k, v) in dict {
                if k == "type" || k == "id" || k == "handler_module" || k == "parent_id" {
                    continue
                }
                meta[k] = v
            }
        }

        // The structural authority baseline ({$self,$parent,$children} on both
        // `can_call` and `callable_by`) is stamped + merged uniformly in `Agent.init`
        // for EVERY agent — so it is NOT re-stamped here; an explicit grant in the
        // payload rides in `meta` and merges over the baseline at construction.

        // WRITE-TIME VALIDATION, same gate as update_agent (py-canonical: create
        // used to bypass it, so a record could be born with meta its own bundle
        // refuses). Record is empty at create: the merged view IS the patch.
        if let bundle = bundles.get(hm), let why = bundle.validateMeta(meta, record: [:]) {
            return .object(["error": .string(why), "created": .bool(false)])
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
        // Operator authority: booting the child is a side effect of the already-
        // authorized create_agent, not the parent reaching out — so it never runs
        // against the parent's grant.
        let bootReply = await KernelTaskLocals.$currentSender.withValue(nil) {
            await send(newAgent.id, .object(["type": .string("boot")]))
        }
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
        if case .object(let dict) = payload {
            for (k, v) in dict where k != "type" && k != "id" {
                patch[k] = v
                changed.append(k)
            }
        }
        // WRITE-TIME VALIDATION. Ask the target's bundle whether it can honour this
        // patch before anything changes — a refusal names the bad field at the point
        // of the mistake instead of surfacing calls later. Mirrors py/rust.
        if let hm = target.handlerModule, let bundle = bundles.get(hm),
            let why = bundle.validateMeta(patch, record: target.meta)
        {
            return .object([
                "error": .string(why), "id": .string(idStr), "updated": .bool(false),
            ])
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

    /// Every script executor this kernel COULD advertise, paired with the
    /// receive shape its connector really implements. The ONE table the
    /// root `self_extension` reads — a hard-coded module name there went
    /// wrong in both directions: on macOS the process-edge executor (registered
    /// in Startup under `#if os(macOS)`) was never named, and on a non-Apple
    /// build the string named an in-proc executor, which needed
    /// JavaScriptCore and is not registered there at all. Swift ships no lua
    /// executor, so none is listed.
    static let scriptExecutors: [(handlerModule: String, receiveShape: String)] = [
        (
            "exec_bridge.tools",
            "its script receives turns via kernel.on_message(callback) and must keep running; the leg's reflect advertises its interpreter allowlist and shims"
        )
    ]

    /// The script executors actually REGISTERED in this kernel — the table
    /// above intersected with `availableBundles()`, in table order.
    func scriptExecutorsHere() -> [(handlerModule: String, receiveShape: String)] {
        let registered = Set(availableBundles().map(\.handlerModule))
        return Kernel.scriptExecutors.filter { registered.contains($0.handlerModule) }
    }

    /// The `self_extension` tail naming this build's executor(s). Empty
    /// intersection is stated, not papered over: with no executor registered
    /// there is no handler_module that runs authored `code`, and a caller
    /// must learn that from the same round-trip.
    func executorClause() -> String {
        let live = scriptExecutorsHere()
        if live.isEmpty {
            return
                "This build ships NO script executor - no registered handler_module runs authored `code`, so create_agent cannot make a tool here until one is registered."
        }
        let named = live.map { "\($0.handlerModule) (\($0.receiveShape))" }
            .joined(separator: ", ")
        return live.count == 1 ? "Executor here: \(named)." : "Executors here: \(named)."
    }
}
