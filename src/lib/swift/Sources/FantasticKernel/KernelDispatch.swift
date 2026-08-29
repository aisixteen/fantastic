// `Kernel.send` / `Kernel.emit` + verb routing.
//
// Mirrors Rust's `send.rs` — `kernel.send([Item(target, payload)])[0].result` resolves
// the target via the flat agents map, dispatches through the system
// verb table OR the agent's bundle, publishes a state event, fans
// the payload out to watchers, returns the reply.

import FantasticJSON
import Foundation
import OrderedCollections

extension Kernel {



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
        // Call gate (see `send`) — the binary path carries the same agent-originated
        // write_stream/read_stream sends, gated two-sided. Only an operator skips it.
        if let orig = KernelTaskLocals.currentSender {
            if let denial = gate(sender: orig, verb: verb, target: target, payload: header) {
                return (denial, Data())
            }
        }
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
                    "error": .string(
                        "agent \(target.id) has no handler_module; cannot answer binary")
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
    public func sendAs(sender: AgentId, target: AgentId, payload: JSON) async -> JSON {
        await KernelTaskLocals.$currentSender.withValue(sender) {
            await send([Item(target, payload)])[0].result
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
        // THE CALL GATE on the agent's OWN reflect. A bundle agent builds its own
        // identity and would otherwise never report its authority, so an agent
        // could not read what it may call or who may call it — only discover it
        // by trying. Same two keys the distilled tree nodes carry.
        for k in ["can_call", "callable_by"] where obj[k] == nil {
            if let v = target.metaValue(forKey: k) { obj[k] = v }
        }

        // The KERNEL's own verbs — answered by the ROOT alone (they act on the
        // TREE and the kernel-wide task table), so they appear in no bundle's
        // `verbs` map. Advertised on EVERY agent, not just the root: root-only
        // was tried and reverted — a model that never reflects the root never
        // learns they exist, guesses, and gives up.
        obj["kernel_verbs"] = Kernel.kernelVerbsView(
            full: payload["detail"].asString == "full")

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
            // The kernel's stable instance `guid`. PERSISTENCE-scoped (unlike the
            // run-scoped runtime/env/version above): sourced from the persisted
            // root meta, minted once at boot + read back verbatim thereafter.
            obj["guid"] = target.metaValue(forKey: "guid") ?? .null
            // The execution model is stated HERE, not only behind
            // `readme:'self-extension'`. Measured twice on gemma-4-12B (2026-08-06):
            // the old "the `code` IS the tool" reads as "the code DEFINES the tool",
            // so the model authored `def run(payload): ...` and sent {type:'run'}
            // expecting a call. Two agents shipped inert (one was valid Python),
            // nothing was written, and {queued:true} never appeared because no job
            // stayed alive. It had planned the structure correctly and lost on the
            // contract.
            //
            // WHAT IS SHARED, AND WHAT IS NOT — this used to claim
            // "python/rust/swift say it verbatim", which is not true of rust. Shared
            // word-for-word are the SCRIPT facts, the ones that cost those runs: the
            // `code` is a SCRIPT, it runs TOP TO BOTTOM when created and again on
            // reboot, it survives a reboot, and a bare `def` only defines and exits —
            // so the work goes at module level.
            //
            // The RECEIVE clause names the shape the executor's connector really
            // implements ("call kernel.on_message(cb) and keep running"), and
            // `executorClause()` appends each REGISTERED executor's own receive shape
            // after it (see `Kernel.scriptExecutors`). Deriving the tail rather than
            // hard-coding it is what keeps the advertised shape true for whatever
            // executors a given build actually ships.
            //
            // Overstating agreement is the same defect class as two copies of one fact
            // where one is wrong: it invites the next editor to "restore parity" by
            // copying a clause that is false for another runtime's executors.
            obj["self_extension"] = .string(
                "You can extend yourself: author your own durable tools as script agents. ADD - send 'kernel' {type:'create_agent', handler_module:'<an executor below>', id:'<name>', code:'<your program as one JSON string>'}; the `code` is a SCRIPT: it runs top to bottom when created and again on reboot, and it survives reboot. A bare `def` only defines and exits — put the work at module level. To handle later sends, call kernel.on_message(cb) and keep running. USE - send the tool a turn by its id; a LIVE script acks {queued:true} and streams results back as emitted say/done events tagged with your client_id (watch its id). REMOVE - send 'kernel' {type:'delete_agent', id:'<name>'}. LEARN - before writing `code`, send 'kernel' the payload {type:'reflect', readme:'self-extension'} (readme is a plain string field; do NOT invent a 'readme_key' field) to fetch the authoring doc - it has a COMPLETE copy-pasteable example plus each executor's exact script API, receive pattern, and code-escaping rules. A running script can itself create_agent more agents - tools compose into more tools. "
                    + executorClause()
            )
            // Always-on doc-tree FRAME: `readmes` (the token-annotated topic
            // index) + `docs` (a one-liner teaching keyed fetch). So the manual's
            // map is in front of a caller from the first root reflect.
            obj["readmes"] = readmeIndex(target)
            obj["docs"] = .string(
                "This kernel's manual is a keyed doc tree. To read it, send 'kernel' a reflect payload with a string `readme` field: {type:'reflect', readme:'index'} lists every topic with its ~token cost; {type:'reflect', readme:'<key>'} opens one (e.g. readme:'self-extension', readme:'streams'); {type:'reflect', readme:true} is the short orientation. The field is named exactly `readme` (a string) - not 'readme_key'. Read only what a task needs."
            )
        }
        switch payload["tree"].asString ?? "all" {
        case "all": obj["tree"] = treeNode(target)
        case "ids":
            // PAGED — the cheap id tier is the one most likely to be walked by a
            // caller with a small budget, so it is also the one that most needs
            // a bound. Same cursor law as `list_agents`/`ps`.
            let (ids, next) = page(descendantIds(target), payload) { $0 }
            obj["tree"] = .array(ids.map { .string($0) })
            obj["tree_next"] = next.map { JSON.string($0) } ?? .null
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
        // Polymorphic `readme`: `true` → the SHORT orientation; `"index"` → the
        // topic tree; `"<key>"` → one topic's body. One agent at a time.
        if payload["readme"].asBool == true {
            obj["readme"] = readReadme(target)
        } else if let sel = payload["readme"].asString, !sel.isEmpty {
            obj["readme"] = readmeView(target, sel)
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

    // ─── keyed / tiered doc tree ───────────────────────────────
    // The manual is a SHORT `readme.md` orientation + a `docs/` topic tree.
    // `reflect readme=index` lists it token-annotated; `reflect readme=<key>`
    // drills one topic. Read from the seeded on-disk tree with an embedded
    // fallback for the in-memory root (mirrors `readReadme`).

    /// ~tokens for a doc: ceil(chars/4), matching ai_core's estimate (Unicode
    /// scalars = code points, so the advertised cost is stable across runtimes).
    private func estTokens(_ text: String) -> Int {
        (text.unicodeScalars.count + 3) / 4
    }

    /// A topic's one-line descriptor: the leading `> ` line if present, else
    /// the first prose line (skipping headings).
    private func readDesc(_ text: String) -> String {
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
        for line in lines where line.hasPrefix("> ") {
            return String(line.dropFirst(2)).trimmingCharacters(in: .whitespaces)
        }
        for line in lines {
            let s = line.trimmingCharacters(in: .whitespaces)
            if !s.isEmpty, !s.hasPrefix("#"), !s.hasPrefix(">") { return s }
        }
        return ""
    }

    /// `(key, body)` topics for an agent: the seeded on-disk `docs/` tree if
    /// present, else the embedded canonical docs for the root (in-memory mode).
    /// FLAT for this runtime.
    private func docsFor(_ target: Agent) -> [(key: String, body: String)] {
        let dir = target.rootPath.appendingPathComponent("docs")
        let fm = FileManager.default
        if storage.isDisk, let names = try? fm.contentsOfDirectory(atPath: dir.path) {
            let mds = names.filter { $0.hasSuffix(".md") }.sorted()
            if !mds.isEmpty {
                return mds.compactMap { name in
                    guard
                        let s = try? String(
                            contentsOf: dir.appendingPathComponent(name), encoding: .utf8)
                    else { return nil }
                    return (String(name.dropLast(3)), s)
                }
            }
        }
        return target.parentId == nil ? RootDocs.all : []
    }

    /// Token-annotated topic index `[{key, desc, tokens}]` — no bodies.
    private func readmeIndex(_ target: Agent) -> JSON {
        .array(
            docsFor(target).map { key, body in
                .object([
                    "key": .string(key),
                    "desc": .string(readDesc(body)),
                    "tokens": .integer(Int64(estTokens(body))),
                ])
            })
    }

    /// Polymorphic `readme=<str>`: `"index"` → the token-annotated tree; a
    /// `<key>` → that topic's body; unknown/escaping key → `{error, keys}`.
    private func readmeView(_ target: Agent, _ sel: String) -> JSON {
        if sel == "index" { return .object(["index": readmeIndex(target)]) }
        let docs = docsFor(target)
        let keys = JSON.array(docs.map { .string($0.key) })
        let safe =
            sel
            .trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            .replacingOccurrences(of: "\\", with: "/")
        if safe.isEmpty || safe.split(separator: "/").contains("..") {
            return .object(["error": .string("bad key '\(sel)'"), "keys": keys])
        }
        guard let hit = docs.first(where: { $0.key == safe }) else {
            return .object(["error": .string("no doc '\(sel)'"), "keys": keys])
        }
        return .object([
            "key": .string(hit.key),
            "desc": .string(readDesc(hit.body)),
            "tokens": .integer(Int64(estTokens(hit.body))),
            "body": .string(hit.body),
        ])
    }

    // ── internals ───────────────────────────────────────────────

    /// Run ONE dispatch as a registered, killable task and await it — the unit
    /// of the task table.
    ///
    /// **Parent propagation.** An unstructured `Task {}` INHERITS the enclosing
    /// task-locals, so the scopes are re-established INSIDE the spawned task:
    /// `currentSender` (the target, as before), `currentTask` (this dispatch's
    /// OWN id, so any `send` the handler issues registers as a CHILD of it — the
    /// table is a tree), and `currentKilled` (its own dying flag). The PARENT
    /// link is read HERE, in the caller's task, before the spawn.
    ///
    /// **The cancellation contract, and why it is not ambiguous.** Both cases
    /// surface at this one await as `nil` (the box was resolved by a `stop`):
    ///   - THE DISPATCH was killed (someone sent `cancel`) → the awaiting caller
    ///     is NOT being killed; it gets the reply `{"cancelled": true}`.
    ///     Cancellation never throws out of `send`.
    ///   - WE were killed too (our own task is inside the killed subtree) → the
    ///     same `{"cancelled": true}` lets our handler unwind through its OWN
    ///     cleanup (Swift cannot force-unwind it the way Rust drops a future, and
    ///     parking it forever would strand every lock it holds). It cannot
    ///     resurrect itself: the `currentKilled` guard below refuses every
    ///     further `send` from a killed task — no new row, no handler — which is
    ///     exactly the point at which Rust would have dropped the future.
    /// `runDispatch`, classified — and with the ITEM'S OWN DEADLINE enforced.
    ///
    /// Expiry kills through the FRONT DOOR (`cancelTask`) so every bundle's
    /// `onCancel` hook runs BEFORE the abort; dropping the task would strand the
    /// OS resources those hooks release (a subprocess, a PTY, a socket).
    ///
    /// The race is reported honestly: a handler that finishes during the grace
    /// window COMPLETES — `cancelTask` no-ops on a finished id.
    func runDispatchOutcome(
        target: Agent, payload: JSON, verb: String, taskOut: TaskIdBox?,
        timeout: Double?, taskId: TaskId
    ) async -> (JSON, Status) {
        guard let secs = timeout else {
            let r = await runDispatch(
                target: target, payload: payload, verb: verb, taskOut: taskOut, taskId: taskId)
            return classify(r)
        }
        // Race the dispatch against the deadline. The loser is cancelled by the
        // group, and the deadline branch kills through the front door first.
        let killer = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(secs * 1_000_000_000))
            guard let self, !Task.isCancelled else { return }
            // Operator authority: the kernel closing out work it was told to
            // kill, not the batch caller reaching out — so a gated caller never
            // makes an `onCancel` fail.
            _ = await KernelTaskLocals.$currentSender.withValue(nil) {
                await self.cancelTask(taskId)
            }
        }
        let r = await runDispatch(
            target: target, payload: payload, verb: verb, taskOut: taskOut, taskId: taskId)
        let fired = killer.isCancelled == false && r["cancelled"].asBool == true
        killer.cancel()
        if fired {
            return (
                Refusal.make(
                    Refusal.timeout,
                    "\(verb) on \(target.id.value) timed out after \(secs)s"),
                .timedOut
            )
        }
        return classify(r)
    }

    /// A reply's terminal state. A handler's own `{error:…}` is a COMPLETED
    /// answer — the kernel does not introspect domain replies; only a THROW
    /// (`raised`) or a kill is a non-completion.
    private func classify(_ r: JSON) -> (JSON, Status) {
        if r["cancelled"].asBool == true { return (r, .cancelled) }
        if r["raised"].asBool == true { return (r, .failed) }
        return (r, .completed)
    }

    private func runDispatch(
        target: Agent, payload: JSON, verb: String, taskOut: TaskIdBox? = nil,
        taskId: TaskId? = nil
    ) async -> JSON {
        // My own task is in a subtree the kernel is killing: register no new work
        // and run no handler. The kernel is INERT to a killed task from the
        // moment of the kill.
        if KernelTaskLocals.currentKilled?.isKilled == true {
            return .object(["cancelled": .bool(true)])
        }

        // Register BEFORE spawning: the row (and its stop handle) must exist the
        // instant the handler can run, or a nested `send` could claim a parent
        // that isn't in the table yet.
        // The pid was minted in the SYNCHRONOUS launch phase and is already the
        // ledger key for an unnamed execution — minting a second one here would
        // split the two apart, so the row and the reply would name different
        // things and a pid could not re-fetch its own outcome.
        let taskId = taskId ?? tasks.mint(target.id)
        // Hand the PID back SYNCHRONOUSLY, at dispatch — before any reply exists. A
        // caller that wants to correlate the answer needs the id at send time, not
        // after; that is the whole point of the id.
        taskOut?.value = taskId
        let handle = TaskHandle()
        tasks.register(
            id: taskId,
            agent: target.id,
            verb: verb,
            parent: KernelTaskLocals.currentTask,  // the dispatch we are nested inside, if any
            handle: handle
        )

        // The CALLER, read HERE in the caller's own context — inside the
        // dispatch `currentSender` becomes the target, so this is the last place
        // the origin is still visible.
        let origin = KernelTaskLocals.currentSender

        let box = DispatchBox()
        let work = Task {
            let reply = await KernelTaskLocals.$currentOrigin.withValue(origin) {
                await KernelTaskLocals.$currentSender.withValue(target.id) {
                await KernelTaskLocals.$currentTask.withValue(taskId) {
                    await KernelTaskLocals.$currentKilled.withValue(handle.killed) {
                            await self.dispatch(target: target, payload: payload)
                        }
                    }
                }
            }
            box.resolve(reply)
        }
        handle.attach(task: work, box: box)

        // Structured concurrency: if the CALLER goes away (its own task is
        // cancelled — a timeout, a dropped request, its own kill), the dispatch
        // it spawned goes with it, exactly as when dispatch ran inline.
        let outcome = await withTaskCancellationHandler {
            await box.wait()
        } onCancel: {
            handle.stop()
        }

        // The row's self-cleaner: the dispatch is over on every exit path.
        // (`cancel` may already have removed it — `forget` is idempotent.)
        handle.stop()
        tasks.forget(taskId)

        // `nil` = the task was stopped. Cancellation is a REPLY, never a throw.
        return outcome ?? .object(["cancelled": .bool(true)])
    }

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
            // An exception is a RESULT. A handler that throws must not take its
            // caller down with it — the caller can act on `{"error": …}` and can
            // act on nothing at all. `raised: true` distinguishes a handler that
            // BLEW UP from one that deliberately answered `{"error": …}`, which is
            // the difference between a bug in the target and a refusal by it. The
            // AI routing path relies on it (python builds the same three-key body
            // in `_route_reply`'s own `except`, adding the `target`).
            return .object(["error": .string("\(error)"), "raised": .bool(true)])
        }
    }

    /// Substrate verbs answered natively on every agent.
    /// `reflect` is intentionally NOT in this list — it's per-bundle
    /// (the bare agent path inside `dispatch` handles bare reflect).
    static func isSystemVerb(_ verb: String) -> Bool {
        switch verb {
        case "create_agent", "delete_agent", "update_agent", "list_agents", "get":
            return true
        // NAMED and unnamed executions and what they became. Terminal
        // non-completed rows ARE the dead-letter queue.
        case "ledger":
            return true
        // THE BLOB CHANNEL — an agent's sidecars, persisted through the store.
        // The consumer never computes a path and never wires a provider.
        case "persist_blob", "load_blob":
            return true
        // The TASK TABLE — work in flight, universal. Baked into the substrate,
        // in no bundle's verb map: cancellation is a property of the one
        // primitive (`send`), so EVERY agent is cancelable and a caller never
        // needs to know which bundle it faces. Plain verbs — an AGENT can call
        // them on another agent's work.
        case "ps", "cancel":
            return true
        default:
            return false
        }
    }
}

