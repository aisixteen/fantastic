// The VIEW AGENTS over the file bridge — python `file_bridge/views.py`,
// `file_agent.py` and `folder_agent.py`, ported verb for verb (rust `views.rs`).
//
// A view is a record of this bundle carrying `view: file|folder`, a child of its
// bridge. This bundle answers it before the bridge's own gate: a view is not
// sealed, its gate is its grant on the bridge (`viewGrants`). Every disk operation
// is a send to the bridge AS THE VIEW, for the view's own path; a verb naming
// another path is refused here, before the bridge is asked.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

let VIEW_FILE = "file"
let VIEW_FOLDER = "folder"
/// The handler module a view record is shipped as: this bundle, its own dispatch.
let VIEW_HANDLER_MODULE = "file_bridge.tools"

/// python `views.GRANTS` — the verbs a view may send its bridge, nothing else.
private let fileGrants = ["read", "write", "watch_path", "unwatch_path"]
private let folderGrants = ["list", "watch_path", "unwatch_path"]
/// python `RELAY_VERB` — a view's relay watches its bridge.
private let relayVerb = "watch"

/// A view's grant on its bridge: its GRANTS, and the relay's `watch`.
func viewGrants(_ kind: String) -> [String] {
    (kind == VIEW_FILE ? fileGrants : folderGrants) + [relayVerb]
}

/// python `views.ANSWERS` — the verbs each view answers: what a caller may be granted on it.
private let fileAnswers = ["read", "write", "watch", "reflect"]
private let folderAnswers = ["list", "reveal", "watch", "reflect"]

/// python `file_agent.VERBS` docs — what `docs of=<file view>` serves: the view's own
/// table, never the bridge's (a view refuses a delete it is not granted).
let fileViewVerbDocs: JSON = [
    "reflect":
        "Identity, the file it views, its bridge, and whether its change relay is on. No args.",
    "read":
        "args: path? (this file only), offset:int=0 (line), limit:int=200 — the file's text page {path, lines:[{n,text}], total, eof, version}; refuses binary.",
    "write":
        "args: bytes (req — the whole document), if_match:str? (the version you read; a changed document is refused) — atomic whole-document write; {path, size, version}.",
    "watch":
        "args: none — start relaying this file's changes onto this agent: `{type: changed, path, kind: added|modified|deleted}`; WATCH this agent to receive them. Idempotent.",
]

/// python `folder_agent.VERBS` docs — the folder view's own table, as `fileViewVerbDocs`.
let folderViewVerbDocs: JSON = [
    "reflect":
        "Identity, the folder it views, its bridge, and whether its change relay is on. No args.",
    "list":
        "args: path? (a folder inside this one, relative to it; empty = this folder) — the folder's entries {files: [{name, path, type, size?}]}.",
    "reveal":
        "args: path (req — a file or folder inside this one, relative to it) — shows where it sits: {revealed, folder_path, dirs} where dirs are the folders to open on the way down; not_found when nothing is there.",
    "watch":
        "args: none — start relaying this folder's entry changes onto this agent: `{type: added|changed|removed, path, name}` (path relative to the folder's bridge); WATCH this agent to receive them. Idempotent.",
]

/// python `views.view_callers` — who may reach a view: whoever may reach its bridge, for
/// the verbs it answers. A bridge grant of `*` is all of them; a list keeps the verbs the
/// view answers. The bridge's `$parent` is named literally; its `$self` is skipped.
func viewCallers(_ bridge: Agent, kind: String) -> OrderedDictionary<String, JSON> {
    let own = kind == VIEW_FILE ? fileAnswers : folderAnswers
    var out: OrderedDictionary<String, Set<String>> = [:]
    for (key, grant) in bridge.metaValue(forKey: "callable_by")?.asObject ?? [:] {
        if key == "$self" { continue }
        var caller = key
        if key == "$parent" {
            guard let parent = bridge.parentId?.value else { continue }
            caller = parent
        }
        let verbs: [String] =
            grant.asString == "*"
            ? own
            : (grant.asArray ?? []).compactMap { $0.asString }.filter { own.contains($0) }
        if !verbs.isEmpty { out[caller, default: []].formUnion(verbs) }
    }
    var result: OrderedDictionary<String, JSON> = [:]
    for (caller, verbs) in out {
        result[caller] = .array(verbs.sorted().map { .string($0) })
    }
    return result
}

