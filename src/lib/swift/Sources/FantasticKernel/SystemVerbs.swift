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
        // ROOT ONLY. These verbs act on the TREE and the kernel-wide task table,
        // so the agent named is the OPERAND, not the actor. Answered everywhere,
        // "send create_agent to X" could mean X performs it, X is created under
        // something, or X is the parent — only the last was ever true, and a
        // model asked "which target?" for 16 turns and finished with nothing
        // created.
        //
        // Refused HERE, in the handler — not at the call gate — so probes
        // asserting a denial `side` for a non-root privileged send stay valid.
        if target.parentId != nil {
            return Refusal.notHere(verb, target.id.value)
        }
        switch verb {
        case "list_agents":
            return listAgentsReply(payload)
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
        // The EXECUTION LEDGER, ORIGIN-SCOPED: an agent sees only what it
        // originated, the operator sees all. Unscoped this would be a
        // side-channel around `callable_by` — reading other agents' result
        // bodies without ever being granted an edge to them.
        case "persist_blob":
            return await persistBlob(payload)
        case "load_blob":
            return await loadBlob(payload)
        case "ledger":
            let rows = ledger.rows(for: KernelTaskLocals.currentOrigin)
            let (paged, next) = page(rows, payload) { $0["id"].asString ?? "" }
            return .object([
                "executions": .array(paged),
                "next": next.map { JSON.string($0) } ?? .null,
            ])
        case "ps":
            // PAGED like `list_agents` — same cursor law: the id of the last row
            // handed over. Over a LIVE table a cursor may miss a row that
            // finished mid-walk; this is a kernel, not a snapshot store.
            let (rows, next) = page(tasks.rows(), payload) { $0["id"].asString ?? "" }
            return .object([
                "tasks": .array(rows),
                "next": next.map { JSON.string($0) } ?? .null,
            ])
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
    /// One line per KERNEL verb — the honest reflect surface.
    ///
    /// These are the SUBSTRATE's own verbs, answered by the ROOT alone, so they
    /// appear in no bundle's map. Mirrors python's `_SYSTEM_VERB_DOCS`.
    ///
    /// THE CONSEQUENCE MUST PRECEDE THE FIRST EM-DASH. A default reflect keeps
    /// only the text BEFORE it, so anything after the dash reaches
    /// `detail:"full"` callers only — and these are the facts a caller needs
    /// BEFORE firing the verb, not after. Measured 2026-08-24: `create_agent`
    /// read as `args: handler_module:str, id:str?, **meta` with "spawn a child
    /// agent" cut, and a model spent 16 turns unable to work out what the target
    /// meant, finishing with `agents_created: []`.
    static func kernelVerbDocs() -> JSON {
        .object([
            "create_agent": .string(
                "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. "
                    + "Creates a CHILD OF `parent_id` — your own id grows your own subtree "
                    + "(no grant needed); 'core' for top-level. No default parent."),
            "update_agent": .string(
                "args: id:str, **meta. EVERY FIELD YOU NAME IS REPLACED, NOT MERGED "
                    + "(a field you do not name is untouched), so a dict field must be sent "
                    + "WHOLE — `can_call`/`callable_by` included: restate every key you want "
                    + "to keep, the baseline `$self`/`$parent`/`$children` among them, or the "
                    + "keys you omit are GONE. Granting one edge reads like a PATCH and "
                    + "behaves like a PUT."),
            "delete_agent": .string(
                "args: id:str. CASCADES — deletes that agent AND ITS WHOLE SUBTREE, "
                    + "deepest first; refuses if any descendant carries `delete_lock`."),
            "list_agents": .string("No args. Every agent's record, flat. Paged."),
            "get": .string("args: id:str. One agent's record — null if unknown."),
            "ps": .string(
                "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, "
                    + "verb, started_at}]}, one row per in-flight dispatch. Paged."),
            "persist_blob": .string(
                "args: agent_id:str, name:str, content:str. Persist one of YOUR OWN "
                    + "sidecars through the kernel's store — a whole-file write at "
                    + "agents/<agent_id>/<name>. You never compute a path or wire a "
                    + "provider. An agent may only name ITSELF."),
            "load_blob": .string(
                "args: agent_id:str, name:str. Read one of YOUR OWN sidecars back — "
                    + "{name, content:str|null}; null means never written. An agent may "
                    + "only name ITSELF."),
            "ledger": .string(
                "No args. Executions and what they became — {executions:[{id, target, "
                    + "status, attempts, retries, outcome?}]}, paged, scoped to what YOU "
                    + "originated. failed/timed_out/cancelled rows are the DEAD-LETTER "
                    + "QUEUE — inspect, then retry with a NEW id if you choose; nothing is "
                    + "retried silently."),
            "cancel": .string(
                "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its "
                    + "own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's "
                    + "batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}."
            ),
        ])
    }

    /// The `_` line that heads `kernel_verbs` — states WHERE to send them and
    /// what the named id means. Without it "send create_agent to X" reads three
    /// ways (X performs it / X is created / X is the parent) and only the last
    /// is true.
    static let kernelVerbsPreamble =
        "Kernel-owned: send to 'kernel'. The id you name is the OPERAND, "
        + "not the actor. (`verbs` is what this agent itself does.)"

    /// `kernel_verbs` as a reflect renders it. `full` keeps each doc whole; the
    /// default truncates at the first em-dash.
    ///
    /// Read-only verbs used to render as `""` (name only), on the ground that
    /// their reply teaches the rest — but a name with no signature is exactly
    /// what left `ps` and `list_agents` unusable without a `full` reflect.
    static func kernelVerbsView(full: Bool) -> JSON {
        var out: OrderedDictionary<String, JSON> = [:]
        out["_"] = .string(kernelVerbsPreamble)
        guard let docs = kernelVerbDocs().asObject else { return .object(out) }
        for name in docs.keys.sorted() {
            let doc = docs[name]?.asString ?? ""
            out[name] =
                .string(
                    full
                        ? doc
                        : String(doc.split(separator: "—", maxSplits: 1)[0])
                            .trimmingCharacters(in: .whitespacesAndNewlines))
        }
        return .object(out)
    }

    // ── list_agents ─────────────────────────────────────────────

    private func listAgentsReply(_ payload: JSON = .object([:])) -> JSON {
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
        // PAGED. Absent `limit` ⇒ everything and `next: null`, so a caller that
        // never learned about paging is unaffected; the cursor is the last id
        // handed over, never an offset.
        let (pageRows, next) = page(rows, payload) { $0["id"].asString ?? "" }
        return .object([
            "agents": .array(pageRows),
            "next": next.map { JSON.string($0) } ?? .null,
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

    func createFromPayload(parent root: Agent, payload: JSON) async -> JSON {
        guard let hm = payload["handler_module"].asString else {
            return Refusal.missing("create_agent", "handler_module")
        }

        // `parent_id` NAMES THE OWNER. It is a real, REQUIRED argument —
        // resolved like a send target (the `kernel` alias included), never
        // defaulted.
        //
        // It used to be accepted and silently STRIPPED while the ADDRESSED agent
        // became the parent: `parent_id:"specialist"` produced `parent_id="core"`
        // with no error. The working mechanism was undiscoverable and the field
        // that looked like it expressed placement did nothing.
        guard let pid = payload["parent_id"].asString, !pid.isEmpty else {
            return Refusal.make(
                Refusal.missingArg, "create_agent: parent_id required",
                hint: "name the OWNER of the new agent — your own id grows your own "
                    + "subtree (no grant needed); the root id for top-level.",
                extra: [("arg", .string("parent_id"))])
        }
        let parent: Agent
        if pid == "kernel" {
            parent = root
        } else if let found = agent(AgentId(pid)) {
            parent = found
        } else {
            return Refusal.unknown(pid, live: allAgents().map { $0.id.value })
        }
        // Mint an id if caller didn't supply one. Convention from
        // Rust: <bundle>_<6 hex chars>.
        let id: String
        if let supplied = payload["id"].asString, !supplied.isEmpty {
            id = supplied
        } else {
            id = mintId(hm)
        }
        // An id BECOMES A DIRECTORY, so it must be one safe segment. Enforced at
        // the mint, so every consumer inherits it rather than each remembering.
        if let badId = Refusal.guardAgentId("create_agent", id) { return badId }
        let newId = AgentId(id)
        if agent(newId) != nil {
            return Refusal.make(
                Refusal.exists, "agent \"\(id)\" already exists",
                extra: [("id", .string(id))])
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
            // THE ONE REFUSAL SHAPE (#27) — the create-side twin of the
            // `updated: false` flag on update_agent. Both were ad-hoc booleans
            // #27 declared dropped, and both carried no `reason` at all.
            return Refusal.make(Refusal.invalidMeta, why)
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
            await send([Item(newAgent.id, .object(["type": .string("boot")]))])[0].result
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
            // THE ONE REFUSAL SHAPE (#27). This carried a bare `locked: true`
            // flag and no `reason` until 2026-08-27, when the shared agentic
            // corpus caught it — in python, rust and swift at once, because the
            // flag #27 declared dropped was never actually removed anywhere.
            return Refusal.make(
                Refusal.locked,
                "delete_agent: \(id.value) blocked by delete_lock on descendant "
                    + "\(blocker.value)",
                hint: "clear the lock with update_agent, then delete again",
                extra: [
                    ("id", .string(id.value)),
                    ("blocked_by", .string(blocker.value)),
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
            return Refusal.missing("update_agent", "id")
        }
        let id = AgentId(idStr)
        guard let target = agent(id) else {
            return Refusal.unknown(idStr, live: allAgents().map { $0.id.value })
        }
        // Patch = every field except type + id.
        var patch: OrderedDictionary<String, JSON> = [:]
        var changed: [String] = []
        if case .object(let dict) = payload {
            // `parent_id` is STRUCTURAL — where the agent actually sits, which
            // only the tree knows. Letting it into meta lets a record LIE about
            // its own parent: `$parent` then resolves differently on the two
            // halves of the gate, and a reboot silently corrects it from the
            // agent's position on disk. Reserved keys are refused for the same
            // reason — they name substrate wiring, not agent meta.
            for (k, v) in dict
            where k != "type" && k != "id" && k != "parent_id"
                && !reservedRecordKeys.contains(k)
            {
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
            // THE ONE REFUSAL SHAPE (#27). A bundle's own `validate_meta`
            // rejection is `invalid_meta`; it carried the ad-hoc `updated:
            // false` flag #27 declared dropped, and no `reason` at all, until
            // the shared agentic corpus caught it 2026-08-27.
            return Refusal.make(
                Refusal.invalidMeta, why, extra: [("id", .string(idStr))])
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
    /// leg's lock (`ingress_rule`/`egress_rule`/`auth`: allow_all_ingress = open,
    /// password = gated, deny_inbound/absent on an io leg = sealed), a
    /// file_bridge's served `root`, and what a consumer persists THROUGH
    /// (`file_bridge_id`). Mirrors py/rust `_POSTURE_KEYS`.
    static let POSTURE_KEYS = [
        "ingress_rule", "egress_rule", "auth", "root", "file_bridge_id",
        // THE CALL GATE, made readable. Without these an agent cannot read its
        // own authority — only discover it by trying, which cost whole turns of
        // empirical probing every run. They also make the replace-not-merge
        // warning on `update_agent` actionable: you must send the map WHOLE, so
        // you must be able to read it.
        "can_call", "callable_by",
    ]

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
    /// receive shape its connector really implements. The ONE table the root
    /// `self_extension` reads — a hard-coded module name there went wrong in
    /// both directions: the process-edge executor (registered in Startup under
    /// `#if os(macOS)`) went unnamed on macOS, and on a build that does not
    /// register it the string named a module a caller cannot use. Deriving the
    /// advertised set from this table ∩ the live registry makes that impossible.
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

    /// The module to SHOW in the `create_agent` example: the first executor this
    /// build registered, else the first known one.
    ///
    /// The slot used to hold the literal `'<an executor below>'`, which left the
    /// example uncopyable — a model had to join it to the tail sentence to learn
    /// the real name. Python interpolates the module here
    /// (`_executor_for_example`) and that is the string a weak model actually
    /// reads: measured 2026-08-05, given a name it could not use, claude-opus-5
    /// authored `handler_module:"local_runner.script"` and built nothing.
    func executorForExample() -> String {
        // NO EXECUTOR REGISTERED → name NOTHING. Falling back to a table entry
        // would advertise a module this build cannot run, which is the exact
        // defect the derivation exists to prevent; the tail then says the build
        // ships none. The placeholder is honest here and only here.
        scriptExecutorsHere().first?.handlerModule ?? "<an executor below>"
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
