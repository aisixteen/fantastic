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
    /// The SAME refusal texts (+ `verb`) as the text channel — python has
    /// no separate binary dispatch at all, a binary frame runs through the
    /// same `_dispatch` resolution the text channel does.
    func dispatchBinary(target: Agent, header: JSON, body: Blob?) async -> (
        JSON?, Blob?, raised: Bool
    ) {
        let verb = header["type"].asString ?? ""
        // THE BYTES DO NOT CHANGE WHO ANSWERS. In python a payload carrying
        // `bytes` is an ordinary payload: `_dispatch` answers the kernel verbs,
        // the root's `reflect` (the substrate identity) and a bare agent exactly
        // as it does without them. Only a bundle's own verb reads the body.
        // (An octet POST of `reflect` to the root answers the kernel's reflect,
        // never the loader bundle's.)
        if Kernel.isSystemVerb(verb) || target.handlerModule == nil
            || (verb == "reflect" && target.parentId == nil)
        {
            let (r, raised) = await dispatch(target: target, payload: header)
            return (r, nil, raised)
        }
        guard let hm = target.handlerModule else {
            return (noHandlerModuleRefusal(target.id.value, verb), nil, false)
        }
        guard let bundle = bundles.get(hm) else {
            return (notInstalledRefusal(target.id.value, hm, verb), nil, false)
        }
        do {
            let (r, b) = try await bundle.handleBinary(
                agentId: target.id, header: header, body: body, kernel: self)
            return (r, b, false)
        } catch {
            // An exception is a RESULT, exactly as the text channel's catch
            // treats it — reported as a raise beside the body.
            return (Kernel.raisedReply("\(error)"), nil, true)
        }
    }

    /// `agent <id!r> has no handler_module; cannot answer verb <verb!r>` —
    /// SHARED by text and binary dispatch. Python `_agent.py:2069-2071`.
    private func noHandlerModuleRefusal(_ id: String, _ verb: String) -> JSON {
        Refusal.make(
            Refusal.unhandled,
            "agent \(pyRepr(id)) has no handler_module; cannot answer verb \(pyRepr(verb))",
            extra: [("verb", .string(verb))])
    }

    /// `agent <id!r>: handler_module <hm!r> cannot be imported in this
    /// runtime: <ExceptionClass>: <message>` — SHARED by text and binary
    /// dispatch. Python's PREFIX, verbatim; the TAIL is not stable
    /// across python's own runs ("No module named 'yaml_state'" or "No module
    /// named 'yaml_state.tools'"), so this names the closest static equivalent of the dynamic
    /// import failure rather than pinning python's exact wording — the same
    /// residual rust picked, for cross-runtime consistency.
    private func notInstalledRefusal(_ id: String, _ hm: String, _ verb: String) -> JSON {
        Refusal.make(
            Refusal.unhandled,
            "agent \(pyRepr(id)): handler_module \(pyRepr(hm)) cannot be imported in this "
                + "runtime: ModuleNotFoundError: No module named \(pyRepr(hm))",
            extra: [("verb", .string(verb))])
    }

    /// THE KERNEL RUNNING AN AGENT'S HANDLER ITSELF — its own bookkeeping: the
    /// cold boot, booting the child `create_agent` just made, the `shutdown`
    /// verb at a graceful stop, the flush to its memory store, persistence
    /// through the loader. A PLAIN FUNCTION CALL into the agent's handler, not a
    /// send: no gate, no call row, no task row, no fanout. The handler runs AS
    /// ITSELF — identity comes only from the kernel running a handler, so the
    /// sends it makes and the tasks it spawns are that agent's — with no caller
    /// (`currentOrigin` nil: the kernel asked). A throw settles `failed`, a
    /// refusal `errored`.
    func runHandler(_ target: Agent, _ payload: JSON) async -> (JSON, Status) {
        let (reply, raised) = await asHandler(target) {
            await self.dispatch(target: target, payload: payload)
        }
        return (reply, Kernel.handlerStatus(reply, raised: raised))
    }

    /// ONE KERNEL BOOKKEEPING CALL — python `Kernel.bookkeeping`: the kernel runs
    /// `agent`'s handler DIRECTLY (`runHandler`) — a boot, a loader hydrating the
    /// roster it was told to build (the root's kernel verbs), a store read or
    /// write. A plain call, not a send — no gate, no task row, no call row — so
    /// it is the same whoever's dispatch asked for it. The handler runs as
    /// `agent`, with no origin: the kernel asked. A raise is `failed`, a refusal
    /// `errored`, anything else `completed`.
    public func bookkeeping(_ agent: Agent, _ payload: JSON) async -> (result: JSON, status: Status) {
        // no caller's marks reach a call the kernel makes itself
        let (reply, status) = await KernelTaskLocals.$secret.withValue([]) {
            await runHandler(agent, payload)
        }
        return (reply, status)
    }

    /// `runHandler` over the BINARY channel: `body` the request's chunk at its
    /// payload path (a `write_stream`), or `nil` to have the reply's chunk come
    /// back raw (a `read_stream`).
    func runHandlerBinary(_ target: Agent, _ header: JSON, _ body: Blob?) async
        -> (JSON, Blob?, Status)
    {
        var head = header
        if let b = body { try? pyDelPath(&head, parts: b.parts) }
        let handlerHeader = head
        let (reply, chunk, raised) = await asHandler(target) {
            await self.dispatchBinary(target: target, header: handlerHeader, body: body)
        }
        let r = reply ?? .null
        return (r, chunk, Kernel.handlerStatus(r, raised: raised))
    }

    /// THE KERNEL RUNNING AN AGENT'S OWN CODE — python `Agent._as_self()`: the
    /// enclosed sends (and the tasks they spawn) are `id`'s, exactly as inside its
    /// dispatch. Internal: the kernel's hooks (`onDelete`, `onCancel`, the
    /// shutdown hooks, `runHandler`) and the test driver use it; no bundle can.
    func asSelf<T>(_ id: AgentId?, _ body: () async throws -> T) async rethrows -> T {
        try await KernelTaskLocals.$sender.withValue(id) { try await body() }
    }

    /// `asSelf` for a synchronous body (`watch`, `emitItem`).
    func asSelfNow<T>(_ id: AgentId?, _ body: () throws -> T) rethrows -> T {
        try KernelTaskLocals.$sender.withValue(id) { try body() }
    }

    /// The scope a handler the kernel runs directly executes in: the agent is
    /// the sender, the kernel the (absent) caller, no batch turnstile, no
    /// inherited kill — a bookkeeping write a cancel must not swallow.
    private func asHandler<T: Sendable>(_ target: Agent, _ body: @Sendable () async -> T)
        async -> T
    {
        await asSelf(target.id) {
            await KernelTaskLocals.$origin.withValue(nil) {
                await KernelTaskLocals.$currentKilled.withValue(nil) {
                    await KernelTaskLocals.$onFanout.withValue(nil) {
                        await KernelTaskLocals.$onBatchLaunched.withValue(nil) {
                            await body()
                        }
                    }
                }
            }
        }
    }

    /// A handler run's status, decided by how it ended: a throw is `failed`, a
    /// truthy `error` `errored`, anything else `completed`.
    static func handlerStatus(_ reply: JSON, raised: Bool) -> Status {
        if raised { return .failed }
        return pyTruthy(reply["error"]) ? .errored : .completed
    }

    /// Fire an event into `targetId`'s inbox without dispatching. Publishes a
    /// `kind:"emit"` state event and mirrors to watchers — the same fanout a
    /// dispatch runs, minus the in-flight bump.
    ///
    /// AN EMIT TO A NON-AGENT ID IS A NO-OP. The target agent is resolved FIRST:
    /// no agent, no inbox put, no state event. Delivering into any
    /// registered inbox and publishing regardless would let a synthetic WS client
    /// id (`_ws_<hex>`) produce telemetry for a sprite that does not exist —
    /// python drops it silently (`_proxy.py:196-200`).
    public func emit(_ targetId: AgentId, _ payload: JSON) async {
        emitItem(targetId, .text(payload))
    }

    /// QUIET `emit`: the inbox and its watchers, NOTHING on
    /// the state stream — a streaming part's deltas, which would flood every
    /// subscriber at token rate. Gated and routed exactly as `emit`.
    public func emitQuiet(_ targetId: AgentId, _ payload: JSON) async {
        emitItem(targetId, .text(payload), publish: false)
    }

    /// `emit` for a payload carrying raw bytes — a `read_stream` chunk a bridge
    /// re-emits from a remote watch, which must reach a local watcher WHOLE.
    /// Same no-op rule for a non-agent id.
    ///
    /// GATED LIKE A SEND when the emitter is another agent: the
    /// emitter's `can_call` and the target's `callable_by` must both
    /// grant verb `emit`. The baseline keeps emits to self; any other edge is an
    /// explicit grant. A denied emit is dropped; an emit has no reply.
    ///
    /// SYNCHRONOUS: an emit is a fanout and nothing else, so a leg can deliver
    /// it in the order its frames arrived — python awaits `kernel.emit` inline
    /// before it reads the next frame.
    ///
    /// `<guid>|<agent>` of a CONNECTED PEER reaches EVERY live route of that
    /// guid (each tab of one identity), gated on the egress side like a send to
    /// it (`emitPeer`).
    public func emitItem(_ targetId: AgentId, _ item: InboxItem, publish: Bool = true) {
        guard
            let target = agent(targetId)
                ?? qualifiedTarget(targetId.value).flatMap({ $0.isRemote ? $0 : nil })
        else { return }
        let orig = KernelTaskLocals.currentSender
        if let orig, orig != target.id,
            gate(sender: orig, verb: "emit", target: target) != nil
        {
            return
        }
        if let r = target.remote {
            emitPeer(r, item: item)
            return
        }
        fanout(target, "emit", item, publish: publish)
    }

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
        // A REFUSAL IS NOT A REFLECT. Decorating one with a tree
        // would produce a reply that reads like a healthy reflect with a stray error
        // field, and a caller could not tell the two apart.
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
            // The kernels this one is CONNECTED to — each agent there is
            // reachable as `<guid>|<agent id>` (reflect `<guid>|kernel` for its
            // tree).
            obj["peers"] = peerRows()
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
                    // A HOOK THAT RAISES FAILS THE REFLECT (python `_agent.py`
                    // calls the duck-typed hook bare). Swallowed, it would drop
                    // `persistence` and `guid` — what an operator reads to learn
                    // whether state persists — with nothing to say so.
                    reply = .object(obj)
                    return .object(["error": .string(error.description), "raised": .bool(true)])
                }
            }
        }
        // DECIDED BY KEY PRESENCE (python `payload.get("tree", "all")`): absent
        // is `all`; a present `all`/`ids` acts; any OTHER present value — null,
        // a bool, a number, a list, `none` — omits the tree. Reading it as
        // `asString ?? "all"` made `tree:null` and `tree:true` insert the whole
        // tree, which is the one answer the caller did not ask for.
        let treeArg = payload.asObject?["tree"]
        let walks: Bool
        switch treeArg {
        case .none, .some(.string("all")), .some(.string("ids")): walks = true
        default: walks = false
        }
        // SUBTREE FILTERS: `show_subtrees` answers with only
        // those branches, `ignore_subtrees` prunes each named branch to a stub
        // `{id, pruned: n}`; together, show first and ignore inside it. Python
        // `kernel/_docs.subtree_filter`.
        var show: [Agent] = []
        var ignore: Set<String> = []
        if walks {
            switch subtreeFilter(payload, target) {
            case .failure(let refusal):
                reply = refusal.reply
                return nil
            case .success(let f):
                show = f.show
                ignore = f.ignore
            }
        }
        let roots = show.isEmpty ? [target] : show
        switch treeArg {
        case .none, .some(.string("all")):
            // DISTILLED IS THE GATED ROSTER: `{id, sentence?, description?,
            // mode?, children}`, each sentence ASKED FROM THE AGENT ITSELF.
            // `detail:full` keeps the wiring node.
            var nodes: [JSON] = []
            for r in roots {
                if ignore.contains(r.id.value) {
                    nodes.append(prunedStub(r))
                } else if payload["detail"].asString == "full" {
                    nodes.append(treeNode(r, ignore: ignore))
                } else {
                    switch await roster(r, ignore: ignore) {
                    case .node(let node): nodes.append(node)
                    case .raised(let raised): return raised
                    }
                }
            }
            obj["tree"] = show.isEmpty ? nodes[0] : .array(nodes)
        case .some(.string("ids")):
            // PAGED — the cheap id tier is the one most likely to be walked by a
            // caller with a small budget, so it is also the one that most needs
            // a bound. Same cursor law as `list_agents`/`ps`; the cursor is
            // `next`, and it is present ONLY when a further page exists. A
            // pruned branch stands as its stub; the cursor is its id.
            let flat = roots.flatMap { descendantEntries($0, ignore: ignore) }
            let (ids, next) = page(flat, payload) { $0.asString ?? ($0["id"].asString ?? "") }
            obj["tree"] = .array(ids)
            if let next { obj["next"] = .string(next) }
        default: break
        }
        // The catalog pages with the SAME `limit`/`after` as `tree=ids`, and
        // names its cursor `bundles_next` only when a further page follows.
        switch payload.asObject?["bundles"] {
        case .some(.string("all")):
            var entries: [JSON] = availableBundles().map {
                var entry: OrderedDictionary<String, JSON> = [
                    "name": .string($0.name),
                    "handler_module": .string($0.handlerModule),
                ]
                // The kind's record schema, where the bundle publishes one —
                // on the full catalog only; `bundles=ids` stays names.
                if let sch = bundles.get($0.handlerModule)?.recordSchema {
                    entry["schema"] = sch
                }
                return .object(entry)
            }
            // FOLDER BUNDLES of this kernel's root, beside the installed ones — a
            // refused folder too, with its `error`.
            entries += bundles.folderRows()
            entries.sort { ($0["name"].asString ?? "") < ($1["name"].asString ?? "") }
            let (cat, next) = page(entries, payload) { $0["name"].asString ?? "" }
            obj["bundles"] = .array(cat)
            if let next { obj["bundles_next"] = .string(next) }
        case .some(.string("ids")):
            let installed = availableBundles().map { $0.name }
            let folders = bundles.folderRows().compactMap { $0["name"].asString }
            let listed = (installed + folders).sorted()
            let (names, next) = page(listed, payload) { $0 }
            obj["bundles"] = .array(names.map { .string($0) })
            if let next { obj["bundles_next"] = .string(next) }
        default: break  // "none" → omit
        }
        // Polymorphic `readme`: `true` → the SHORT orientation; `"index"` → the
        // topic tree; `"<key>"` → one topic's body. One agent at a time.
        //
        // A non-UTF-8 doc is NOT caught (python catches only `OSError`; a
        // `UnicodeDecodeError` propagates and fails the WHOLE reflect).
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
    /// NOT gated per node: a reflect that passes the gate returns the tree
    /// whole.
    ///
    /// `.failure` carries a node's RAISED reflect: python's exception
    /// propagates out of `_roster` and fails the whole reflect.
    func roster(_ a: Agent, ignore: Set<String> = []) async -> RosterResult {
        var raw: JSON
        if a.handlerModule != nil {
            let (reply, status) = await internalDispatch(a, .object(["type": .string("reflect")]))
            if status == .failed { return .raised(reply) }
            raw = reply
        } else {
            raw = reflectIdentity(a)
        }
        var node: OrderedDictionary<String, JSON> = ["id": .string(a.id.value)]
        if pyTruthy(raw["sentence"]) { node["sentence"] = raw["sentence"] }
        // Operator-set purpose. More specific than the bundle sentence when
        // present ("the prod deploy key" vs "a yaml store").
        if let d = a.descriptionMeta { node["description"] = .string(d) }
        // Purpose comes from the agent's REFLECT, not its record: much of what
        // an agent is FOR lives in its reflect, not as a record field.
        for k in Kernel.purposeKeys {
            let v = raw[k]
            if case .null = v { continue }
            node[k] = v
        }
        var children: [JSON] = []
        for c in a.childIds().compactMap({ self.agent($0) }) {
            if ignore.contains(c.id.value) {
                children.append(prunedStub(c))
                continue
            }
            switch await roster(c, ignore: ignore) {
            case .node(let n): children.append(n)
            case .raised(let raised): return .raised(raised)
            }
        }
        node["children"] = .array(children)
        return .node(.object(node))
    }

    /// The addressed agent's bundle readme (string) or null — served from the
    /// bundle, never from a copy on disk. Python's text mode:
    /// `\r\n`/lone `\r` → `\n`.
    private func readReadme(_ target: Agent) throws(PyError) -> JSON {
        guard let hm = target.handlerModule, let text = bundles.get(hm)?.readme else {
            return .null
        }
        return .string(pyUniversalNewlines(text))
    }

    // ─── keyed / tiered doc tree ───────────────────────────────
    // The manual is a SHORT `readme.md` orientation + a `docs/` topic tree.
    // `reflect readme=index` lists it token-annotated; `reflect readme=<key>`
    // drills one topic. Served from the agent's bundle (`AgentBundle.docs`),
    // never from a copy on disk.

    /// ~tokens for a doc: ceil(chars/4), matching ai_core's estimate (Unicode
    /// scalars = code points, so the advertised cost is stable across runtimes).
    private func estTokens(_ text: String) -> Int {
        (text.unicodeScalars.count + 3) / 4
    }

    /// A topic's one-line descriptor: the leading `> ` line if present, else
    /// the first prose line (skipping headings). Python `str.splitlines()` /
    /// `str.strip()` — NOT a bare `\n` split / ASCII-whitespace trim,
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

    /// The topic TREE for an agent: its bundle's `docs`, nested by key (a `/`
    /// in a key is a tier) — python's `_readme_index` over the package `docs/`.
    private func docsFor(_ target: Agent) throws(PyError) -> [RootDocs.Node] {
        guard let hm = target.handlerModule, let bundle = bundles.get(hm) else { return [] }
        return RootDocs.tree(bundle.docs)
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
    /// Python `_readme_view`: the key is checked LEXICALLY, then against the
    /// keys the index lists.
    private func readmeView(_ target: Agent, _ sel: String) throws(PyError) -> JSON {
        let docs = try docsFor(target)
        if sel == "index" { return .object(["index": readmeIndex(docs)]) }
        let keys = JSON.array(RootDocs.keys(docs).map { .string($0) })
        let safe =
            sel
            .trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            .replacingOccurrences(of: "\\", with: "/")
        let parts = safe.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        if safe.isEmpty || parts.contains(where: { $0 == "" || $0 == "." || $0 == ".." }) {
            return .object(["error": .string("bad key \(pyRepr(sel))"), "keys": keys])
        }
        func find(_ nodes: [RootDocs.Node]) -> RootDocs.Node? {
            for n in nodes {
                if n.key == safe { return n }
                if let hit = find(n.children) { return hit }
            }
            return nil
        }
        guard let hit = find(docs) else {
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
    /// `timed_out` ONLY WHEN THE DEADLINE FIRED AND STOPPED THE TASK — python
    /// reports it only when `asyncio.wait` leaves the task pending and the kill
    /// then ends it. A task stopped by anyone else's `cancel` settles
    /// `cancelled`, and a REPLY that merely says `{cancelled: true}` (the
    /// `cancel` verb's own answer) is a reply like any other. The race is
    /// reported honestly: a handler that finishes during the grace window
    /// COMPLETES — `cancelTask` no-ops on a finished id.
    func runDispatchOutcome(
        target: Agent, payload: JSON, verb: String, taskOut: TaskIdBox?,
        timeout: Deadline?, taskId: TaskId, binary: Blob?, rawReply: Bool,
        marks: [String] = []
    ) async -> (JSON, Status, Blob?) {
        guard let deadline = timeout else {
            let end = await runDispatch(
                target: target, payload: payload, verb: verb, taskOut: taskOut,
                taskId: taskId, binary: binary, rawReply: rawReply, marks: marks)
            let (reply, status) = classify(end)
            return (reply, status, end.body)
        }
        // Race the dispatch against the deadline. The deadline branch marks
        // that IT fired, then kills through the front door.
        let fired = FiredFlag()
        let killer = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(deadline.seconds * 1_000_000_000))
            guard let self, !Task.isCancelled else { return }
            fired.set()
            // Each `onCancel` hook runs as its own agent (`cancelTask`), never as
            // the batch caller.
            _ = await self.cancelTask(taskId)
        }
        // The BLOB rides the timed branch too: routing stamps the layer's deadline
        // on EVERY item, so every model-issued `send_binary` takes this branch.
        let end = await runDispatch(
            target: target, payload: payload, verb: verb, taskOut: taskOut, taskId: taskId,
            binary: binary, rawReply: rawReply, marks: marks)
        killer.cancel()
        if end.stopped && fired.isSet {
            // Python `_agent.py`: `f"send: {d.target.id} {d.payload['type']!r} "
            // f"exceeded its {d.timeout}s timeout; task cancelled"`, carrying
            // `task_id=d.pid or "-"` — always the real pid here (a dispatch
            // that reached this race already minted one).
            return (
                Refusal.make(
                    Refusal.timeout,
                    "send: \(target.id.value) \(pyRepr(verb)) exceeded its "
                        + "\(deadline.text)s timeout; task cancelled",
                    extra: [("task_id", .integer(taskId.value))]),
                .timedOut,
                nil
            )
        }
        let (reply, status) = classify(end)
        return (reply, status, end.body)
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
        let pid = mintId()
        var row: Row = [
            "id": .integer(pid), "kind": .string("call"), "verb": payload["type"],
        ]
        row["parent"] = KernelTaskLocals.currentTask.map { JSON.integer($0.value) } ?? .null
        row["caller"] = KernelTaskLocals.currentSender.map { JSON.string($0.value) } ?? .null
        // The internal variant carries its payload too — key order
        // `id, kind, verb, parent, caller, payload, internal`.
        row["payload"] = payload
        row["internal"] = .bool(true)
        _ = try? rowsPut(target.id, row)
        let end = await runDispatch(
            target: target, payload: payload, verb: payload["type"].asString ?? "",
            taskOut: nil, taskId: TaskId(pid), binary: nil, rawReply: false, marks: [])
        return classify(end)
    }

    /// A dispatch's terminal state. FAILURE IS A STATUS: a handler that RAN and
    /// said no — a missing path, a patch that did not match, a refusal it
    /// composed itself — settles `errored`. A THROW is `failed`; a stopped task
    /// is `cancelled`.
    ///
    /// DECIDED BY HOW THE DISPATCH ENDED, never by reading the reply. Python's
    /// `_settle_once` sets FAILED only in its `except` branch and CANCELLED only
    /// when the task itself was cancelled; a returned dict is ERRORED or
    /// COMPLETED on its `error` alone. Reading `raised`/`cancelled` off the body
    /// settled the `cancel` verb's own `{cancelled: true}` answer `cancelled`
    /// (and dead-lettered it), and a peer's relayed `{error, raised: true}`
    /// `failed`.
    private func classify(_ end: DispatchEnd) -> (JSON, Status) {
        if end.stopped { return (.object(["cancelled": .bool(true)]), .cancelled) }
        if end.raised { return (end.reply, .failed) }
        if errorShaped(end.reply) { return (end.reply, .errored) }
        return (end.reply, .completed)
    }

    /// Whether a handler's reply is a REFUSAL — an object carrying a truthy
    /// `error`. One test, not one per consumer off the result body.
    ///
    /// PYTHON TRUTHINESS, not key presence: `ERRORED if reply.get("error") else
    /// COMPLETED`, so an `error` of `0`, `0.0`, `[]` or `{}` settles COMPLETED
    /// exactly as `""` and `false` do.
    private func errorShaped(_ r: JSON) -> Bool {
        pyTruthy(r["error"])
    }

    /// `binary` has NO DEFAULT, deliberately: with a default, a caller could forget
    /// to pass it and the compiler would accept it. Every caller states the channel:
    /// `binary` (a request body at its payload path) or `rawReply` (the binary
    /// channel with no request body — a raw read) or neither (the text channel).
    private func runDispatch(
        target: Agent, payload: JSON, verb: String, taskOut: TaskIdBox?,
        taskId: TaskId?, binary: Blob?, rawReply: Bool, marks: [String]
    ) async -> DispatchEnd {
        // (A killed sender never reaches here: `launch` settles it `cancelled`
        // before an id is minted.)

        // Register BEFORE spawning: the row (and its stop handle) must exist the
        // instant the handler can run, or a nested `send` could claim a parent
        // that isn't in the table yet.
        // The pid was minted in the SYNCHRONOUS launch phase and is already the
        // ledger key for an unnamed execution — minting a second one here would
        // split the two apart, so the row and the reply would name different
        // things and a pid could not re-fetch its own outcome.
        let taskId = taskId ?? TaskId(mintId())
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
        //
        // A REMOTE TARGET is not an agent of this tree: nobody here can watch it
        // and it has no backlog to report, so neither the in-flight count nor the
        // fanout applies (python's `RemoteTarget` has neither). It keeps
        // everything else — the task row above, the deadline, the settle.
        let local = !target.isRemote
        if local { target.inFlightEnter() }
        defer {
            if local {
                target.inFlightLeave()
                publishState(Kernel.drainEvent(target.id, target.inFlight))
            }
        }
        if local {
            // A REQUEST BODY rides at ITS path — where python's own payload dict
            // holds it — so a watcher receives the body, not a header with a hole
            // where it was. A call that only READS a raw chunk carries none: its
            // payload is a plain dict, a text frame.
            // A MARKED call is mirrored as shown (`Secret`).
            let shown = Secret.shown(payload, marks)
            fanout(
                target, "send",
                binary.map { Kernel.binaryItem(shown, $0) } ?? .text(shown))
        }
        // The BINARY channel whenever bytes travel either way: a request body
        // goes in, or the reply's chunk comes back raw. A request body IS the
        // payload's value at its path (python's `payload[field] = body` /
        // `set_path` overwrites the slot), so the header the handler gets holds
        // nothing there — a caller names the slot only to keep python's key order
        // on the row and the watcher frame. (A payload that did not name the slot
        // leaves nothing to take.)
        var header = payload
        if let b = binary { try? pyDelPath(&header, parts: b.parts) }
        let handlerPayload = header
        let binaryChannel = binary != nil || rawReply
        // The prologue is done (a remote target has none): the next item of the
        // caller's batch may start.
        KernelTaskLocals.onFanout?()

        let box = DispatchBox()
        let extras = DispatchExtras()
        let work = Task {
            // THE ONE PLACE A SEND TAKES AN IDENTITY: inside the dispatch the
            // target is the sender, so everything its handler sends (and every
            // task it spawns) is the target's.
            let reply = await KernelTaskLocals.$onBatchLaunched.withValue(nil) {
                await KernelTaskLocals.$onFanout.withValue(nil) {
                    await KernelTaskLocals.$origin.withValue(origin) {
                        await KernelTaskLocals.$sender.withValue(target.id) {
                            await KernelTaskLocals.$currentTask.withValue(taskId) {
                                await KernelTaskLocals.$ownTask.withValue(taskId) {
                                    await KernelTaskLocals.$currentKilled.withValue(handle.killed) {
                                        await KernelTaskLocals.$secret.withValue(marks) {
                                            await self.dispatchBody(
                                                target: target, payload: payload,
                                                handlerPayload: handlerPayload, binary: binary,
                                                binaryChannel: binaryChannel, extras: extras)
                                        }
                                    }
                                }
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
        guard let outcome else { return DispatchEnd(reply: .null, stopped: true) }
        return DispatchEnd(reply: outcome, body: extras.body, raised: extras.raised)
    }

    /// What a dispatch task runs, inside its scopes. A blob rides the BINARY
    /// channel under this same row/ledger/timeout, and THE REPLY BODY COMES BACK
    /// WITH IT: the batch is the carrier for the `bytes` value python's
    /// reply dict holds natively.
    private func dispatchBody(
        target: Agent, payload: JSON, handlerPayload: JSON, binary: Blob?,
        binaryChannel: Bool, extras: DispatchExtras
    ) async -> JSON {
        // ANOTHER KERNEL'S AGENT: the call goes to its peer's route, bytes and
        // all; no handler runs here. Its reply is the peer's answer — a relayed
        // `raised` is data, not a raise here.
        if let r = target.remote {
            let (reply, b) = await callPeer(
                r, qualified: target.id, payload: handlerPayload, body: binary)
            extras.body = b
            return reply
        }
        if binaryChannel {
            let (r, b, raised) = await dispatchBinary(
                target: target, header: handlerPayload, body: binary)
            extras.body = b
            extras.raised = raised
            return r ?? .null
        }
        let (r, raised) = await dispatch(target: target, payload: payload)
        extras.raised = raised
        return r
    }

    /// One handler run: the reply, and whether the handler RAISED (python's
    /// `except` branch — the one source of `failed`). The raise is reported
    /// beside the reply, never read back out of it.
    private func dispatch(target: Agent, payload: JSON) async -> (JSON, raised: Bool) {
        let verb = payload["type"].asString ?? ""

        if Kernel.isSystemVerb(verb) {
            do {
                return (
                    try await handleSystemVerb(target: target, verb: verb, payload: payload), false
                )
            } catch {
                // A kernel verb that RAISED settles like any other raised
                // handler: python's one `except` wraps these too, so a digest
                // over an unsized `refs` fails the call instead of quietly
                // answering a page python never produces.
                return (Kernel.raisedReply(error.description), true)
            }
        }

        // THE TREE ROOT ANSWERS `reflect` WITH THE SUBSTRATE IDENTITY even when
        // it carries a handler_module (the `kernel_state` root loader): its
        // persistence verbs (load_tree / persist_record / boot) still route to
        // the bundle below, but `reflect <root>` is the kernel's own identity +
        // tree, matching the `kernel` address. Python `_agent.py`: `if verb ==
        // "reflect" and self.parent is None`.
        if verb == "reflect", target.parentId == nil {
            return (reflectIdentity(target), false)
        }

        // Bare agents (no handler_module): only the universal verbs
        // can answer. `reflect` is handled here for parity with Rust.
        guard let hm = target.handlerModule else {
            switch verb {
            case "boot", "shutdown":
                return (.null, false)
            case "reflect":
                return (reflectIdentity(target), false)
            default:
                // CODED, not a bare error. A reason-less `{error}` is decorated
                // by `applyReflectFlags` into something that reads like a
                // healthy reflect (kernel_verbs, tree, more) with a stray error
                // field — the caller cannot tell a refusal from an answer.
                return (noHandlerModuleRefusal(target.id.value, verb), false)
            }
        }

        guard let bundle = bundles.get(hm) else {
            // NOT a create-time refusal: this kernel legitimately holds records
            // whose bundle belongs to ANOTHER runtime (the browser's `*.ts`
            // agents persist here), or is installed later. The call simply
            // cannot be answered HERE, and says so with a code.
            return (notInstalledRefusal(target.id.value, hm, verb), false)
        }

        do {
            let reply = try await bundle.handle(
                agentId: target.id, payload: payload, kernel: self)
            return (reply ?? .null, false)
        } catch {
            // An exception is a RESULT. A handler that throws must not take its
            // caller down with it — the caller can act on `{"error": …}` and can
            // act on nothing at all. `raised: true` distinguishes a handler that
            // BLEW UP from one that deliberately answered `{"error": …}`, which is
            // the difference between a bug in the target and a refusal by it. The
            // AI routing path relies on it (python builds the same three-key body
            // in `_route_reply`'s own `except`, adding the `target`).
            return (Kernel.raisedReply("\(error)"), true)
        }
    }

    /// The body a RAISED handler settles with — python's `{"error": "<Type>:
    /// <msg>", "raised": True}`.
    static func raisedReply(_ text: String) -> JSON {
        .object(["error": .string(text), "raised": .bool(true)])
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
        // The EXECUTION LEDGER's retire verb — a dead letter is acknowledged.
        case "ack_deadletter":
            return true
        // The kernel's LIFECYCLE. `kernel_start` is the one start sequence (the
        // root's memory store first, then everyone); `kernel_stop` the graceful
        // stop of the process. `shutdown_agents` stops the agents, not the
        // process. Root-only, gated inside their handler to the tree root.
        case "kernel_start", "shutdown_agents", "kernel_stop":
            return true
        // HOW TO CALL an agent — its bundle's verb docs. Answered by the root,
        // and the one verb the call gate never checks (`gate`).
        case "docs":
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
/// NOT TRUNCATED. A summary that silently
/// drops the end of a payload lies about what was sent — the reader cannot tell a
/// short payload from a cut one, and the dropped part is exactly what you opened
/// telemetry to find. Eliding is a RENDER decision, where it is visible.
func summarize(_ payload: JSON, body: Blob? = nil) -> String {
    guard let body = body else { return pyJsonDumps(payload, ensureAscii: false) }
    var shrunk = payload
    // A telemetry line is not worth a refusal: an unplaceable path (never one the
    // kernel admitted) leaves the payload as it is.
    try? pySetPath(&shrunk, parts: body.parts, .string("<bytes:\(body.bytes.count)>"))
    return pyJsonDumps(shrunk, ensureAscii: false)
}

extension Kernel {
    /// May `origin` receive the outcome of an execution that ran `verb` on
    /// `target`? A denial, or nil.
    ///
    /// Joining hands back ANOTHER caller's result verbatim, so it is equivalent
    /// to having made that call: the gate re-judges the ORIGINAL verb for the
    /// joiner. An OPERATOR joiner (no origin) is ungated, as everywhere.
    func joinDenial(origin: AgentId?, payload: JSON?, target: Agent) -> JSON? {
        guard let joiner = origin else { return nil }
        // THE EXECUTION'S OWN VERB, read off its payload. A rehydrated row has no
        // payload: python's `(entry.get("payload") or {}).get("type")` is then
        // `None` and the gate runs on an empty verb.
        return gate(sender: joiner, verb: payload?["type"].asString ?? "", target: target)
    }

    /// Resolve an id the way `send` does — the reserved `kernel` address or a literal id.
    /// `nil` when it names no live agent.
    func resolveAgent(_ id: JSON) -> Agent? {
        guard let s = id.asString, !s.isEmpty else { return nil }
        return s == "kernel" ? root : agent(AgentId(s))
    }

    /// The two-sided call gate for an agent-originated send. Returns the error
    /// `JSON` if the send is refused, else `nil`.
    ///
    /// Egress (the sender's `can_call` grants `(target, verb)`) AND ingress (the
    /// target's `callable_by` grants `(sender, verb)`) — every target, the root
    /// included: a kernel verb takes the same two-sided check against the root.
    /// The sender is always a LOCAL agent — a call a bridge leg serves is judged
    /// as the leg. GRANTS ARE THE ONLY RULE: the tree verbs take
    /// this check and nothing more. A sender with no record → empty grant →
    /// deny-all.
    /// Every id ABOVE `agent` — what `$subtree` matches against: a grant holder
    /// reaches the other end when it is one of these. A peer target (no local
    /// parent) has none.
    public func ancestorIds(of agent: Agent?) -> Set<String> {
        var out: Set<String> = []
        var next = agent?.parentId
        while let pid = next, out.insert(pid.value).inserted {
            next = self.agent(pid)?.parentId
        }
        return out
    }

    func gate(sender: AgentId, verb: String, target: Agent) -> JSON? {
        let senderAgent = agent(sender)
        let sCan = senderAgent?.metaValue(forKey: "can_call") ?? .null
        let sParent = senderAgent?.parentId?.value

        // Egress — may the SENDER call the target?
        let d = authorizeCall(
            grant: sCan, verb: verb, otherId: target.id.value,
            holderId: sender.value, parentId: sParent, ancestorIds: ancestorIds(of: target))
        if !d.allowed {
            if let remote = target.remote {
                return .object([
                    "error": .string(
                        "\(sender.value) may not call \(pyRepr(verb)) on \(target.id.value)"),
                    "reason": .string("unauthorized"),
                    "hint": .string(
                        "\(target.id.value) is an agent on the connected kernel \(remote.guid): "
                            + "name it in can_call — {\(pyRepr(target.id.value)): [...]} or "
                            + "{'\(remote.guid)|*': [...]}"),
                    "side": .string("egress"),
                ])
            }
            return .object([
                "error": .string(
                    "\(sender.value) may not call \(pyRepr(verb)) on \(target.id.value)"),
                "reason": .string("unauthorized"), "hint": .string(d.hint),
                "side": .string("egress"),
            ])
        }
        // A target on ANOTHER KERNEL: egress is the whole check here. Whether
        // that agent accepts the call is its OWN kernel's decision, made where
        // the agent lives.
        if target.remote != nil { return nil }

        // Ingress — does the TARGET accept the sender? (tokens relative to the
        // target).
        let di = authorizeCall(
            grant: target.metaValue(forKey: "callable_by") ?? .null, verb: verb,
            otherId: sender.value, holderId: target.id.value, parentId: target.parentId?.value,
            ancestorIds: ancestorIds(of: senderAgent))
        if !di.allowed {
            return .object([
                "error": .string(
                    "\(target.id.value) does not accept \(pyRepr(verb)) from \(sender.value)"),
                "reason": .string("unauthorized"), "hint": .string(di.hint),
                "side": .string("ingress"),
            ])
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