/// python `views.FOLDER_EVENT` — the bridge's disk kind → a folder's event type. Any
/// other kind is nil: the relay drops it, it is never relabelled as a change.
func folderEvent(_ raw: String) -> String? {
    switch raw {
    case "added": return "added"
    case "modified": return "changed"
    case "deleted": return "removed"
    default: return nil
    }
}

/// python `views.inside`: is the bridge path `target` inside the view's own path `own`?
/// A file is its own path only; a folder is itself and everything under it.
func viewInside(_ kind: String, own: String, target: String) -> Bool {
    if kind == VIEW_FILE { return target == own }
    return own == "." || target == own || target.hasPrefix(own + "/")
}

/// The verbs a view may send its bridge that name one path of the disk (python
/// `tools.SCOPED_VERBS`).
let viewScopedVerbs: Set<String> = ["read", "write", "list", "watch_path", "unwatch_path"]

/// python `tools._view_scope`: a VIEW caller reaches only inside its own path, whatever
/// its grant names. Nil admits; a refusal is returned. An operator or a plain agent is
/// not path-scoped here.
func viewPathGate(
    _ kernel: Kernel, bridge: String, origin: AgentId?, verb: String, payload: JSON
) -> JSON? {
    guard let scope = viewCallerScope(kernel, bridge: bridge, origin: origin) else { return nil }
    let rel: String
    switch payload["path"] {
    case .null: rel = ""
    case .string(let s): rel = s
    default: return Refusal.bad(verb, "path", "a string")
    }
    if let target = allowNorm(rel), !scope.own.isEmpty, viewInside(scope.kind, own: scope.own, target: target) {
        return nil
    }
    return Refusal.make(
        Refusal.unauthorized,
        "\(verb): \(pyRepr(rel)) is outside this \(scope.kind) view's path \(pyRepr(scope.own))",
        hint: "a view reaches only its own path; open a view of that path (open_view)",
        extra: [("path", .string(rel))])
}

/// python `views.caller_scope`: the (kind, path) a VIEW caller is scoped to on this
/// bridge, or nil for a caller that is not a view. A view of ANOTHER bridge has no
/// standing here: its path is empty, which matches nothing.
func viewCallerScope(_ kernel: Kernel, bridge: String, origin: AgentId?) -> (kind: String, own: String)? {
    guard let origin, let caller = kernel.agent(origin) else { return nil }
    guard let kind = caller.metaValue(forKey: "view")?.asString,
        kind == VIEW_FILE || kind == VIEW_FOLDER
    else { return nil }
    if caller.metaValue(forKey: "bridge")?.asString != bridge { return (kind, "") }
    return (kind, caller.metaValue(forKey: "path")?.asString ?? ".")
}

/// python `views.bridge_path`: the bridge path a view's relative `rel` names. A file
/// takes no operand but its own path; a folder's operand is inside the folder.
func viewBridgePath(_ kind: String, own: String, rel: String) -> String? {
    if kind == VIEW_FILE {
        let sub: String? = rel.isEmpty ? own : allowNorm(rel)
        return sub == own ? own : nil
    }
    guard let sub = allowNorm(rel) else { return nil }
    if own == "." { return sub }
    return sub == "." ? own : "\(own)/\(sub)"
}