// MARK: - Telemetry helper

/// Compact summary for state events. Mirrors py/rust `summarize_payload`.
///
/// NOT TRUNCATED. This used to cut at 160 chars + "...". A summary that silently
/// drops the end of a payload lies about what was sent — the reader cannot tell a
/// short payload from a cut one, and the dropped part is exactly what you opened
/// telemetry to find. Eliding is a RENDER decision, where it is visible.
func summarize(_ payload: JSON) -> String {
    payload.serialize()
}

extension Kernel {
    /// The two-sided call gate for an agent-originated send. Returns the error
    /// `JSON` if the send is refused, else `nil`. Checks egress (the sender's
    /// `can_call`) then ingress (the target's `callable_by`), both via the one
    /// `authorizeCall` engine, then the authority-conferral rules (no self-raise;
    /// delegate only a subset). A holder with no record → empty grant → deny-all.
    /// May `origin` receive the outcome of an execution that ran `verb` on
    /// `target`? A denial, or nil.
    ///
    /// Joining hands back ANOTHER caller's result verbatim, so it is equivalent
    /// to having made that call. Asking the gate about the joiner's own payload
    /// instead — a different verb, whose reply it never gets — authorizes the
    /// wrong operation. An OPERATOR joiner (no origin) is ungated, as everywhere.
    func joinDenial(origin: AgentId?, verb: String, target: Agent) -> JSON? {
        guard let joiner = origin else { return nil }
        return gate(
            sender: joiner, verb: verb, target: target,
            payload: .object(["type": .string(verb)]))
    }

