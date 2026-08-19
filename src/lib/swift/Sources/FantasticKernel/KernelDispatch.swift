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

        // Run the dispatch as a REGISTERED, killable task (one row in the
        // kernel's task table) and await it — that is what makes ANY send
        // cancelable, on any bundle. A cancelled dispatch does NOT throw out of
        // `send`: it resolves to `{"cancelled": true}`.
        var reply = await runDispatch(target: target, payload: payload, verb: verb)

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
            // The substrate's OWN verbs — answered natively by EVERY agent, so
            // they appear in no bundle's `verbs` map. Advertised here (once, on
            // the root) so the reflect surface stays honest: one round-trip tells
            // a caller that work in flight is inspectable (`ps`) and killable
            // (`cancel`), on any agent.
            obj["system_verbs"] = Kernel.systemVerbDocs()
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
            // The RECEIVE clause is per-runtime BY DESIGN, because the executors are.
            // Python ships one executor and names its shape inline ("call
            // kernel.on_message(cb) and keep running"). Rust advertises THREE with
            // three different shapes, so it parameterizes the clause instead ("take
            // each turn from kernel.on_message — each executor's exact shape is named
            // below"): the callback wording is WRONG for js/lua, whose readme says
            // "Never pass it a function" — they CALL (js: await) `on_message()` in a
            // LOOP and it returns the next payload. This kernel keeps python's inline
            // clause and lets `executorClause()` append each REGISTERED executor's own
            // receive shape after it (see `Kernel.scriptExecutors`), so a js author
            // reading it is corrected by the tail.
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
    private func runDispatch(target: Agent, payload: JSON, verb: String) async -> JSON {
        // My own task is in a subtree the kernel is killing: register no new work
        // and run no handler. The kernel is INERT to a killed task from the
        // moment of the kill.
        if KernelTaskLocals.currentKilled?.isKilled == true {
            return .object(["cancelled": .bool(true)])
        }

        // Register BEFORE spawning: the row (and its stop handle) must exist the
        // instant the handler can run, or a nested `send` could claim a parent
        // that isn't in the table yet.
        let taskId = tasks.mint(target.id)
        let handle = TaskHandle()
        tasks.register(
            id: taskId,
            agent: target.id,
            verb: verb,
            parent: KernelTaskLocals.currentTask,  // the dispatch we are nested inside, if any
            handle: handle
        )

        let box = DispatchBox()
        let work = Task {
            let reply = await KernelTaskLocals.$currentSender.withValue(target.id) {
                await KernelTaskLocals.$currentTask.withValue(taskId) {
                    await KernelTaskLocals.$currentKilled.withValue(handle.killed) {
                        await self.dispatch(target: target, payload: payload)
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