/// python `views.scope`: the bridge path a verb's `path` names, or the refusal.
func viewScope(_ verb: String, kind: String, own: String, rel: JSON) -> Result<String, Refused> {
    let text: String
    switch rel {
    case .null: text = ""
    case .string(let s): text = s
    default: return .failure(Refused(json: Refusal.bad(verb, "path", "a string")))
    }
    guard let target = viewBridgePath(kind, own: own, rel: text) else {
        return .failure(
            Refused(
                json: Refusal.make(
                    Refusal.unauthorized,
                    "\(verb): \(pyRepr(text)) is outside this agent's path \(pyRepr(own))",
                    hint: "a view reaches only its own path; open a view of that path (open_view)",
                    extra: [("path", .string(text))])))
    }
    return .success(target)
}

/// python `os.path.basename(path) or "."`.
func viewBaseName(_ path: String) -> String {
    let base = path.split(separator: "/", omittingEmptySubsequences: false).last.map(String.init)
        ?? path
    return base.isEmpty ? "." : base
}

/// One send to the bridge, as the calling view. A send that never settled with a
/// reply is a refusal of its own, never a silent empty answer.
func viewSendBridge(_ kernel: Kernel, _ bridge: String, _ payload: JSON) async -> JSON {
    let verb = payload["type"].asString ?? ""
    let out = await kernel.send([Item(AgentId(bridge), payload)])
    guard let res = out.first?.result, res.asObject != nil else {
        return Refusal.make(
            Refusal.transportError,
            "\(bridge) \(verb): the send did not settle with a reply",
            extra: [("bridge", .string(bridge))])
    }
    return res
}

/// python `views.find_view`: the view record of this kind for this bridge and path.
func viewFind(_ kernel: Kernel, bridge: String, kind: String, path: String) -> String? {
    kernel.allAgents().first { a in
        a.metaValue(forKey: "view")?.asString == kind
            && a.metaValue(forKey: "bridge")?.asString == bridge
            && a.metaValue(forKey: "path")?.asString == path
    }?.id.value
}

/// python `views.open_view`: reuse the view of `path` on this bridge, or make it. The
/// bridge grants the new view exactly its grant in its `callable_by`.
///
/// `chosen` is a caller-picked id for the new view. It is taken only when it is free: a
/// `chosen` that is another agent, or a view of another path, is refused; so is a path
/// that already has a view under another id. Nothing is replaced or ignored.
func viewOpen(
    _ kernel: Kernel, bridge: Agent, kind: String, path: String, chosen: String? = nil
) async -> JSON {
    let bid = bridge.id.value
    let existing = viewFind(kernel, bridge: bid, kind: kind, path: path)
    if let chosen {
        if let held = kernel.agent(AgentId(chosen)) {
            let isView = held.metaValue(forKey: "view")?.asString == kind
                && held.metaValue(forKey: "bridge")?.asString == bid
                && held.metaValue(forKey: "path")?.asString == path
            if !isView {
                return Refusal.make(
                    Refusal.exists,
                    "open_view: \(pyRepr(chosen)) is already an agent, not a \(kind) view of \(pyRepr(path))",
                    extra: [("id", .string(chosen))])
            }
            return .object([
                "id": .string(chosen), "kind": .string(kind), "path": .string(path),
                "created": .bool(false),
            ])
        }
        if let existing {
            return Refusal.make(
                Refusal.exists,
                "open_view: \(pyRepr(path)) already has the \(kind) view \(pyRepr(existing))",
                hint: "open it by that id, or omit `id`",
                extra: [("id", .string(existing))])
        }
    }
    if let existing {
        return .object([
            "id": .string(existing), "kind": .string(kind), "path": .string(path),
            "created": .bool(false),
        ])
    }
    let own = viewGrants(kind)
    var payload: OrderedDictionary<String, JSON> = [
        "handler_module": .string(VIEW_HANDLER_MODULE),
        "parent_id": .string(bid),
        "view": .string(kind),
        "bridge": .string(bid),
        "path": .string(path),
        "display_name": .string(viewBaseName(path)),
        "can_call": .object([bid: .array(own.map { .string($0) })]),
        "callable_by": .object(viewCallers(bridge, kind: kind)),
    ]
    if let chosen { payload["id"] = .string(chosen) }
    let made = await kernel.createFromPayload(payload: .object(payload))
    if made.asObject?["error"] != nil { return made }
    let vid = made["id"].asString ?? ""
    var cb = bridge.metaValue(forKey: "callable_by")?.asObject ?? [:]
    var grants = Set(cb[vid]?.asArray?.compactMap { $0.asString } ?? [])
    grants.formUnion(own)
    cb[vid] = .array(grants.sorted().map { .string($0) })
    kernel.update(bridge.id, ["callable_by": .object(cb)])
    return .object([
        "id": .string(vid), "kind": .string(kind), "path": .string(path), "created": .bool(true),
    ])
}

