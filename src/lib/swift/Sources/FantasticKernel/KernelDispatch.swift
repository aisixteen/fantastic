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
    ) async -> (JSON, Data?) {
        let target: Agent? = targetId.value == "kernel" ? root : agent(targetId)
        guard let target = target else {
            // The SAME refusal the text channel gives — coded, and carrying the
            // live id index so the caller can see what it could have meant.
            return (
                Refusal.unknown(targetId.value, live: allAgents().map { $0.id.value }), nil
            )
        }
        let verb = header["type"].asString ?? ""
        // Call gate (see `send`) — the binary path carries the same agent-originated
        // write_stream/read_stream sends, gated two-sided. Only an operator skips it.
        if let orig = KernelTaskLocals.currentSender {
            if let denial = gate(sender: orig, verb: verb, target: target, payload: header) {
                return (denial, nil)
            }
        }
        // BEFORE the handler, with the body: this is the frame python puts on a
        // watcher's inbox for a `write_stream`, and the only reason a watcher can
        // ever see the bytes at all. The `drain` event follows on every exit path.
        target.inFlightEnter()
        defer {
            target.inFlightLeave()
            publishState(Kernel.drainEvent(target.id, target.inFlight))
        }
        fanout(target, "send", Kernel.binaryItem(header, blob))

        let (reply, body): (JSON?, Data?) = await KernelTaskLocals.$currentSender.withValue(
            target.id
        ) {
            await dispatchBinary(target: target, header: header, blob: blob)
        }
        return (reply ?? .null, body)
    }

    /// C34: the SAME refusal texts (+ `verb`) as the text channel — python has
    /// no separate binary dispatch at all, a binary frame runs through the
    /// same `_dispatch` resolution the text channel does.
    func dispatchBinary(target: Agent, header: JSON, blob: Data) async -> (JSON?, Data?) {
        let verb = header["type"].asString ?? ""
        guard let hm = target.handlerModule else {
            return (noHandlerModuleRefusal(target.id.value, verb), nil)
        }
        guard let bundle = bundles.get(hm) else {
            return (notInstalledRefusal(target.id.value, hm, verb), nil)
        }
        do {
            return try await bundle.handleBinary(
                agentId: target.id, header: header, blob: blob, kernel: self)
        } catch {
            // An exception is a RESULT, exactly as the text channel's catch
            // treats it — `raised: true` distinguishes a handler that BLEW UP
            // from one that deliberately answered `{error}`.
            return (.object(["error": .string("\(error)"), "raised": .bool(true)]), nil)
        }
    }

    /// `agent <id!r> has no handler_module; cannot answer verb <verb!r>` —
    /// SHARED by text and binary dispatch (C34). Python `_agent.py:2069-2071`.
    private func noHandlerModuleRefusal(_ id: String, _ verb: String) -> JSON {
        Refusal.make(
            Refusal.unhandled,
            "agent \(pyRepr(id)) has no handler_module; cannot answer verb \(pyRepr(verb))",
            extra: [("verb", .string(verb))])
    }

    /// `agent <id!r>: handler_module <hm!r> cannot be imported in this
    /// runtime: <ExceptionClass>: <message>` — SHARED by text and binary
    /// dispatch (C24/C34). Python's PREFIX, verbatim; the TAIL is not stable
    /// across python's own runs (D3 residual — measured "No module named
    /// 'yaml_state'" one round, "No module named 'yaml_state.tools'" the
    /// next), so this names the closest static equivalent of the dynamic
    /// import failure rather than pinning python's exact wording — the same
    /// residual rust picked, for cross-runtime consistency.
    private func notInstalledRefusal(_ id: String, _ hm: String, _ verb: String) -> JSON {
        Refusal.make(
            Refusal.unhandled,
            "agent \(pyRepr(id)): handler_module \(pyRepr(hm)) cannot be imported in this "
                + "runtime: ModuleNotFoundError: No module named \(pyRepr(hm))",
            extra: [("verb", .string(verb))])
    }

    /// Send WITH explicit sender attribution. State events tag this
    /// dispatch as originating from `senderId`. Used by web_ws.
    public func sendAs(sender: AgentId, target: AgentId, payload: JSON) async -> JSON {
        await KernelTaskLocals.$currentSender.withValue(sender) {
            await send([Item(target, payload)])[0].result
        }
    }

    /// Fire an event into `targetId`'s inbox without dispatching. Publishes a
    /// `kind:"emit"` state event and mirrors to watchers — the same fanout a
    /// dispatch runs, minus the in-flight bump.
    ///
    /// AN EMIT TO A NON-AGENT ID IS A NO-OP. The target agent is resolved FIRST:
    /// no agent, no inbox put, no state event. Swift used to deliver into any
    /// registered inbox and publish regardless, so a synthetic WS client id
    /// (`_ws_<hex>`) produced telemetry for a sprite that does not exist —
    /// python drops it silently (`_proxy.py:196-200`).
    public func emit(_ targetId: AgentId, _ payload: JSON) async {
        await emitItem(targetId, .text(payload))
    }

    /// `emit` for a payload carrying raw bytes — a `read_stream` chunk a bridge
    /// re-emits from a remote watch, which must reach a local watcher WHOLE.
    /// Same no-op rule for a non-agent id.
    public func emitItem(_ targetId: AgentId, _ item: InboxItem) async {
        guard let target = agent(targetId) else { return }
        fanout(target, "emit", item)
    }

    /// The escalation line every DISTILLED reflect carries — python
    /// `_MORE_HINT`, verbatim. Escalation rides in the REPLY itself, never in
    /// any caller's system prompt.
    static let moreHint =
        "Distilled: every agent and what it is FOR. Reflect one agent for its verbs "
        + "(<type>reflect</type>). Manual: <type>reflect</type> <readme>index</readme> - keys "
        + "include system-verbs (ps/cancel/create_agent args) and self-extension (author your "
        + "own durable tools). <detail>full</detail> on a reflect adds wiring and posture - ask "
        + "for it only when you need to see how agents are plumbed, not to find one."

    /// PURPOSE, not wiring — the reflect keys a roster node copies beyond its
    /// `sentence` (python `_PURPOSE_KEYS`).
    static let purposeKeys = ["mode"]

    /// Append the composable reflect flags to any reflect reply —
    /// applied uniformly to bare-agent and bundle reflects. `tree`
    /// defaults to `all`, `bundles` to `none`, `readme` to false.
    /// Non-object replies pass through.
    ///
    /// Returns the RAISED reply of a roster node whose own reflect blew up —
    /// python's exception propagates out of `_roster` and fails the whole
    /// reflect, so the caller settles it `failed` with that body. `nil` = fine.
    func applyReflectFlags(_ target: Agent, _ payload: JSON, _ reply: inout JSON) async -> JSON? {
        guard case .object(var obj) = reply else { return nil }
        // A REFUSAL IS NOT A REFLECT. Decorating one with kernel_verbs/tree/more
        // produced a reply that reads like a healthy reflect with a stray error
        // field, and a caller could not tell the two apart (2026-09-02).
        if obj["error"] != nil { return nil }
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
            // DUCK-TYPED ROOT EXTENSION (python `_agent.py:1697-1709`). The
            // root's handler_module — the `kernel_state` loader — contributes
            // the root-only fields the substrate cannot compute without
            // coupling to bundle logic: `persistence: {provider}`, the instance
            // `guid`, the doc-tree frame and the `self_extension` shape. No
            // import here: the registry already holds the bundle, and a root
            // whose bundle is not installed in this runtime contributes
            // nothing.
            if let hm = target.handlerModule, let bundle = bundles.get(hm) {
                do {
                    if let extra = try bundle.reflectRootExtra(
                        agent: target, full: payload["detail"].asString == "full", kernel: self)
                    {
                        for (k, v) in extra { obj[k] = v }
                    }
                } catch {
                    // THE HOOK IS BEST EFFORT. Python wraps the whole duck-typed
                    // call in a bare `except Exception: pass` (`_agent.py`), so a
                    // root whose frame cannot be built contributes NOTHING and the
                    // reflect answers without it — measured on a `docs/` holding a
                    // non-UTF-8 file: `detail=full` on the root drops
                    // `persistence`/`readmes`/`docs`/`self_extension` and still
                    // returns the tree. C60's raise is the `readme=<key>` path,
                    // which is not this one.
                }
            }
        }
        // DECIDED BY KEY PRESENCE (python `payload.get("tree", "all")`): absent
        // is `all`; a present `all`/`ids` acts; any OTHER present value — null,
        // a bool, a number, a list, `none` — omits the tree. Reading it as
        // `asString ?? "all"` made `tree:null` and `tree:true` insert the whole
        // tree, which is the one answer the caller did not ask for.
        switch payload.asObject?["tree"] {
        case .none, .some(.string("all")):
            // DISTILLED IS THE GATED ROSTER: `{id, sentence?, description?,
            // mode?, children}`, each sentence ASKED FROM THE AGENT ITSELF.
            // `detail:full` keeps the wiring node.
            if payload["detail"].asString == "full" {
                obj["tree"] = treeNode(target)
            } else {
                switch await roster(target) {
                case .node(let node): obj["tree"] = node
                case .raised(let raised): return raised
                }
            }
        case .some(.string("ids")):
            // PAGED — the cheap id tier is the one most likely to be walked by a
            // caller with a small budget, so it is also the one that most needs
            // a bound. Same cursor law as `list_agents`/`ps`; the cursor is
            // `next`, and it is present ONLY when a further page exists.
            let (ids, next) = page(descendantIds(target), payload) { $0 }
            obj["tree"] = .array(ids.map { .string($0) })
            if let next { obj["next"] = .string(next) }
        default: break
        }
        if payload["detail"].asString != "full" { obj["more"] = .string(Kernel.moreHint) }
        // The catalog pages with the SAME `limit`/`after` as `tree=ids`, and
        // names its cursor `bundles_next` only when a further page follows.
        switch payload.asObject?["bundles"] {
        case .some(.string("all")):
            let (cat, next) = page(availableBundles(), payload) { $0.name }
            obj["bundles"] = .array(
                cat.map {
                    .object([
                        "name": .string($0.name),
                        "handler_module": .string($0.handlerModule),
                    ])
                })
            if let next { obj["bundles_next"] = .string(next) }
        case .some(.string("ids")):
            let (names, next) = page(availableBundles().map { $0.name }, payload) { $0 }
            obj["bundles"] = .array(names.map { .string($0) })
            if let next { obj["bundles_next"] = .string(next) }
        default: break  // "none" → omit
        }
        // Polymorphic `readme`: `true` → the SHORT orientation; `"index"` → the
        // topic tree; `"<key>"` → one topic's body. One agent at a time.
        //
        // A non-UTF-8 doc is NOT caught (python catches only `OSError`; a
        // `UnicodeDecodeError` propagates and fails the WHOLE reflect — C60).
        if payload["readme"].asBool == true {
            do {
                obj["readme"] = try readReadme(target)
            } catch {
                reply = .object(obj)
                return .object(["error": .string(error.description), "raised": .bool(true)])
            }
        } else if let sel = payload["readme"].asString, !sel.isEmpty {
            do {
                obj["readme"] = try readmeView(target, sel)
            } catch {
                reply = .object(obj)
                return .object(["error": .string(error.description), "raised": .bool(true)])
            }
        }
        reply = .object(obj)
        return nil
    }

    /// `{id, sentence?, description?, mode?, children:[...]}` — the tree,
    /// carrying what each agent is FOR instead of how it is wired (python
    /// `_roster`).
    ///
    /// The sentence is ASKED FROM THE AGENT ITSELF, never mirrored into a
    /// parallel source, so it cannot drift from what the agent actually says: a
    /// node with a handler is reflected through `internalDispatch` (a real call
    /// — row, task row, watcher echo, state events — but WITHOUT the reflect
    /// flags, so a roster never recurses into rosters); a bare node answers its
    /// identity directly.
    ///
    /// GATED PER NODE. `reflect` is not exempt from the call gate anywhere
    /// else, and this walk dispatched it directly — so any caller allowed to
    /// reflect ONE agent read every descendant's `sentence`/`description`
    /// through the tree. A denied node keeps only its `id` and `children` (the
    /// SHAPE is already public through `tree=ids`) and is not dispatched at all,
    /// so its watchers see nothing either. An operator (no sender) sees all.
    ///
    /// `.failure` carries a node's RAISED reflect: python's exception
    /// propagates out of `_roster` and fails the whole reflect.
    func roster(_ a: Agent) async -> RosterResult {
        let mayReflect =
            KernelTaskLocals.currentSender.map {
                gate(sender: $0, verb: "reflect", target: a, payload: .object([:])) == nil
            } ?? true
        var raw: JSON = .object([:])
        if a.handlerModule != nil && mayReflect {
            let (reply, status) = await internalDispatch(a, .object(["type": .string("reflect")]))
            if status == .failed { return .raised(reply) }
            raw = reply
        } else if mayReflect {
            raw = reflectIdentity(a)
        }
        var node: OrderedDictionary<String, JSON> = ["id": .string(a.id.value)]
        if pyTruthy(raw["sentence"]) { node["sentence"] = raw["sentence"] }
        // Operator-set purpose. More specific than the bundle sentence when
        // present ("the prod deploy key" vs "a yaml store").
        if mayReflect, let d = a.descriptionMeta { node["description"] = .string(d) }
        // Purpose comes from the agent's REFLECT, not its record: much of what
        // an agent is FOR lives in its reflect, not as a record field.
        for k in Kernel.purposeKeys {
            let v = raw[k]
            if case .null = v { continue }
            node[k] = v
        }
        var children: [JSON] = []
        for c in a.childIds().compactMap({ self.agent($0) }) {
            switch await roster(c) {
            case .node(let n): children.append(n)
            case .raised(let raised): return .raised(raised)
            }
        }
        node["children"] = .array(children)
        return .node(.object(node))
    }

    /// The addressed agent's readme.md (string) or null. Python's `readme`
    /// property: `read_text(encoding="utf-8")` when the file exists, `None`
    /// otherwise or on an `OSError`. NO on-disk fallback (C33): an existing
    /// but empty file answers `""`, not the embedded canonical text — that
    /// text is for the in-memory root ONLY, which has no on-disk file at all.
    /// A non-UTF-8 file is NOT an `OSError` — it throws (C60).
    private func readReadme(_ target: Agent) throws(PyError) -> JSON {
        if storage.isDisk {
            guard let data = try? Data(contentsOf: target.readmeFile) else { return .null }
            if let decodeError = pyUtf8DecodeError(data) {
                throw PyError("UnicodeDecodeError", decodeError)
            }
            return .string(pyUniversalNewlines(String(decoding: data, as: UTF8.self)))
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
    /// the first prose line (skipping headings). Python `str.splitlines()` /
    /// `str.strip()` (C59) — NOT a bare `\n` split / ASCII-whitespace trim,
    /// which miss python's wider boundary/strip sets.
    private func readDesc(_ text: String) -> String {
        let lines = pySplitlines(text)
        for line in lines where line.hasPrefix("> ") {
            return pyStrip(String(line.dropFirst(2)))
        }
        for line in lines {
            let s = pyStrip(line)
            if !s.isEmpty, !s.hasPrefix("#"), !s.hasPrefix(">") { return s }
        }
        return ""
    }

    /// The topic TREE for an agent: the seeded on-disk `docs/` dir if present
    /// (NO fallback — an empty/absent dir is `[]`, C33), else the embedded
    /// canonical docs for the root (in-memory mode, swift-only). Nested dirs
    /// are tiers — python's `_readme_index` walk (`RootDocs.walk`).
    private func docsFor(_ target: Agent) throws(PyError) -> [RootDocs.Node] {
        if storage.isDisk {
            return try RootDocs.walk(target.rootPath.appendingPathComponent("docs"))
        }
        return target.parentId == nil ? RootDocs.tree : []
    }

    /// Token-annotated topic index `[{key, desc, tokens, children?}]` — no
    /// bodies, nesting preserved. Python `_readme_index`.
    private func readmeIndex(_ nodes: [RootDocs.Node]) -> JSON {
        .array(
            nodes.map { n in
                var e: OrderedDictionary<String, JSON> = [
                    "key": .string(n.key),
                    "desc": .string(readDesc(n.body)),
                    "tokens": .integer(Int64(estTokens(n.body))),
                ]
                if !n.children.isEmpty { e["children"] = readmeIndex(n.children) }
                return .object(e)
            })
    }

    /// The token-annotated topic index for an agent — the ROOT loader bundle
    /// serves it as the `readmes` half of its `reflectRootExtra` frame.
    public func readmeIndex(_ target: Agent) throws(PyError) -> JSON {
        readmeIndex(try docsFor(target))
    }

    /// Polymorphic `readme=<str>`: `"index"` → the token-annotated tree; a
    /// `<key>` → that topic's body + its immediate sub-docs' index; an
    /// unknown/escaping key → `{error, keys}` (the reply stays well-formed).
    /// Python `_readme_view`, its texts included: `bad key` is a path that
    /// resolves OUTSIDE `docs/`, `no doc` is one inside it with no file.
    ///
    /// DISK mode goes through [`readmeViewOnDisk`] — python's own path
    /// semantics, straight off the real filesystem (R2-RFL-5). The in-memory
    /// root (swift-only; python has no such mode) keeps the tree-search below.
    private func readmeView(_ target: Agent, _ sel: String) throws(PyError) -> JSON {
        if storage.isDisk {
            return try readmeViewOnDisk(target.rootPath.appendingPathComponent("docs"), sel)
        }
        let docs = try docsFor(target)
        if sel == "index" { return .object(["index": readmeIndex(docs)]) }
        let keys = JSON.array(RootDocs.keys(docs).map { .string($0) })
        let safe =
            sel
            .trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            .replacingOccurrences(of: "\\", with: "/")
        // `docs/<safe>.md` resolved the way pathlib resolves it: `.` and empty
        // segments vanish, `..` pops — and popping past the root is the clamp.
        var stack: [String] = []
        for part in safe.split(separator: "/", omittingEmptySubsequences: true) {
            if part == "." { continue }
            if part == ".." {
                if stack.popLast() == nil {
                    return .object(["error": .string("bad key \(pyRepr(sel))"), "keys": keys])
                }
                continue
            }
            stack.append(String(part))
        }
        let lookup = stack.joined(separator: "/")
        func find(_ nodes: [RootDocs.Node]) -> RootDocs.Node? {
            for n in nodes {
                if n.key == lookup { return n }
                if let hit = find(n.children) { return hit }
            }
            return nil
        }
        guard !lookup.isEmpty, let hit = find(docs) else {
            return .object(["error": .string("no doc \(pyRepr(sel))"), "keys": keys])
        }
        var view: OrderedDictionary<String, JSON> = [
            "key": .string(safe),
            "desc": .string(readDesc(hit.body)),
            "tokens": .integer(Int64(estTokens(hit.body))),
            "body": .string(hit.body),
        ]
        if !hit.children.isEmpty { view["children"] = readmeIndex(hit.children) }
        return .object(view)
    }


    /// An absolute filesystem path's components (no leading empty piece).
    private func absPathComponents(_ path: String) -> [String] {
        path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
    }



    /// DISK-mode `readme=<str>` — python's own semantics: a lexical key check,
    /// then the index, then the file.
    private func readmeViewOnDisk(_ docsDir: URL, _ sel: String) throws(PyError) -> JSON {
        func keys() throws(PyError) -> JSON {
            .array(RootDocs.keys(try RootDocs.walk(docsDir)).map { .string($0) })
        }
        if sel == "index" {
            return .object(["index": readmeIndex(try RootDocs.walk(docsDir))])
        }
        let safe =
            sel
            .trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            .replacingOccurrences(of: "\\", with: "/")
        // LEXICAL, then the key list — never the filesystem (python 2026-09-20):
        // a `..` segment is a bad key whether or not the path it walked through
        // exists, and a key is a doc only if the index lists it.
        let parts = safe.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        if safe.isEmpty || parts.contains(where: { $0 == "" || $0 == "." || $0 == ".." }) {
            return .object(["error": .string("bad key \(pyRepr(sel))"), "keys": try keys()])
        }
        guard RootDocs.keys(try RootDocs.walk(docsDir)).contains(safe) else {
            return .object(["error": .string("no doc \(pyRepr(sel))"), "keys": try keys()])
        }
        let fPath = docsDir.appendingPathComponent(safe + ".md").path
        let fm = FileManager.default
        guard let data = fm.contents(atPath: fPath) else {
            return .object(["error": .string("unreadable \(pyRepr(sel))"), "keys": try keys()])
        }
        if let decodeError = pyUtf8DecodeError(data) {
            throw PyError("UnicodeDecodeError", decodeError)
        }
        let body = pyUniversalNewlines(String(decoding: data, as: UTF8.self))
        var view: OrderedDictionary<String, JSON> = [
            "key": .string(safe),
            "desc": .string(readDesc(body)),
            "tokens": .integer(Int64(estTokens(body))),
            "body": .string(body),
        ]
        // Children come from `docs/<safe>` (unresolved, same rules) — NEVER
        // from a pre-walked index tree — keyed with the RAW `safe` prefix.
        let subPath = docsDir.appendingPathComponent(safe).path
        var isDir: ObjCBool = false
        if fm.fileExists(atPath: subPath, isDirectory: &isDir), isDir.boolValue {
            let children = try RootDocs.walk(URL(fileURLWithPath: subPath), prefix: safe + "/")
            if !children.isEmpty { view["children"] = readmeIndex(children) }
        }
        return .object(view)
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
        timeout: Double?, taskId: TaskId, binary: Data?
    ) async -> (JSON, Status, Data?) {
        guard let secs = timeout else {
            let (r, body) = await runDispatch(
                target: target, payload: payload, verb: verb, taskOut: taskOut,
                taskId: taskId, binary: binary)
            let (reply, status) = classify(r)
            return (reply, status, body)
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
        // The BLOB rides the timed branch too. It did not until 2026-09-03:
        // routing stamps the layer's deadline on EVERY item, so every model-issued
        // `send_binary` took this branch, lost its bytes, and landed on the TEXT
        // channel — `file_bridge` answered "bytes must be raw bytes on the binary
        // channel" for a call that was correct (e2e `E10_dialog:swift`).
        let (r, body) = await runDispatch(
            target: target, payload: payload, verb: verb, taskOut: taskOut, taskId: taskId,
            binary: binary)
        let fired = killer.isCancelled == false && r["cancelled"].asBool == true
        killer.cancel()
        if fired {
            // Python `_agent.py`: `f"send: {d.target.id} {d.payload['type']!r} "
            // f"exceeded its {d.timeout}s timeout; task cancelled"`, carrying
            // `task_id=d.pid or "-"` — always the real pid here (a dispatch
            // that reached this race already minted one).
            return (
                Refusal.make(
                    Refusal.timeout,
                    "send: \(target.id.value) \(pyRepr(verb)) exceeded its "
                        + "\(pyFloatRepr(secs))s timeout; task cancelled",
                    extra: [("task_id", .integer(taskId.value))]),
                .timedOut,
                nil
            )
        }
        let (reply, status) = classify(r)
        return (reply, status, body)
    }

    /// The kernel's OWN dispatch of one payload — python `_run_dispatch`, kept
    /// for the distilled roster, which must reflect each node WITHOUT the
    /// reflect flags (no recursion into rosters).
    ///
    /// It is a call like any other: a minted id, a call row (`internal: true`
    /// tells a reader it was the kernel's own read, not a caller's send), a
    /// task-table row, the watcher fanout and the state events. It opens NO
    /// ledger execution, so nothing stamps or settles the row.
    func internalDispatch(_ target: Agent, _ payload: JSON) async -> (JSON, Status) {
        let pid = rows.mintId()
        var row: Row = [
            "id": .integer(pid), "kind": .string("call"), "verb": payload["type"],
        ]
        row["parent"] = KernelTaskLocals.currentTask.map { JSON.integer($0.value) } ?? .null
        row["origin"] = KernelTaskLocals.currentSender.map { JSON.string($0.value) } ?? .null
        row["internal"] = .bool(true)
        _ = try? rows.put(target.id, row)
        let (reply, _) = await runDispatch(
            target: target, payload: payload, verb: payload["type"].asString ?? "",
            taskOut: nil, taskId: TaskId(pid), binary: nil)
        return classify(reply)
    }

    /// A reply's terminal state. FAILURE IS A STATUS: a handler that RAN and
    /// said no — a missing path, a patch that did not match, a refusal it
    /// composed itself — settles `errored`. A THROW (`raised`) is `failed`; a
    /// kill is `cancelled`. The reply is kept verbatim either way; what changes
    /// is that a reader learns the outcome from `status` instead of parsing
    /// `result`.
    private func classify(_ r: JSON) -> (JSON, Status) {
        if r["cancelled"].asBool == true { return (r, .cancelled) }
        if r["raised"].asBool == true { return (r, .failed) }
        if errorShaped(r) { return (r, .errored) }
        return (r, .completed)
    }

    /// Whether a handler's reply is a REFUSAL — an object carrying a truthy
    /// `error`. The same test every consumer used to make for itself off the
    /// result body.
    ///
    /// PYTHON TRUTHINESS, not key presence: `ERRORED if reply.get("error") else
    /// COMPLETED`, so an `error` of `0`, `0.0`, `[]` or `{}` settles COMPLETED
    /// exactly as `""` and `false` already did. Swift settled every one of them
    /// `errored` because it read anything but a bool/string as truthy.
    private func errorShaped(_ r: JSON) -> Bool {
        pyTruthy(r["error"])
    }

    /// `binary` has NO DEFAULT, deliberately: it defaulted to `nil` until
    /// 2026-09-03 and the timed branch of `runDispatchOutcome` simply forgot to
    /// pass it, which the compiler accepted. Every caller now states the channel.
    private func runDispatch(
        target: Agent, payload: JSON, verb: String, taskOut: TaskIdBox?,
        taskId: TaskId?, binary: Data?
    ) async -> (JSON, Data?) {
        // (A killed sender never reaches here: `launch` settles it `cancelled`
        // before an id is minted.)

        // Register BEFORE spawning: the row (and its stop handle) must exist the
        // instant the handler can run, or a nested `send` could claim a parent
        // that isn't in the table yet.
        // The pid was minted in the SYNCHRONOUS launch phase and is already the
        // ledger key for an unnamed execution — minting a second one here would
        // split the two apart, so the row and the reply would name different
        // things and a pid could not re-fetch its own outcome.
        let taskId = taskId ?? TaskId(rows.mintId())
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

        // BEFORE THE HANDLER, once per attempt — python's `_dispatch` bumps
        // `_in_flight`, fans out, and only then resolves the verb. Here, too, is
        // the last point at which `currentSender` is still the CALLER: the task
        // below re-points it at the target, so a fanout inside it would attribute
        // every event to its own recipient. The `defer` publishes the matching
        // `drain` on every exit path, timeout included.
        target.inFlightEnter()
        defer {
            target.inFlightLeave()
            publishState(Kernel.drainEvent(target.id, target.inFlight))
        }
        fanout(
            target, "send",
            binary.map { Kernel.binaryItem(payload, $0) } ?? .text(payload))

        let box = DispatchBox()
        let bodyBox = BodyBox()
        let work = Task {
            let reply = await KernelTaskLocals.$currentOrigin.withValue(origin) {
                await KernelTaskLocals.$currentSender.withValue(target.id) {
                await KernelTaskLocals.$currentTask.withValue(taskId) {
                await KernelTaskLocals.$ownTask.withValue(taskId) {
                    await KernelTaskLocals.$currentKilled.withValue(handle.killed) {
                            // A blob rides the BINARY channel under this same
                            // row/ledger/timeout, and THE REPLY BODY COMES BACK
                            // WITH IT (C37): the batch is the carrier for the
                            // `bytes` value python's reply dict holds natively.
                            // Dropping it here forced every byte-moving caller
                            // onto `sendWithBinary`, which minted no task row,
                            // no ledger id and no timeout.
                            await {
                                if let blob = binary {
                                    let (r, b) = await self.dispatchBinary(
                                        target: target, header: payload, blob: blob)
                                    bodyBox.value = b
                                    return r ?? .null
                                }
                                return await self.dispatch(target: target, payload: payload)
                            }()
                        }
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

        // `nil` = the task was stopped. Cancellation is a REPLY, never a throw
        // — and a stopped dispatch carries no chunk.
        guard let outcome else { return (.object(["cancelled": .bool(true)]), nil) }
        return (outcome, bodyBox.value)
    }

    private func dispatch(target: Agent, payload: JSON) async -> JSON {
        let verb = payload["type"].asString ?? ""

        if Kernel.isSystemVerb(verb) {
            do {
                return try await handleSystemVerb(target: target, verb: verb, payload: payload)
            } catch {
                // A kernel verb that RAISED settles like any other raised
                // handler: python's one `except` wraps these too, so a digest
                // over an unsized `refs` fails the call instead of quietly
                // answering a page python never produces.
                return .object(["error": .string(error.description), "raised": .bool(true)])
            }
        }

        // THE TREE ROOT ANSWERS `reflect` WITH THE SUBSTRATE IDENTITY even when
        // it carries a handler_module (the `kernel_state` root loader): its
        // persistence verbs (load_tree / persist_record / boot) still route to
        // the bundle below, but `reflect <root>` is the kernel's own identity +
        // tree, matching the `kernel` alias. Python `_agent.py`: `if verb ==
        // "reflect" and self.parent is None`.
        if verb == "reflect", target.parentId == nil {
            return reflectIdentity(target)
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
                // CODED, not a bare error. A reason-less `{error}` is decorated
                // by `applyReflectFlags` into something that reads like a
                // healthy reflect (kernel_verbs, tree, more) with a stray error
                // field — the caller cannot tell a refusal from an answer
                // (measured 2026-09-02).
                return noHandlerModuleRefusal(target.id.value, verb)
            }
        }

        guard let bundle = bundles.get(hm) else {
            // NOT a create-time refusal: this kernel legitimately holds records
            // whose bundle belongs to ANOTHER runtime (the browser's `*.ts`
            // agents persist here), or is installed later. The call simply
            // cannot be answered HERE, and says so with a code.
            return notInstalledRefusal(target.id.value, hm, verb)
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
        case "create_agent", "delete_agent", "update_agent", "list_agents":
            return true
        // NAMED and unnamed executions and what they became. Terminal
        // non-completed rows ARE the dead-letter queue.
        case "ledger":
            return true
        // NOT `persist_blob` / `load_blob`: the sidecar byte channel is the
        // `kernel_state` LOADER's own verb pair (python `tools.py`'s VERBS), not
        // a substrate verb. They pass root ingress like any bundle verb, lose the
        // drain exemption, and a non-root target gets its own bundle's unknown
        // type instead of a kernel `not_here`.
        // The TASK TABLE — work in flight, universal. Baked into the substrate,
        // in no bundle's verb map: cancellation is a property of the one
        // primitive (`send`), so EVERY agent is cancelable and a caller never
        // needs to know which bundle it faces. Plain verbs — an AGENT can call
        // them on another agent's work.
        case "ps", "cancel":
            return true
        // THE ROW STORE — `messages` is the digest read every agent may be
        // granted; `rows_read`/`rows_write` are hydration, PRIVILEGED.
        case "messages", "rows_read", "rows_write":
            return true
        // Root-only control verbs — gated inside their handler to the tree
        // root. `shutdown_agents` stops the agents; `shutdown_kernel` stops
        // the whole kernel process gracefully.
        case "shutdown_agents", "shutdown_kernel":
            return true
        default:
            return false
        }
    }
}

// MARK: - Telemetry helper

/// One-line, bytes-shrunk summary for state events — python's
/// `_summarize_payload`.
///
/// `json.dumps(shrunk, default=str, ensure_ascii=False)`, so `, ` / `: `
/// separators and raw non-ASCII, not swift's compact `serialize()`. A bytes
/// value becomes `<bytes:N>` — the swift carrier keeps the bytes beside the
/// payload (see `InboxItem`), so the marker is written back at the body's own
/// path.
///
/// NOT TRUNCATED. This used to cut at 160 chars + "...". A summary that silently
/// drops the end of a payload lies about what was sent — the reader cannot tell a
/// short payload from a cut one, and the dropped part is exactly what you opened
/// telemetry to find. Eliding is a RENDER decision, where it is visible.
func summarize(_ payload: JSON, body: InboxBody? = nil) -> String {
    guard let body = body else { return pyJsonDumps(payload, ensureAscii: false) }
    var shrunk = payload
    setAtPath(&shrunk, body.path, .string("<bytes:\(body.data.count)>"))
    return pyJsonDumps(shrunk, ensureAscii: false)
}

/// Write `value` at a dotted `path` (python's `set_path`: object keys, list
/// indices). Silently no-ops on a path that does not resolve — the only caller
/// is the summary above, and a telemetry line is not worth a crash.
func setAtPath(_ obj: inout JSON, _ path: String, _ value: JSON) {
    let parts = path.split(separator: ".").map(String.init)
    guard let last = parts.last else { return }
    if parts.count == 1 {
        switch obj {
        case .object(var o):
            o[last] = value
            obj = .object(o)
        case .array(var a):
            guard let i = Int(last), a.indices.contains(i) else { return }
            a[i] = value
            obj = .array(a)
        default: return
        }
        return
    }
    let head = parts[0]
    let rest = parts.dropFirst().joined(separator: ".")
    switch obj {
    case .object(var o):
        guard var child = o[head] else { return }
        setAtPath(&child, rest, value)
        o[head] = child
        obj = .object(o)
    case .array(var a):
        guard let i = Int(head), a.indices.contains(i) else { return }
        var child = a[i]
        setAtPath(&child, rest, value)
        a[i] = child
        obj = .array(a)
    default: return
    }
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
    func joinDenial(origin: AgentId?, payload: JSON?, target: Agent) -> JSON? {
        guard let joiner = origin else { return nil }
        // THE EXECUTION'S OWN PAYLOAD, not a synthesized `{type:<verb>}`. The
        // gate reads arguments as well as the verb — `create_agent`'s
        // `parent_id` IS the placement it authorizes — so a joiner rebuilt from
        // the verb alone was asked a different question than the call it is
        // about to be handed. A rehydrated row has no payload: python's
        // `(entry.get("payload") or {}).get("type")` is then `None` and the gate
        // runs on an empty call. Python `_agent.py:786-793`.
        let orig = payload ?? .object([:])
        return gate(
            sender: joiner, verb: orig["type"].asString ?? "", target: target, payload: orig)
    }

    /// Resolve an id the way `send` does — the `kernel` alias or a literal id.
    /// `nil` when it names no live agent.
    func resolveAgent(_ id: JSON) -> Agent? {
        guard let s = id.asString, !s.isEmpty else { return nil }
        return s == "kernel" ? root : agent(AgentId(s))
    }

    /// Two-sided gate on the PLACEMENT TARGET of `create_agent`. Returns a
    /// refusal, or `nil` when the sender may create under `parent`.
    ///
    /// Placing inside your OWN subtree is baseline (no grant). Anywhere else
    /// needs egress on the parent (explicitly named — `create_agent` is
    /// privileged, so a wildcard never covers it) AND the parent's own
    /// `callable_by` accepting the sender.
    func gatePlacement(
        sender: AgentId, sCan: JSON, sParent: String?, sKids: Set<String>,
        loaderId: String?, parent: Agent
    ) -> JSON? {
        var node: Agent? = parent
        while let n = node {
            if n.id == sender { return nil }  // your own subtree — baseline
            node = n.parentId.flatMap { agent($0) }
        }
        let d = authorizeCall(
            grant: sCan, verb: "create_agent", otherId: parent.id.value,
            holderId: sender.value, parentId: sParent, childIds: sKids, loaderId: loaderId,
            enforcePrivileged: true)
        if !d.allowed {
            return .object([
                "error": .string("\(sender.value) may not create under \(parent.id.value)"),
                "reason": .string("unauthorized"), "hint": .string(d.hint),
                "side": .string("egress"),
            ])
        }
        let tCall = parent.metaValue(forKey: "callable_by") ?? .null
        let di = authorizeCall(
            grant: tCall, verb: "create_agent", otherId: sender.value,
            holderId: parent.id.value, parentId: parent.parentId?.value,
            childIds: Set(parent.childIds().map { $0.value }), loaderId: loaderId,
            enforcePrivileged: false)
        if !di.allowed {
            return .object([
                "error": .string(
                    "\(parent.id.value) does not accept a child from \(sender.value)"),
                "reason": .string("unauthorized"), "hint": .string(di.hint),
                "side": .string("ingress"),
            ])
        }
        return nil
    }

    func gate(sender: AgentId, verb: String, target: Agent, payload: JSON) -> JSON? {
        let loaderId = root?.id.value
        let senderAgent = agent(sender)
        let sCan = senderAgent?.metaValue(forKey: "can_call") ?? .null
        let sParent = senderAgent?.parentId?.value
        let sKids = Set(senderAgent?.childIds().map { $0.value } ?? [])

        // PLACEMENT IS THE OPERAND. `create_agent` is answered by the root, but
        // the agent it ACTS UPON is the parent named in `parent_id` — so that is
        // what the gate asks about, on both sides. Gating the root instead made
        // one grant (`<root>:[create_agent]`, the ordinary "may author tools"
        // wiring) a key to the whole tree: the holder could plant a child under
        // ANY agent and inherit `$parent:*` reach into it (measured 2026-09-02).
        //
        // FORKING YOUR OWN SUBTREE IS BASELINE — placing under yourself or a
        // descendant needs no grant at all, which is what makes self-extension
        // work with zero wiring at any depth.
        if verb == "create_agent", let parent = resolveAgent(payload["parent_id"]) {
            if let denial = gatePlacement(
                sender: sender, sCan: sCan, sParent: sParent, sKids: sKids,
                loaderId: loaderId, parent: parent)
            {
                return denial
            }
            return checkConferral(
                sender: sender, sCan: sCan, sParent: sParent, sKids: sKids,
                loaderId: loaderId, payload: payload)
        }
        // An unknown parent falls through to the ordinary path; the verb itself
        // answers `unknown_agent`.

        // Egress — may the SENDER call the target?
        let d = authorizeCall(
            grant: sCan, verb: verb, otherId: target.id.value,
            holderId: sender.value, parentId: sParent, childIds: sKids, loaderId: loaderId,
            enforcePrivileged: true)
        if !d.allowed {
            return .object([
                "error": .string(
                    "\(sender.value) may not call \(pyRepr(verb)) on \(target.id.value)"),
                "reason": .string("unauthorized"), "hint": .string(d.hint),
                "side": .string("egress"),
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
                    "\(target.id.value) does not accept \(pyRepr(verb)) from \(sender.value)"),
                "reason": .string("unauthorized"), "hint": .string(di.hint),
                "side": .string("ingress"),
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
                let probeKey: String
                switch key {
                // Relative to the HOLDER — no lateral reach.
                case "$self", "$parent", "$children":
                    continue
                // NOT relative: `$loader` resolves to the ROOT for whoever holds
                // it, so conferring it hands over real reach on the kernel's
                // control surface (ps, cancel, ledger, shutdown_kernel).
                // Subset-probe it like any lateral edge — skipping it let a
                // grandchild grant its own child root reach it did not hold
                // (measured 2026-09-02).
                case "$loader":
                    guard let r = loaderId else { continue }
                    probeKey = r
                default:
                    if key.hasPrefix("$") {
                        return .object([
                            "error": .string(
                                "\(sender.value) cannot confer \(pyRepr(key)): not a structural "
                                    + "token — $self, $parent, $children and $loader are "
                                    + "the only ones"),
                            "reason": .string("bad_arg"), "arg": .string("can_call"),
                            "side": .string("delegation"), "hint": .string(fromAbove),
                        ])
                    }
                    probeKey = key
                }
                // Conferring "*" (or a list with it) requires the granter to hold
                // "*" on that target; a concrete list requires each verb. C21:
                // python `probe = need if (need and "*" not in need) else ["*"]`
                // — an EMPTY list is falsy, so it probes `*` too, same as "*"
                // itself; this used to leave `probe` empty for `[]`, so the
                // loop below never ran and an empty verb list passed with no
                // check at all.
                var probe: [String] = ["*"]
                if case .array(let vs) = verbs, !vs.isEmpty,
                    !vs.contains(where: { $0.asString == "*" })
                {
                    probe = vs.compactMap { $0.asString }
                }
                for v in probe {
                    let ok = authorizeCall(
                        grant: sCan, verb: v, otherId: probeKey, holderId: sender.value,
                        parentId: sParent, childIds: sKids, loaderId: loaderId
                    ).allowed
                    if !ok {
                        if key == "$loader" {
                            return .object([
                                "error": .string(
                                    "\(sender.value) cannot delegate \(pyRepr(v)) on $loader: it "
                                        + "resolves to \(probeKey), which you do not hold — "
                                        + "$loader is real reach on the kernel's control "
                                        + "surface, not a structural token"),
                                "reason": .string("unauthorized"),
                                "side": .string("delegation"), "hint": .string(fromAbove),
                            ])
                        }
                        // Name the TRUE reason. A target that is not an agent at all
                        // is almost always one created in the SAME batch — items
                        // launch concurrently, so it is not there yet when this gate
                        // runs. "you don't hold it" sent a builder into a 382-second
                        // deliberation (measured 2026-09-02).
                        if agent(AgentId(key)) == nil {
                            return .object([
                                "error": .string(
                                    "\(sender.value) cannot delegate \(pyRepr(v)) on \(key): no such "
                                        + "agent — calls of one batch run concurrently, so an "
                                        + "agent created in this batch does not exist yet; "
                                        + "create in one batch, grant in the next"),
                                "reason": .string("unauthorized"), "side": .string("delegation"),
                                "missing": .string(key),
                                "hint": .string(fromAbove),
                            ])
                        }
                        // The target exists but the granter does not hold it. Say WHY
                        // in one line — the usual cause is a specialist created under
                        // the root instead of under the granter, and "you don't hold
                        // it" hid that for twenty passes (2026-09-02).
                        let pid = agent(AgentId(key))?.parentId?.value
                        return .object([
                            "error": .string(
                                "\(sender.value) cannot delegate \(pyRepr(v)) on \(key): \(key) is not "
                                    + "your child (its parent is \(pid ?? "None")) and not in your "
                                    + "can_call — you may only confer can_call you already hold; "
                                    + "create the agents you will wire as YOUR children"),
                            "reason": .string("unauthorized"), "side": .string("delegation"),
                            "parent": pid.map { .string($0) } ?? .null,
                            "hint": .string(fromAbove),
                        ])
                    }
                }
            }
        }
        return nil
    }
}

/// One roster node, or the RAISED reply of a node whose own reflect blew up —
/// python's exception propagating out of `_roster` fails the whole reflect.
enum RosterResult {
    case node(JSON)
    case raised(JSON)
}
