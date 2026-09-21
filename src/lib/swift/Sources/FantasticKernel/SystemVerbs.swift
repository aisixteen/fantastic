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
    /// A python RAISE inside one of these is a RESULT, not a crash: the
    /// caller's settle carries `{error:"<Type>: <msg>", raised:true}`, exactly
    /// as a bundle handler's throw does (python wraps every handler, kernel
    /// verbs included, in the one `except`).
    func handleSystemVerb(target: Agent, verb: String, payload: JSON) async throws(PyError) -> JSON {
        // ROOT ONLY. These verbs act on the TREE and the kernel-wide task table,
        // so the agent named is the OPERAND, not the actor. Answered everywhere,
        // "send create_agent to X" could mean X performs it, X is created under
        // something, or X is the parent — only the last was ever true, and a
        // model asked "which target?" for 16 passes and finished with nothing
        // created.
        //
        // Refused HERE, in the handler — not at the call gate — so probes
        // asserting a denial `side` for a non-root privileged send stay valid.
        if target.parentId != nil {
            // C32: EVERY system verb on a non-root — `shutdown_agents` and
            // `shutdown_kernel` included — answers the SAME generic
            // `Refusal.notHere`. Python has no special case here: `if self.
            // parent is not None: return refusal(NOT_HERE, ...)` covers all
            // seven verbs uniformly ("shutdown_kernel already refused this
            // way; this generalises it to all seven" — `_agent.py:2036-2044`).
            // The two-stop-verbs special case this used to carry answered a
            // DIFFERENT, uncoded `{error}` with no `reason`/`hint`/`verb`.
            return Refusal.notHere(verb, target.id.value)
        }
        switch verb {
        case "list_agents":
            return listAgentsReply(payload)
        case "create_agent":
            return await createFromPayload(payload: payload)
        case "delete_agent":
            return await deleteFromPayload(caller: target, payload: payload)
        case "update_agent":
            return await updateFromPayload(caller: target, payload: payload)
        // The live TASK TABLE, kernel-wide (like `list_agents`), not per-agent:
        // one row per in-flight dispatch, `parent` chaining them into the send
        // tree. `ps` shows its own row too — it IS a dispatch. The table is honest.
        // The EXECUTION LEDGER, ORIGIN-SCOPED: an agent sees only what it
        // originated, the operator sees all. Unscoped this would be a
        // side-channel around `callable_by` — reading other agents' result
        // bodies without ever being granted an edge to them.
        case "ledger":
            let rows = ledger.rows(for: KernelTaskLocals.currentOrigin)
            let (paged, next) = page(rows, payload) { $0["id"].asInt.map(String.init) ?? ($0["id"].asString ?? "") }
            return .object([
                "executions": .array(paged),
                "next": next.map { JSON.string($0) } ?? .null,
            ])
        case "messages":
            return try messagesReply(payload)
        case "rows_read":
            return try rowsReadReply(payload)
        case "rows_write":
            return try rowsWriteReply(payload)
        case "ps":
            // PAGED like `list_agents` — same cursor law: the id of the last row
            // handed over. Over a LIVE table a cursor may miss a row that
            // finished mid-walk; this is a kernel, not a snapshot store.
            let (rows, next) = page(tasks.rows(), payload) { $0["id"].asInt.map(String.init) ?? ($0["id"].asString ?? "") }
            return .object([
                "tasks": .array(rows),
                "next": next.map { JSON.string($0) } ?? .null,
            ])
        // Kill a task AND ITS SUBTREE (children first, like killing a process
        // group). Each bundle's `onCancel` hook runs BEFORE the abort so real
        // resources (OS subprocesses, PTYs, HTTP streams) die with it. An unknown
        // or already-finished id returns {cancelled:false} — it does not pretend.
        case "cancel":
            // CODED. `{error: "cancel: task_id (str) required"}` could only be
            // branched on by matching prose, so the shared corpus' `reason`
            // matcher could never be satisfied here.
            // C18: python `_verb_cancel` — missing iff `raw` is absent/null, a
            // bool (either one), `""`, or anything but an int/str (a double,
            // list, object). Otherwise resolved through the SAME `_as_id` path
            // every other id argument uses (`asMessageId` — a positive int, or
            // an all-Unicode-decimal-digit string, ASCII `Int64.init?` rejected:
            // it also accepts a leading `+`, which python's `str.isdigit()`
            // does not); a string that is not a digit-shaped id falls back to
            // the caller's own execution NAME. Nothing resolved is an honest
            // `{cancelled:false}`, never a refusal — the same answer a stale id
            // gets.
            let rawTaskId = payload["task_id"]
            let taskIdMissing: Bool
            switch rawTaskId {
            case .null, .bool: taskIdMissing = true
            case .string(let s): taskIdMissing = s.isEmpty
            case .integer: taskIdMissing = false
            default: taskIdMissing = true  // double, array, object
            }
            if taskIdMissing { return Refusal.missing("cancel", "task_id") }
            var resolved = try asMessageId(rawTaskId)
            if resolved == nil, case .string(let s) = rawTaskId {
                resolved = ledger.resolveName(s)
            }
            guard let tid = resolved else {
                return .object(["cancelled": .bool(false), "killed": .array([])])
            }
            return await cancelTask(TaskId(tid))
        // The agent half of a graceful stop: drain, the `shutdown` verb to
        // every agent that advertises it, quiet, cancel the rest. The kernel
        // keeps serving; the persister catches up. Root control surface only.
        case "shutdown_agents":
            return await shutdownAgents()
        // Gracefully stop the whole kernel PROCESS. Order: (1) ack
        // `{type:"shutdown_kernel", ok:true}`; (2) AFTER the reply is on the
        // wire, trigger the daemon's normal graceful path; (3) exit 0. The
        // trigger is DEFERRED 100 ms so the in-flight REST body / WS frame is
        // fully written before the serve loop tears the listeners down —
        // never signal synchronously here or the caller sees a dropped
        // socket, not an ack. Idempotent: `requestShutdown` collapses a racing
        // signal to one shutdown. With no serve loop there is nothing to stop
        // — it still acks. Python `_verb_shutdown_kernel`.
        case "shutdown_kernel":
            Task { [weak self] in
                try? await Task.sleep(nanoseconds: 100_000_000)
                self?.requestShutdown()
            }
            return .object(["type": .string("shutdown_kernel"), "ok": .bool(true)])
        default:
            return Refusal.make(
                Refusal.unhandled, "unhandled system verb \(pyRepr(verb))",
                extra: [("verb", .string(verb))])
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
    /// agent" cut, and a model spent 16 passes unable to work out what the target
    /// meant, finishing with `agents_created: []`.
    static func kernelVerbDocs() -> JSON {
        .object([
            "create_agent": .string(
                "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. "
                    + "Creates a CHILD OF `parent_id` — your own id grows your own subtree "
                    + "(no grant needed); 'kernel_state' for top-level. No default parent."),
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
            "ps": .string(
                "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, "
                    + "verb, started_at}]}, one row per in-flight dispatch. Paged."),
            // EVERY execution, not just the named ones. Python's line said
            // NAMED-only while the verb iterated the whole table, so a model
            // reading the reflect expected its own handful of jobs and received
            // every minted-pid row beside them — the doc contradicting the reply
            // it had just been handed.
            "ledger": .string(
                "No args. EVERY execution and what it became — named under its id, "
                    + "unnamed under its minted pid — {executions:[{id, target, status, "
                    + "attempts, retries, named, outcome?}]}, paged, scoped to what YOU "
                    + "originated. Failed/timed_out/cancelled rows are the dead-letter "
                    + "queue: inspect, then retry with a NEW id if you choose — nothing "
                    + "is retried silently."),
            "messages": .string(
                "args: ids:list[str] (req), limit:int?, after:str?. Conversation rows by id, "
                    + "at DIGEST resolution — {rows:[{id, agent, parent, kind, role, head, bytes, "
                    + "title?, level?, refs?}], refused:[ids], next?}. Your own rows always; another "
                    + "agent's only if its callable_by names you for `messages`. Bodies are behind "
                    + "that agent's `pull`."),
            "rows_read": .string(
                "args: agent:str (req), after:int|str?, limit:int?. PRIVILEGED (a wildcard does "
                    + "not carry it). That agent's "
                    + "rows WHOLE (content included), oldest first — {rows:[…], next?}. The "
                    + "hydration read; agents read digests through `messages` instead."),
            "rows_write": .string(
                "args: agent:str (req), rows:list[dict] (req). PRIVILEGED (a wildcard does "
                    + "not carry it). Appends rows "
                    + "to that agent's conversation, each under a FRESH id — {ids:[int]}. A row "
                    + "may not carry an `id`; re-ids are the caller's to remap."),
            "cancel": .string(
                "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its "
                    + "own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's "
                    + "batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}."
            ),
            "shutdown_agents": .string(
                "No args. Stops the AGENTS, not the process: drains (new roots refused), "
                    + "sends `shutdown` to every agent advertising it, waits for in-flight work, "
                    + "cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}."
            ),
            "shutdown_kernel": .string(
                "No args. Stops the kernel PROCESS — under env:'container', the container."
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
    ///
    /// ORDER IS PYTHON'S: `full` iterates `_SYSTEM_VERB_DOCS` in DECLARATION
    /// order (the map is spread whole); only the distilled view sorts, because
    /// it is built by a comprehension over `sorted(_SYSTEM_VERB_DOCS)`.
    static func kernelVerbsView(full: Bool) -> JSON {
        var out: OrderedDictionary<String, JSON> = [:]
        out["_"] = .string(kernelVerbsPreamble)
        guard let docs = kernelVerbDocs().asObject else { return .object(out) }
        for name in full ? Array(docs.keys) : docs.keys.sorted() {
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
        // REGISTRATION ORDER, not sorted: python pages
        // `[a.record for a in self.ctx.agents.values()]` straight off the
        // registry dict, so the walk reads the order the agents were made in.
        let rows: [JSON] = allAgents().map { recordToJSON($0.record()) }
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
                    + "manual. <type>reflect</type> on the root is the briefing: each agent's "
                    + "purpose, plus `self_extension` (how to author your own durable tools "
                    + "in THIS kernel — which executor, and the fields without which the "
                    + "agent is created and never runs) and `readmes` (the manual index). "
                    + "Reflect the root before building anything."),
        ])
    }


    // ── create_agent ────────────────────────────────────────────

    /// A grant map's SHAPE, judged at WRITE time — the gate only judges edges.
    /// `{id|token: "*" | [verbs]}`; anything else is refused so a record can
    /// never carry a grant the gate silently denies on (py-canonical, 2026-09-01).
    private func badGrantShape(_ meta: OrderedDictionary<String, JSON>) -> String? {
        for field in ["can_call", "callable_by"] {
            guard let g = meta[field] else { continue }
            guard case .object(let map) = g else {
                return "\(field) must be a grant MAP {\"<id|$token>\": \"*\" | "
                    + "[\"<verb>\", …]} — got \(pyTypeName(g)). To open wide, grant per key "
                    + "(e.g. {\"$self\": \"*\", \"ai\": \"*\"}), never a bare value"
            }
            for (k, v) in map {
                let ok: Bool
                switch v {
                case .string(let s): ok = s == "*"
                case .array(let items): ok = items.allSatisfy { $0.asString != nil }
                default: ok = false
                }
                if !ok { return "\(field)[\(pyRepr(k))] must be \"*\" or a list of verb strings" }
            }
        }
        return nil
    }

    func createFromPayload(payload: JSON) async -> JSON {
        guard let hm = payload["handler_module"].asString, !hm.isEmpty else {
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
                hint: "name the agent that will OWN the new one — your own id for a "
                    + "tool you keep, or the root ('kernel_state') for a top-level agent",
                extra: [("arg", .string("parent_id"))])
        }
        // Resolved the SAME way `send` does (C35): the `kernel` alias, then a
        // declared `alias` meta, then a literal id — so `parent_id` accepts
        // anything a caller could have addressed.
        guard let parent = resolveAgent(pid) else {
            return Refusal.unknown(pid, live: allAgents().map { $0.id.value })
        }

        // Reserved keys name substrate wiring; a record may not carry them (the
        // loader refuses such a record on the next boot). Checked in SORTED
        // key order (python `sorted(RESERVED_RECORD_KEYS)`), against the raw
        // payload — before meta is even built.
        if case .object(let dict) = payload {
            for k in reservedRecordKeys.sorted() where dict[k] != nil {
                return Refusal.bad(
                    "create_agent", k,
                    "absent — root_path/ctx/parent name substrate wiring the "
                        + "record may not carry")
            }
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
        if let why = badGrantShape(meta) {
            return Refusal.make(Refusal.invalidMeta, why)
        }
        if let bundle = bundles.get(hm), let why = bundle.validateMeta(meta, record: [:]) {
            // THE ONE REFUSAL SHAPE (#27) — the create-side twin of the
            // `updated: false` flag on update_agent. Both were ad-hoc booleans
            // #27 declared dropped, and both carried no `reason` at all.
            return Refusal.make(Refusal.invalidMeta, why)
        }

        // Mint an id if caller didn't supply one. Convention from
        // Rust: <bundle>_<6 hex chars>.
        // A supplied id — even "" — is the caller's choice and is validated as
        // one by `guardAgentId`; only an ABSENT id is minted (python parity).
        let id: String
        if case .object(let o) = payload, let supplied = o["id"], supplied != .null {
            id = supplied.asString ?? ""
        } else {
            id = mintId(hm)
        }
        // An id BECOMES A DIRECTORY, so it must be one safe segment. Enforced at
        // the mint, so every consumer inherits it rather than each remembering.
        if let badId = Refusal.guardAgentId("create_agent", id) { return badId }
        let newId = AgentId(id)
        if agent(newId) != nil {
            return Refusal.make(
                Refusal.exists, "agent \(pyRepr(id)) exists",
                hint: "pick another id, or update the existing agent",
                extra: [("id", .string(id))])
        }

        let rootPath = parent.childrenDir(childrenDir).appendingPathComponent(id)
        let newAgent = Agent(
            id: newId,
            handlerModule: hm,
            parentId: parent.id,
            meta: meta,
            rootPath: rootPath
        )

        // Wire into the kernel + parent. NO INLINE PERSIST: the `added` event
        // below is what makes the record dirty, and the ROOT LOADER's debounced
        // `persist_writer` writes it — with its readme + docs — when it drains.
        // Persisting here as well wrote the record twice and, worse, wrote it
        // even when no writer was installed, which is the one case python keeps
        // in RAM.
        register(newAgent)
        parent.insertChild(newAgent)

        // LIFECYCLE SHAPE, python's: `{agent_id, kind, …}` like every other state
        // event, so one subscriber reads one shape. `created` with a
        // `handler_module` was a swift/rust-only kind nothing else answers;
        // `name` is what a view renders.
        let event: JSON = .object([
            "agent_id": .string(newAgent.id.value),
            "kind": .string("added"),
            "name": .string(displayNameOrId(newAgent)),
            "parent_id": .string(parent.id.value),
        ])
        publishState(event)

        let recordJson = recordToJSON(newAgent.record())

        // Fire `boot` on the new agent. Failures are logged via the
        // reply but don't undo the create — matches Rust's behaviour.
        // Operator authority: booting the child is a side effect of the already-
        // authorized create_agent, not the parent reaching out — so it never runs
        // against the parent's grant.
        let bootOutcome = await KernelTaskLocals.$currentSender.withValue(nil) {
            await send([Item(newAgent.id, .object(["type": .string("boot")]))])[0]
        }
        // BRANCH ON THE OUTCOME STATUS, not on the presence of an `error`: a
        // handler that RAISED (`failed`) is a bug the operator must see; one
        // that deliberately answered `{error}` (`errored`) is a refusal the
        // create does not undo and does not narrate. Python
        // `_agent.py:2663-2670` prints for the raise alone.
        if bootOutcome.status == .failed {
            printFlushed(
                "  [create_agent] \(newAgent.id.value) boot failed: "
                    + pyStr(bootOutcome.result))
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
        guard let idStr = payload["id"].asString, !idStr.isEmpty else {
            return Refusal.missing("delete_agent", "id")
        }
        let id = AgentId(idStr)
        guard let target = agent(id) else {
            // THE ONE REFUSAL SHAPE, py-canonical: a code to branch on, the LIVE
            // id index to self-correct from, and `deleted:false` so a caller
            // reading that field is not told a typo succeeded. The bare `{error}`
            // this used to answer is the measured 34-retry case — a fact with
            // nothing to act on.
            var r = Refusal.unknown(idStr, live: allAgents().map { $0.id.value })
            r["deleted"] = .bool(false)
            return r
        }
        // THE ROOT IS NOT DELETABLE. It owns the persistence surface and answers
        // every kernel verb, so cascading it empties the kernel and leaves a
        // process with no control surface at all — `{"deleted": true}` and an
        // empty tree (measured 2026-09-02).
        if target.parentId == nil {
            return Refusal.make(
                Refusal.badArg,
                "delete_agent: \(idStr) is the ROOT — it owns persistence and "
                    + "answers the kernel verbs; deleting it would leave no kernel",
                extra: [
                    ("arg", .string("id")), ("id", .string(idStr)),
                    ("deleted", .bool(false)),
                ])
        }
        if let blocker = findLockedDescendant(of: target) {
            // THE ONE REFUSAL SHAPE (#27). This carried a bare `locked: true`
            // flag and no `reason` until 2026-08-27, when the shared agentic
            // corpus caught it — in python, rust and swift at once, because the
            // flag #27 declared dropped was never actually removed anywhere.
            return Refusal.make(
                Refusal.locked,
                "delete_agent: \(pyRepr(id.value)) blocked by delete_lock on "
                    + "\(pyRepr(blocker.value)); clear it via update_agent before deleting",
                hint: "clear the lock with update_agent, then delete again",
                extra: [
                    ("blocked_by", .string(blocker.value)),
                    ("id", .string(id.value)),
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
        // Unregister from the kernel. DISK CLEANUP IS THE WRITER'S: the
        // `removed` event below queues the dir it cached for this agent, and
        // the root loader's `persist_writer` removes it through the discovered
        // provider when it drains.
        unregister(target.id)

        let event: JSON = .object([
            "agent_id": .string(target.id.value),
            "kind": .string("removed"),
            "name": .string(displayNameOrId(target)),
        ])
        publishState(event)
    }

    // ── update_agent ────────────────────────────────────────────

    func updateFromPayload(caller: Agent, payload: JSON) async -> JSON {
        guard let idStr = payload["id"].asString, !idStr.isEmpty else {
            return Refusal.missing("update_agent", "id")
        }
        let id = AgentId(idStr)
        // `parent_id` is STRUCTURAL — where the agent actually sits, which
        // only the tree knows. Letting it into meta lets a record LIE about
        // its own parent: `$parent` then resolves differently on the two
        // halves of the gate, and a reboot silently corrects it from the
        // agent's position on disk. Reserved keys are refused for the same
        // reason — they name substrate wiring, not agent meta.
        //
        // REFUSED, not silently skipped. `handler_module` is fixed at create —
        // swapping it would leave a live agent whose in-process state belongs
        // to the OLD bundle. Reserved keys used to be dropped without a word,
        // so the caller believed a change had landed that never did
        // (2026-09-02). Checked BEFORE the target is even resolved, `handler_
        // module` first then sorted reserved keys (python `("handler_module",
        // *sorted(RESERVED_RECORD_KEYS))`).
        if case .object(let dict) = payload {
            for k in ["handler_module"] + reservedRecordKeys.sorted() where dict[k] != nil {
                return Refusal.bad(
                    "update_agent", k,
                    "absent — handler_module is fixed at create_agent, and "
                        + "root_path/ctx/parent name substrate wiring the record "
                        + "may not carry")
            }
        }
        // Patch = every field except type + id + parent_id.
        var patch: OrderedDictionary<String, JSON> = [:]
        var changed: [String] = []
        if case .object(let dict) = payload {
            for (k, v) in dict where k != "type" && k != "id" && k != "parent_id" {
                patch[k] = v
                changed.append(k)
            }
        }
        guard let target = agent(id) else {
            return Refusal.unknown(idStr, live: allAgents().map { $0.id.value })
        }
        // Grant-map SHAPE, judged here at write time — the gate only judges
        // edges (py-canonical, 2026-09-01: `can_call:"*"` accepted as a bare
        // string left a silently-dead grant; refuse loud at the mistake).
        if let why = badGrantShape(patch) {
            return Refusal.make(Refusal.invalidMeta, why, extra: [("id", .string(idStr))])
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
        // NO INLINE PERSIST: the `updated` event below is what makes the record
        // dirty for the root loader's `persist_writer`.
        let rec = target.updateMeta(patch)
        let recJson = recordToJSON(rec)

        // Python's shape: `{agent_id, kind:"updated", changed}` — the CHANGED
        // KEYS, nothing more. The full record rode along as `agent` in the port;
        // a state event is a notification that something moved, and a subscriber
        // that wants the record reads it.
        let event: JSON = .object([
            "agent_id": .string(target.id.value),
            "kind": .string("updated"),
            "changed": .array(changed.map { .string($0) }),
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

        // PYTHON'S REPLY SHAPE. The bare record left a caller that branches on
        // `updated` — the field the kernel_verbs doc teaches — reading nothing on
        // this runtime for the call that succeeded, so a model read success as
        // failure and retried.
        return .object([
            "updated": .bool(true),
            "id": .string(target.id.value),
            "agent": recJson,
        ])
    }
}

// ── Helpers ───────────────────────────────────────────────────────

/// A record as the wire sees it — `id, handler_module?, <meta>, parent_id`.
/// Built as an OrderedDictionary, never through `JSONEncoder` (C15).
func recordToJSON(_ rec: AgentRecord) -> JSON { rec.json }

/// Mint an id like `<bundle>_<6 hex>` — python `f"{bundle}_{secrets.token_hex(3)}"`
/// where `bundle` is the SECOND-TO-LAST dotted component of the handler module
/// (`terminal_backend.tools` → `terminal_backend`), the WHOLE STRING when there
/// is no dot — python's own branch (`_agent.py::create`:
/// `handler_module.split(".")[-2] if "." in handler_module else handler_module`),
/// not an `IndexError`.
func mintId(_ bundle: String) -> String {
    let parts = bundle.components(separatedBy: ".")
    let prefix = parts.count > 1 ? parts[parts.count - 2] : bundle
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
        // own authority — only discover it by trying, which cost whole passes of
        // empirical probing every run. They also make the replace-not-merge
        // warning on `update_agent` actionable: you must send the map WHOLE, so
        // you must be able to read it.
        "can_call", "callable_by",
    ]

    /// `display_name or id` — PYTHON TRUTHINESS, so an EMPTY name is no name.
    /// `?? id` alone only covered an absent one, and a record written with
    /// `display_name: ""` reflected a blank identity.
    func displayNameOrId(_ agent: Agent) -> String {
        guard let n = agent.displayName, !n.isEmpty else { return agent.id.value }
        return n
    }

    func reflectIdentity(_ agent: Agent) -> JSON {
        var obj: OrderedDictionary<String, JSON> = [:]
        obj["id"] = .string(agent.id.value)
        obj["sentence"] = .string(sentenceFor(agent))
        obj["parent_id"] = agent.parentId.map { .string($0.value) } ?? .null
        obj["handler_module"] = agent.handlerModule.map { .string($0) } ?? .null
        obj["display_name"] = .string(displayNameOrId(agent))
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
        obj["display_name"] = .string(displayNameOrId(agent))
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
        let kids = agent.childIds().compactMap { self.agent($0) }
        obj["children"] = .array(kids.map { treeNode($0) })
        return .object(obj)
    }

    /// Flat id index of an agent + all descendants (DFS, self first,
    /// children in creation order). The cheap `tree=ids` tier.
    func descendantIds(_ agent: Agent) -> [String] {
        var out = [agent.id.value]
        let kids = agent.childIds().compactMap { self.agent($0) }
        for c in kids { out.append(contentsOf: descendantIds(c)) }
        return out
    }

    /// The INSTALLABLE-bundle catalog as `(name, handlerModule)` pairs, sorted
    /// by name — python's entry-point scan. The `bundles=all`/`bundles=ids`
    /// source; a test-registered fixture (`registerUnlisted`) is not in it,
    /// exactly as an importable module with no entry point is not in python's.
    func availableBundles() -> [(name: String, handlerModule: String)] {
        bundles.catalog()
            .map { (handlerModule, bundle) in (bundle.name, handlerModule) }
            .sorted { $0.name < $1.name }
    }

}

// MARK: - THE ROW STORE's verbs (python `_verb_messages`/`_verb_rows_read`/`_verb_rows_write`)

/// `len(x)` for a sized value, `1` for a scalar — a digest counts, it never
/// raises on a row a caller wrote by hand (python, 2026-09-20).
private func sizedLen(_ v: JSON) -> Int { (try? pyLen(v)) ?? 1 }

/// A message id as an int: an int, or its digit string (a model may quote what
/// it copied out of a tag). Anything else is not an id. Python `_as_id`.
///
/// `isdecimal`, not `isdigit` (python, 2026-09-20): `'²'` is not an id, where
/// the wider guard let it through to raise inside `int()`. `'٢'` is a decimal
/// digit and reads as 2.
public func asMessageId(_ v: JSON?) throws(PyError) -> Int64? {
    switch v {
    case .integer(let n)?: return n > 0 ? n : nil
    case .string(let s)? where pyIsDecimalString(s):
        return Int64(try pyInt(.string(s)))
    default: return nil
    }
}

/// Python's `int(x or 0)` for a page limit, with python's own `except
/// (TypeError, ValueError): limit = 0`. A magnitude python holds and `Int`
/// cannot SATURATES with its sign, because that is what the comparisons below
/// mean against an unbounded int: a huge positive limit caps nothing, a huge
/// negative one keeps nothing.
func pyLimit(_ v: JSON) -> Int {
    do {
        return try pyIntOr(v, 0)
    } catch {
        guard error.type == "OverflowError" else { return 0 }
        return pyLimitIsNegative(v) ? Int.min : Int.max
    }
}

/// `content[:120].replace("\n", " ")` — python slices and substitutes CODE
/// POINTS. Both halves are walked scalar by scalar because swift's own
/// `prefix`/`replacingOccurrences` work on GRAPHEMES, where `\r\n` is one
/// character: the prefix would hand back 240 scalars and the substitution
/// would not find the `\n` inside a CRLF at all.
func pyHead(_ content: String) -> String {
    var head = ""
    for u in content.unicodeScalars.prefix(120) {
        head.unicodeScalars.append(u == "\n" ? " " : u)
    }
    return head
}

private func pyLimitIsNegative(_ v: JSON) -> Bool {
    switch v {
    case .integer(let n): return n < 0
    case .double(let d): return d < 0
    case .string(let s): return s.unicodeScalars.drop(while: pyIsSpace).first == "-"
    default: return false
    }
}

extension Kernel {
    /// Rows by id at DIGEST resolution, gated INSIDE: the caller's own rows
    /// always; another agent's only when that owner's `callable_by` names the
    /// caller for `messages` (the same ingress check the call gate runs, re-run
    /// here against the OWNER because a root verb skips ingress); the operator
    /// sees all. Refused ids are LISTED, never silently dropped. Bodies stay
    /// behind the owner's `pull`.
    func messagesReply(_ payload: JSON) throws(PyError) -> JSON {
        let raw = payload["ids"].asArray ?? []
        // The parse runs over the WHOLE list before the shape guard, like
        // python's comprehension: a `'²'` in position 3 raises, it does not
        // reach the `bad_arg`.
        let parsed = try raw.map { (x) throws(PyError) in try asMessageId(x) }
        guard !parsed.isEmpty, !parsed.contains(where: { $0 == nil }) else {
            return Refusal.make(
                Refusal.badArg, "messages: `ids` must be a non-empty list of message ids (ints)",
                extra: [("arg", .string("ids"))])
        }
        var ids = parsed.map { $0! }
        let caller = KernelTaskLocals.currentOrigin
        let loaderId = root?.id.value
        var decided: [String: Bool] = [:]
        func mayRead(_ ownerId: String) -> Bool {
            guard let caller, caller.value != ownerId else { return true }
            if let d = decided[ownerId] { return d }
            guard let owner = agent(AgentId(ownerId)) else {
                decided[ownerId] = false
                return false
            }
            let d = authorizeCall(
                grant: owner.metaValue(forKey: "callable_by") ?? .null, verb: "messages",
                otherId: caller.value, holderId: owner.id.value,
                parentId: owner.parentId?.value,
                childIds: Set(owner.childIds().map { $0.value }), loaderId: loaderId,
                enforcePrivileged: false)
            decided[ownerId] = d.allowed
            return d.allowed
        }
        if let after = try asMessageId(payload["after"]), let i = ids.firstIndex(of: after) {
            ids = Array(ids[(i + 1)...])
        }
        var out: [JSON] = []
        var refused: [JSON] = []
        for rid in ids {
            guard let row = rows.getOne(rid) else {
                refused.append(.integer(rid))
                continue
            }
            let ownerId = row["agent"]?.asString ?? ""
            guard mayRead(ownerId) else {
                refused.append(.integer(rid))
                continue
            }
            // `str(content)` — a row's content is whatever was written there, and
            // a digest of a number, a list or `None` is python's rendering of it,
            // not an empty head. CODE POINTS throughout: python slices and counts
            // a str, so an emoji is one, not four bytes and not one grapheme.
            let content = pyStr(row["content"] ?? .string(""))
            let kind = row["kind"]?.asString
            let kindRaw = row["kind"] ?? .null
            var d: OrderedDictionary<String, JSON> = [
                "id": .integer(rid),
                "agent": row["agent"] ?? .null,
                "parent": row["parent"] ?? .null,
                "kind": pyTruthy(kindRaw) ? kindRaw : (row["role"] ?? .null),
                "role": row["role"] ?? .null,
                "head": .string(pyHead(content)),
                "bytes": .integer(Int64(content.unicodeScalars.count)),
            ]
            // PYTHON TRUTHINESS decides every optional key, and the value copied
            // is the RAW one — a `level` of `false` or `[]` is absent, a `level`
            // of `2.5` is `2.5`.
            if let t = row["title"], pyTruthy(t) { d["title"] = t }
            if let l = row["level"], pyTruthy(l) { d["level"] = l }
            if let refs = row["refs"], pyTruthy(refs) { d["refs"] = .integer(Int64(sizedLen(refs))) }
            if kind == "call" || kind == "task" {
                d["verb"] = row["verb"] ?? .null
                if let n = row["name"], pyTruthy(n) { d["name"] = n }
                // The status is the SETTLE's, read off the nested stamp — a
                // top-level `status` is not one, and an empty stamp is no status
                // at all. A non-dict stamp is no status either (python guards
                // `.get` since 2026-09-20; it used to raise off the wire).
                if let o = row["outcome"], pyTruthy(o) {
                    if case .object = o { d["status"] = o["status"] ?? .null } else { d["status"] = .null }
                }
            } else if kind == "view" {
                // a view digest says how much it holds, never the array itself
                let viewIds = row["ids"] ?? .null
                d["ids"] = .integer(Int64(pyTruthy(viewIds) ? sizedLen(viewIds) : 0))
                d["budget"] = row["budget"] ?? .null
                // LAST, and an assignment: a view that already carried a truthy
                // `title` keeps that key's position and takes `why` as its value.
                d["title"] = row["why"] ?? .null
            }
            out.append(.object(d))
        }
        // DIGESTS FIT WHERE THEY ARE GOING: page by `limit`, and never past ~32KB.
        // The cap measures `len(json.dumps(d))` — python's separators, its
        // `ensure_ascii` escapes and its float repr, counted in CODE POINTS, so
        // the page boundary lands on the same row python's does.
        let limit = pyLimit(payload["limit"])
        let capBytes = 32 * 1024
        var kept: [JSON] = []
        var used = 0
        for d in out {
            if limit != 0 && kept.count >= limit { break }
            let n = pyJsonDumps(d).unicodeScalars.count
            if used + n > capBytes { break }
            kept.append(d)
            used += n
        }
        let next: JSON = kept.count < out.count ? (kept.last?["id"] ?? .null) : .null
        return .object(["rows": .array(kept), "refused": .array(refused), "next": next])
    }

    /// That agent's rows WHOLE — content included, oldest first —
    /// {rows:[…], next:int|None}. PRIVILEGED: a wildcard grant never carries
    /// it; this is hydration's read, the one `messages` deliberately is not.
    func rowsReadReply(_ payload: JSON) throws(PyError) -> JSON {
        guard let agentArg = payload["agent"].asString,
            !pyStrip(agentArg).isEmpty
        else {
            return Refusal.make(
                Refusal.badArg, "rows_read: `agent` must be an agent id",
                extra: [("arg", .string("agent"))])
        }
        var list = rows.forAgent(AgentId(pyStrip(agentArg)))
        if let after = try asMessageId(payload["after"]) {
            if let i = list.firstIndex(where: { $0["id"]?.asInt == after }) {
                list = Array(list[(i + 1)...])
            } else {
                list = []
            }
        }
        // `rows[:limit]` is a PYTHON SLICE: any non-zero limit truncates, and a
        // negative one drops that many from the END instead of capping the head.
        let limit = pyLimit(payload["limit"])
        let truncated = limit != 0 && list.count > limit
        let end: Int
        if limit >= 0 {
            end = min(limit, list.count)
        } else if limit < -list.count {
            end = 0
        } else {
            end = list.count + limit
        }
        let kept = truncated ? Array(list.prefix(end)) : list
        return .object([
            "rows": .array(kept.map { JSON.object($0) }),
            "next": truncated ? (kept.last?["id"] ?? .null) : .null,
        ])
    }

    /// Append rows to that agent's conversation, each under a FRESH id —
    /// {ids:[int]}, in the order given. A row may not carry its own `id`: the
    /// store is the only allocator; re-ids are the CALLER's to remap.
    func rowsWriteReply(_ payload: JSON) throws(PyError) -> JSON {
        guard let agentArg = payload["agent"].asString,
            !pyStrip(agentArg).isEmpty
        else {
            return Refusal.make(
                Refusal.badArg, "rows_write: `agent` must be an agent id",
                extra: [("arg", .string("agent"))])
        }
        guard let list = payload["rows"].asArray, !list.isEmpty,
            list.allSatisfy({ if case .object = $0 { return true } else { return false } })
        else {
            return Refusal.make(
                Refusal.badArg, "rows_write: `rows` must be a non-empty list of rows",
                extra: [("arg", .string("rows"))])
        }
        if list.contains(where: { $0["id"] != .null }) {
            return Refusal.make(
                Refusal.badArg,
                "rows_write: a row may not carry an `id` — the store mints every "
                    + "id, and a copy is a NEW row",
                extra: [("arg", .string("rows"))])
        }
        let aid = AgentId(pyStrip(agentArg))
        var ids: [JSON] = []
        for r in list {
            var row = r.asObject ?? [:]
            row.removeValue(forKey: "id")
            // `put` refuses only a present id; a null one was stripped above.
            // NO FALLBACK: a refused put used to report `0` as a minted id, so
            // the caller learned nothing and kept a row number that names no row.
            // Python's `rows_put` RAISES here, and the one dispatch `except`
            // turns that into `{error, raised}` — the refusal IS the answer.
            do {
                ids.append(.integer(try rows.put(aid, row)))
            } catch {
                throw PyError("ValueError", "\(error)")
            }
        }
        return .object(["ids": .array(ids)])
    }
}