/// The views this bundle is running: each view's relay, by view id. RAM, per bundle.
final class ViewRelays: @unchecked Sendable {
    struct Relay {
        let inbox: AgentId
        let bridge: String
        let path: String
    }

    private let lock = NSLock()
    private var held: [String: Relay] = [:]

    func has(_ view: String) -> Bool { lock.withLock { held[view] != nil } }

    func put(_ view: String, _ relay: Relay) { lock.withLock { held[view] = relay } }

    func take(_ view: String) -> Relay? { lock.withLock { held.removeValue(forKey: view) } }
}

/// python `views._relay_rule`: one bridge `changed` → the view's own event, or nil.
func viewRelayEvent(_ kind: String, own: String, ev: JSON) -> JSON? {
    guard let p = ev["path"].asString else { return nil }
    guard let raw = ev["kind"].asString, ["added", "modified", "deleted"].contains(raw) else {
        // The bridge reports added / modified / deleted, always. Anything else is a bug
        // upstream: dropped and logged, never relabelled as a change.
        FileHandle.standardError.write(
            Data("  [file_bridge] view relay: dropped event with kind \(ev["kind"]) for \(pyRepr(p))\n".utf8))
        return nil
    }
    if kind == VIEW_FILE {
        if p != own { return nil }
        return .object(["type": .string("changed"), "path": .string(own), "kind": .string(raw)])
    }
    let parent: String
    let name: String
    if let cut = p.lastIndex(of: "/") {
        parent = String(p[..<cut])
        name = String(p[p.index(after: cut)...])
    } else {
        parent = "."
        name = p
    }
    if p == own || parent != own { return nil }  // the folder itself, or something deeper
    guard let event = folderEvent(raw) else { return nil }
    return .object([
        "type": .string(event), "path": .string(p), "name": .string(name),
    ])
}

/// python `views.watch`: arm the bridge's disk watch for this view's path once, and
/// relay its changes onto the view's own inbox.
func viewWatch(
    _ kernel: Kernel, relays: ViewRelays, view: AgentId, kind: String, own: String, bridge: String
) async -> JSON {
    if relays.has(view.value) {
        return .object([
            "watching": .string(own), "agent": .string(view.value), "relayed": .bool(true),
        ])
    }
    let armed = await viewSendBridge(
        kernel, bridge, .object(["type": .string("watch_path"), "path": .string(own)]))
    if armed.asObject?["error"] != nil { return armed }
    let inbox = AgentId("\(view.value)#relay")
    let rx = kernel.ensureInbox(inbox)
    if let gate = kernel.watch(src: AgentId(bridge), watcher: inbox) {
        kernel.removeInbox(inbox)
        _ = await viewSendBridge(
            kernel, bridge, .object(["type": .string("unwatch_path"), "path": .string(own)]))
        return gate
    }
    kernel.spawnTask(agent: view, verb: "watch") {
        while let item = await rx.next() {
            guard item.payload["type"].asString == "changed" else { continue }
            if let out = viewRelayEvent(kind, own: own, ev: item.payload) {
                await kernel.emit(view, out)
            }
        }
    }
    relays.put(view.value, .init(inbox: inbox, bridge: bridge, path: own))
    return .object([
        "watching": .string(own), "agent": .string(view.value), "relayed": .bool(false),
    ])
}