    func gate(sender: AgentId, verb: String, target: Agent, payload: JSON) -> JSON? {
        let loaderId = root?.id.value
        let senderAgent = agent(sender)
        let sCan = senderAgent?.metaValue(forKey: "can_call") ?? .null
        let sParent = senderAgent?.parentId?.value
        let sKids = Set(senderAgent?.childIds().map { $0.value } ?? [])

        // FORKING YOUR OWN SUBTREE IS BASELINE — and the exemption keys on
        // PLACEMENT, not on who answers. `create_agent` is privileged, so a
        // wildcard never covers it; the one documented exception is that growing
        // your OWN subtree needs no grant, which is what makes self-extension
        // work with zero wiring.
        //
        // While the verb was answered by every agent that fell out for free
        // (`otherId == holderId`, because you addressed yourself). Now the verb
        // is kernel-owned and the target is always the root, so that exemption
        // could never fire again and EVERY agent would need an explicit
        // `create_agent` grant on the root just to author its own tool. The
        // property being preserved was never "you addressed yourself" — it is
        // "the new agent lands in MY subtree", which `parent_id` states outright.
        let placementIsOwn =
            verb == "create_agent" && payload["parent_id"].asString == sender.value

        // Egress — may the SENDER call the target?
        let d = authorizeCall(
            grant: sCan, verb: verb, otherId: target.id.value,
            holderId: sender.value, parentId: sParent, childIds: sKids, loaderId: loaderId,
            enforcePrivileged: !placementIsOwn)
        if !d.allowed {
            return .object([
                "error": .string("\(sender.value) may not call '\(verb)' on \(target.id.value)"),
                "reason": .string("unauthorized"), "side": .string("egress"),
                "hint": .string(d.hint),
            ])
        }

        // INGRESS IS NOT CHECKED FOR A KERNEL VERB AT THE ROOT. `callable_by`
        // answers "do I accept being acted upon", and the root is not what is
        // acted upon — the operand is in the payload. Requiring it would make
        // the operator edit the ROOT's record for every agent that wants to read
        // the task table. Consequence accepted: an agent granted
        // `<root>:[ps,cancel]` can kill ANY task; the per-victim edge a manifest
        // could grant before is GONE.
        if Kernel.isSystemVerb(verb) && target.parentId == nil {
            // ...but CONFERRAL still applies. `update_agent` is a system verb
            // whose target is ALWAYS the root, so returning nil flat made
            // `checkConferral` unreachable for the exact case it guards: any
            // agent holding `<root>:update_agent` could rewrite its OWN
            // can_call and grant itself egress to the whole tree — the
            // deny-all gate defeated from below. Python runs the check on this
            // path (`_agent.py`, `verb in _SYSTEM_VERBS and target.parent is
            // None`); swift did not. Skipping INGRESS here is deliberate and
            // unchanged — skipping conferral was not.
            if verb == "create_agent" || verb == "update_agent" {
                return checkConferral(
                    sender: sender, sCan: sCan, sParent: sParent, sKids: sKids,
                    loaderId: loaderId, payload: payload)
            }
            return nil
        }

        // Ingress — does the TARGET accept the sender? (tokens relative to target;
        // privileged-no-wildcard is egress-only, so enforcePrivileged: false.)
        let tCall = target.metaValue(forKey: "callable_by") ?? .null
        let tParent = target.parentId?.value
        let tKids = Set(target.childIds().map { $0.value })
        let di = authorizeCall(
            grant: tCall, verb: verb, otherId: sender.value,
            holderId: target.id.value, parentId: tParent, childIds: tKids, loaderId: loaderId,
            enforcePrivileged: false)
        if !di.allowed {
            return .object([
                "error": .string(
                    "\(target.id.value) does not accept '\(verb)' from \(sender.value)"),
                "reason": .string("unauthorized"), "side": .string("ingress"),
                "hint": .string(di.hint),
            ])
        }

        // Authority conferral — no self-raise, delegate only a subset you hold.
        if verb == "create_agent" || verb == "update_agent" {
            return checkConferral(
                sender: sender, sCan: sCan, sParent: sParent, sKids: sKids,
                loaderId: loaderId, payload: payload)
        }
        return nil
    }