/// python `views.stop`: the view is going away — its relay ends and the bridge's
/// watch is let go. Runs AS the view (the kernel's `onDelete` context).
func viewStop(_ kernel: Kernel, relays: ViewRelays, view: AgentId) async {
    guard let st = relays.take(view.value) else { return }
    kernel.unwatch(src: AgentId(st.bridge), watcher: st.inbox)
    kernel.removeInbox(st.inbox)
    _ = await viewSendBridge(
        kernel, st.bridge, .object(["type": .string("unwatch_path"), "path": .string(st.path)]))
}

extension FileBundle {
    /// The view's verb table: a `file` answers read, write, watch, reflect; a `folder`
    /// answers list, reveal, watch, reflect. Anything else is unhandled by the view.
    func viewHandle(_ kernel: Kernel, agent: Agent, verb: String, payload: JSON) async -> JSON {
        let kind = agent.metaValue(forKey: "view")?.asString ?? ""
        let own = agent.metaValue(forKey: "path")?.asString ?? ""
        let bridge = agent.metaValue(forKey: "bridge")?.asString ?? ""
        switch (kind, verb) {
        case (_, "reflect"):
            return viewReflect(agent: agent)
        case (VIEW_FILE, "read"), (VIEW_FILE, "write"):
            switch viewScope(verb, kind: VIEW_FILE, own: own, rel: payload["path"]) {
            case .failure(let refused): return refused.json
            case .success(let target):
                var body = payload.asObject ?? [:]
                body["type"] = .string(verb)
                body["path"] = .string(target)
                return await viewSendBridge(kernel, bridge, .object(body))
            }
        case (VIEW_FILE, "watch"), (VIEW_FOLDER, "watch"):
            return await viewWatch(
                kernel, relays: viewRelays, view: agent.id, kind: kind, own: own, bridge: bridge)
        case (VIEW_FOLDER, "list"):
            switch viewScope("list", kind: VIEW_FOLDER, own: own, rel: payload["path"]) {
            case .failure(let refused): return refused.json
            case .success(let target):
                return await viewSendBridge(
                    kernel, bridge, .object(["type": .string("list"), "path": .string(target)]))
            }
        case (VIEW_FOLDER, "reveal"):
            return await viewReveal(kernel, own: own, bridge: bridge, payload: payload)
        default:
            return Refusal.make(
                Refusal.unhandled, "\(kind) agent: unknown type \(pyRepr(verb))",
                extra: [("verb", .string(verb))])
        }
    }

    /// python `reflect` of a view: what it IS — its kind, path, bridge, relay state.
    /// A VIEW answers its own verbs, so `docs of=<view>` serves its table (python
    /// `instance_verbs`); every other record keeps the bridge's.
    public func instanceVerbDocs(_ agent: Agent) -> JSON? {
        switch agent.metaValue(forKey: "view")?.asString {
        case VIEW_FILE: return fileViewVerbDocs
        case VIEW_FOLDER: return folderViewVerbDocs
        default: return nil
        }
    }

    private func viewReflect(agent: Agent) -> JSON {
        let kind = agent.metaValue(forKey: "view")?.asString ?? ""
        let sentence =
            kind == VIEW_FILE
            ? "A file on a file_bridge — one live object per path: its text, its writes and its changes."
            : "A folder on a file_bridge — one live object per folder: its entries and their changes."
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string(sentence),
            "kind": .string(kind),
            "path": agent.metaValue(forKey: "path") ?? .null,
            "bridge": agent.metaValue(forKey: "bridge") ?? .null,
            "watching": .bool(viewRelays.has(agent.id.value)),
        ])
    }

    /// python `_reveal`: where a path inside the folder sits — the folders to open on
    /// the way down. `not_found` when nothing is there.
    private func viewReveal(_ kernel: Kernel, own: String, bridge: String, payload: JSON) async
        -> JSON
    {
        guard let rel = payload["path"].asString, !rel.isEmpty else {
            return Refusal.missing("reveal", "path")
        }
        let target: String
        switch viewScope("reveal", kind: VIEW_FOLDER, own: own, rel: payload["path"]) {
        case .failure(let refused): return refused.json
        case .success(let t): target = t
        }
        let parent: String
        if let cut = target.lastIndex(of: "/") {
            parent = String(target[..<cut])
        } else {
            parent = "."
        }
        let listed = await viewSendBridge(
            kernel, bridge, .object(["type": .string("list"), "path": .string(parent)]))
        if listed.asObject?["error"] != nil { return listed }
        let name = viewBaseName(target)
        let present = (listed["files"].asArray ?? []).contains { $0["name"].asString == name }
        if !present {
            return Refusal.make(
                Refusal.notFound, "reveal: \(pyRepr(rel)) is not in this folder",
                extra: [("path", .string(rel))])
        }
        // The path inside the folder: python's `posixpath.relpath(target, own)`.
        let inside: String
        if own == "." {
            inside = target
        } else if target.hasPrefix(own + "/") {
            inside = String(target.dropFirst(own.count + 1))
        } else {
            inside = "."
        }
        var dirs: [JSON] = []
        if let cut = inside.lastIndex(of: "/") {
            let parts = inside[..<cut].split(separator: "/", omittingEmptySubsequences: false)
            for i in parts.indices {
                dirs.append(.string(parts[...i].joined(separator: "/")))
            }
        }
        return .object([
            "revealed": .string(inside), "folder_path": .string(own), "dirs": .array(dirs),
        ])
    }

    /// The `open_view` verb on a bridge: the arguments checked the way python checks
    /// them, then the view reused or made. The path must be exposed (`allow`), as any
    /// path verb's is.
    func openViewVerb(_ kernel: Kernel, agent: Agent, payload: JSON) async -> JSON {
        let kind: String
        switch payload["kind"].asString {
        case .some(let k) where k == VIEW_FILE || k == VIEW_FOLDER: kind = k
        default: return Refusal.bad("open_view", "kind", "`file` or `folder`")
        }
        let chosen: String?
        switch payload["id"] {
        case .null: chosen = nil
        case .string(let s):
            if let err = Refusal.guardAgentId("open_view", s) { return err }
            chosen = s
        default: return Refusal.bad("open_view", "id", "an agent id (a string)")
        }
        let path: String
        do { path = try pathArg("open_view", payload, "path") } catch { return error.json }
        guard let norm = allowNorm(path) else {
            return Refusal.bad("open_view", "path", "a root-relative path inside the root")
        }
        if kind == VIEW_FILE && norm == "." {
            return Refusal.bad("open_view", "path", "a file path (the root is a folder)")
        }
        guard let base = kernel.workdir?.path else { return Self.noRunningDir() }
        if let err = gatePath(
            "open_view", root: rootStr(agent), allow: allowList(agent), path: norm, base: base)
        {
            return err
        }
        return await viewOpen(kernel, bridge: agent, kind: kind, path: norm, chosen: chosen)
    }

    /// The `validate_meta` judgement on the view field: a view is `file` or `folder`.
    static func viewRefusal(_ meta: OrderedDictionary<String, JSON>) -> String? {
        switch meta["view"] {
        case nil, .some(.null): return nil
        case .some(.string(let s)) where s == VIEW_FILE || s == VIEW_FOLDER: return nil
        default:
            return "view must be `file` or `folder` — a view of one path on this bridge; "
                + "open one with open_view"
        }
    }
}