    /// Authority flows from ABOVE only: an agent may not raise its OWN authority
    /// fields, and may confer on another only a SUBSET of its own `can_call`.
    private func checkConferral(
        sender: AgentId, sCan: JSON, sParent: String?, sKids: Set<String>,
        loaderId: String?, payload: JSON
    ) -> JSON? {
        guard case .object(let obj) = payload,
            obj["can_call"] != nil || obj["callable_by"] != nil
        else { return nil }
        let verb = payload["type"].asString ?? ""
        let edited = payload["id"].asString
        // Self-raise: never edit your own authority fields.
        if verb == "update_agent" && edited == sender.value {
            return .object([
                "error": .string("\(sender.value) may not edit its own can_call/callable_by"),
                "reason": .string("unauthorized"), "side": .string("self-grant"),
                "hint": .string(fromAbove),
            ])
        }
        // Subset: every LATERAL edge in the conferred `can_call` must be one the
        // granter itself holds (structural tokens free; `callable_by` is unbounded).
        if case .object(let conferred) = payload["can_call"] {
            for (key, verbs) in conferred {
                if key.hasPrefix("$") { continue }
                // Conferring "*" (or a list with it) requires the granter to hold
                // "*" on that target; a concrete list requires each verb.
                var probe: [String] = ["*"]
                if case .array(let vs) = verbs, !vs.contains(where: { $0.asString == "*" }) {
                    probe = vs.compactMap { $0.asString }
                }
                for v in probe {
                    let ok = authorizeCall(
                        grant: sCan, verb: v, otherId: key, holderId: sender.value,
                        parentId: sParent, childIds: sKids, loaderId: loaderId
                    ).allowed
                    if !ok {
                        return .object([
                            "error": .string(
                                "\(sender.value) cannot delegate '\(v)' on \(key) — you may "
                                    + "only confer can_call you already hold"),
                            "reason": .string("unauthorized"), "side": .string("delegation"),
                            "hint": .string(fromAbove),
                        ])
                    }
                }
            }
        }
        return nil
    }
}
