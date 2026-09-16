// Filesystem-as-agent bundle — the fs-transport member of the io family.
//
// Mirrors py `file_bridge.tools` (the spec) and rust `fantastic-file`. This
// agent is the GATE (the ingress rule) + the verb shape over `Fs.swift`, the one
// disk surface; it never touches the disk itself. Each agent owns a `root`
// directory (from agent meta) and a `readonly` flag; every verb resolves its
// path within `root` through `fsResolve`, which refuses `..`, an absolute path
// and any symlink pointing out of the root.
//
// Verbs: reflect / boot / list / read_stream / write_stream /
//        pump / delete / rename / mkdir.
//
// Every refusal is the one kernel shape (`Refusal`): a caller branches on
// `reason`, never on prose. A payload can never trap this bundle — a negative
// `offset` used to hit `UInt64(Int64)` and abort the whole kernel (#34/#36).

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "file_bridge.tools"

/// Default hidden-file names hidden from `list` — python's `DEFAULT_HIDDEN`, in
/// its order (the list's LENGTH is `hidden_count`, so it is a list, not a set).
let DEFAULT_HIDDEN: [String] = [
    ".git", ".env", ".fantastic", "node_modules", "__pycache__",
]

let IMAGE_EXTENSIONS: Set<String> = [
    "png", "jpg", "jpeg", "gif", "webp", "svg",
]

/// A readonly leg refuses every mutating verb — in THE ONE REFUSAL SHAPE.
///
/// This answered a bare `{"error": "agent is readonly"}` until 2026-08-27, when
/// the shared agentic corpus caught it: no `reason`, so a caller could only
/// branch on the prose. `unauthorized` is the code the io SEAL answers with too
/// — both say "this edge is not open for that", and neither is about the
/// caller, which is why neither carries a `side`.
func readonlyRefusal() -> JSON {
    Refusal.make(
        Refusal.unauthorized, "agent is readonly",
        hint: "this leg accepts reads only; an operator may clear `readonly`")
}

/// `password_ingress` is meaningless on an in-process leg: the token is read
/// off a WIRE ENVELOPE, and the fs/process edges have none — no frame ever
/// presents one, so the rule could only ever refuse (python) or, worse, be
/// satisfied by a secret persisted ON THE RECORD (the ports, until
/// 2026-09-02). Refused at the write, next to the mistake (#116). Both
/// spellings: the scoped `ingress_rule` and the `auth` stem. Shared with
/// `FantasticExecBridge`.
public func passwordRefusal(_ meta: OrderedDictionary<String, JSON>, edge: String) -> String? {
    for (field, stem) in [("ingress_rule", "password_ingress"), ("auth", "password")] {
        if let spec = meta[field], ruleName(spec, default: "") == stem {
            return "\(field)=\(pyRepr(spec)) is not applicable to the \(edge): "
                + "a leg with no wire envelope can never present a token — "
                + "legal values: allow_all_ingress, deny_ingress"
        }
    }
    return nil
}

/// A refusal carried as an error, so argument helpers compose with `try`.
struct Refused: Error {
    let json: JSON
}

/// A string argument that is a ROOT-RELATIVE path: absent/null ⇒ `""`; a
/// non-string or an absolute path is `bad_arg` (the absolute form is refused,
/// never silently re-rooted — python used to `lstrip("/")` against its own
/// docstring; #117). Mirrors py `_path_arg`.
func pathArg(_ verb: String, _ payload: JSON, _ name: String) throws(Refused) -> String {
    let p: String
    switch payload[name] {
    case .null: p = ""
    case .string(let s): p = s
    default: throw Refused(json: Refusal.bad(verb, name, "a string"))
    }
    if p.hasPrefix("/") {
        throw Refused(json: Refusal.bad(verb, name, "a root-relative path (no leading '/')"))
    }
    return p
}

/// An integer argument with a floor: absent/null ⇒ `def`; anything that is
/// not a JSON integer (a bool, a float, a string) or is below `minimum` is
/// `bad_arg` naming the arg. Mirrors py `_int_arg`. Parsed as Int64 and
/// refused BEFORE any unsigned conversion — `UInt64(-1)` is a trap.
func intArg(_ verb: String, _ payload: JSON, _ name: String, default def: Int64, minimum: Int64)
    throws(Refused) -> Int64
{
    switch payload[name] {
    case .null: return def
    case .integer(let v) where v >= minimum: return v
    default: throw Refused(json: Refusal.bad(verb, name, "an integer >= \(minimum)"))
    }
}

public struct FileBundle: AgentBundle {
    public let name = "file_bridge"

    public init() {}

    public var readme: String? { Self.readmeText }

    /// Refuse an `ingress_rule` no resolver implements. An unknown rule now SEALS
    /// rather than throwing, so the edge is safe either way — but reporting
    /// `{"updated": true}` for a security control that does not exist is its own
    /// failure, and the complaint belongs at the write, next to the typo.
    /// Mirrors py `file_bridge.validate_meta` / rust `FileBundle::validate_meta`.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        if let spec = meta["ingress_rule"], let rule = try? IngressRules.resolve(spec),
            let deny = rule as? IngressRules.DenyInbound, deny.unknown != nil
        {
            // The SPEC as written, python's `{spec!r}` — naming the resolved rule
            // instead hid the typo the operator has to find.
            return "ingress_rule=\(pyRepr(spec)) is not a rule this bundle implements — "
                + "legal values: allow_all_ingress, deny_ingress"
        }
        return passwordRefusal(meta, edge: "fs edge")
    }

    /// The verbs this bundle serves, in python's `VERBS` order.
    static let verbs: [String] = [
        "reflect", "boot", "list", "read_stream", "write_stream", "pump", "delete", "rename",
        "mkdir",
    ]

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        let type = payload["type"]
        let verb = type.asString ?? ""
        // UNKNOWN VERB FIRST — before the gate and before any clamp, as python's
        // `VERBS.get(t)` is. A sealed bridge answers `unhandled` for a verb it
        // does not have, not `unauthorized` (`shutdown` is one of those).
        guard Self.verbs.contains(verb) else {
            return Refusal.make(
                Refusal.unhandled, "file_bridge: unknown type \(pyRepr(type))",
                extra: [("verb", type)])
        }
        // GATE — the fs edge is an io_bridge leg: SEALED by default. Every verb
        // except `reflect` is denied until opened (ingress_rule=allow_all_ingress);
        // python's UNGATED set is `{"reflect"}` alone, so a sealed `boot` denies.
        // NO token: the fs edge has no wire envelope, so nothing can present one.
        // It used to pass the record's `auth_token` — a persisted secret in the
        // portable workdir satisfied a `password_ingress` leg, the one thing the
        // password rule exists to prevent (#116).
        if let denied = gateVerb(
            ingressRule: agent.metaValue(forKey: "ingress_rule"),
            auth: agent.metaValue(forKey: "auth"),
            authToken: nil,
            agentId: agent.id.value,
            verb: verb)
        {
            return denied
        }
        let base = workdirBase(kernel: kernel)
        switch verb {
        case "reflect":
            return reflect(agent: agent, base: base)
        case "boot":
            return .null
        case "list":
            return listFiles(agent: agent, payload: payload, base: base)
        case "delete":
            return deleteFile(agent: agent, payload: payload, base: base)
        case "rename":
            return renameFile(agent: agent, payload: payload, base: base)
        case "mkdir":
            return makeDir(agent: agent, payload: payload, base: base)
        // THE STREAM VERBS ON THE TEXT CHANNEL, as python serves them: a model
        // writes a document VERBATIM between `<bytes>` and `</bytes>` and the
        // tag grammar delivers it as utf-8 bytes to the same handler; a chunk
        // read back renders utf-8 in the reply. Swift refused both here with
        // "raw bytes on the binary channel" — the `subai_graph` routine's
        // program write failed on it every turn (measured 2026-09-13, the same
        // refusal rust carried in 43 e2e logs). The binary channel stays for
        // real blobs.
        case "read_stream":
            let (reply, body) = readStream(agent: agent, payload: payload, base: base)
            guard case .object(var o) = reply, o["error"] == nil else { return reply }
            // python's key order: path, offset, bytes, next_offset, eof, size.
            var out: OrderedDictionary<String, JSON> = [:]
            out["path"] = o["path"]
            out["offset"] = o["offset"]
            out["bytes"] = .string(String(decoding: body, as: UTF8.self))
            out["next_offset"] = o["next_offset"]
            out["eof"] = o["eof"]
            out["size"] = o["size"]
            return .object(out)
        case "write_stream":
            if pyTruthy(agent.metaValue(forKey: "readonly") ?? .null) { return readonlyRefusal() }
            let blob: Data
            switch payload["bytes"] {
            case .string(let text): blob = Data(text.utf8)
            // The tag grammar types a bare number; a number is a document too.
            case .integer(let n): blob = Data(String(n).utf8)
            case .double(let d): blob = Data(pyFloatRepr(d).utf8)
            case .null where payload.asObject?["bytes"] == nil:
                // ABSENT is an empty chunk (python `payload.get("bytes", b"")`);
                // an explicit `null` is not a chunk at all.
                blob = Data()
            default:
                return Refusal.missing("write_stream", "bytes (one raw chunk)")
            }
            return writeStream(agent: agent, payload: payload, blob: blob, base: base)
        case "pump":
            // The PUMP coordinates a SOURCE→SINK copy over the binary channel
            // (it never touches bytes itself), so it rides the text channel.
            return await pump(agent: agent, payload: payload, kernel: kernel)
        default:
            return nil
        }
    }

    /// Binary channel: `read_stream` (empty request → reply BODY = one raw chunk)
    /// and `write_stream` (request BODY = one raw chunk → status reply, no body).
    /// Symmetric with py/rust's `bytes`-in-the-dict; never base64. Same
    /// sealed-by-default gate + running-dir clamp as the text channel. Any other
    /// verb routes through the text `handle` (its blob is unused).
    public func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data) {
        let verb = header["type"].asString ?? ""
        guard verb == "read_stream" || verb == "write_stream"
        else {
            let reply = try await handle(agentId: agentId, payload: header, kernel: kernel)
            return (reply, Data())
        }
        guard let agent = kernel.agent(agentId) else {
            return (.object(["error": .string("no agent \(agentId)")]), Data())
        }
        // GATE — same sealed-by-default choke point as the text channel.
        if let denied = gateVerb(
            ingressRule: agent.metaValue(forKey: "ingress_rule"),
            auth: agent.metaValue(forKey: "auth"),
            authToken: nil,
            agentId: agent.id.value, verb: verb)
        {
            return (denied, Data())
        }
        let base = workdirBase(kernel: kernel)
        if verb == "read_stream" {
            return readStream(agent: agent, payload: header, base: base)
        }
        if pyTruthy(agent.metaValue(forKey: "readonly") ?? .null) {
            return (readonlyRefusal(), Data())
        }
        return (writeStream(agent: agent, payload: header, blob: blob, base: base), Data())
    }

    // MARK: - Verbs

    /// python `_reflect`: identity + the clamped root + the readonly flag + the
    /// auth posture. NEVER refuses — a root that escapes shows the configured
    /// string plus `root_error`.
    private func reflect(agent: Agent, base: String) -> JSON {
        let root = rootStr(agent)
        var rootDisplay: String
        var rootError: String? = nil
        do {
            rootDisplay = try fsResolveRoot(root, base: base)
        } catch {
            rootDisplay = root
            rootError = error.message
        }
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(agent.id.value),
            "sentence": .string(
                "Filesystem edge of the io family — sealed by default, running-dir only."),
            "root": .string(rootDisplay),
            "readonly": .bool(pyTruthy(agent.metaValue(forKey: "readonly") ?? .null)),
            "hidden_count": .integer(Int64(hidden(agent: agent).count)),
            "verbs": [
                "reflect":
                    "Identity + filesystem root + readonly flag + auth posture (ingress/egress/sealed/see). No args.",
                "boot": "No-op. Returns None.",
                "list":
                    "args: path:str (default ''). Returns {path, files:[{name, path, type, size?}, ...]}. Hides DEFAULT_HIDDEN entries (.git, .env, .fantastic, …).",
                "read_stream":
                    "args: path:str (req), offset:int=0, length:int=65536. The SOURCE half — returns",
                "write_stream":
                    "args: path:str (req), bytes:bytes (req — ONE raw chunk; a model writes it VERBATIM between `<bytes>` and `</bytes>`, nothing escaped), offset:int? (default =",
                "pump": "The PUMP — a server-side stream copy SOURCE→SINK, chunk by chunk, in ONE",
                "delete":
                    "args: path:str (req), recursive:bool=False. Returns {path, deleted:true}.",
                "rename":
                    "args: old_path:str, new_path:str (both req). Returns {old_path, new_path}. Creates new parent dirs. Refused if readonly.",
                "mkdir":
                    "args: path:str (req). Returns {path, created:true}. Recursive (parents ok). Refused if readonly.",
            ] as JSON,
        ]
        // The auth posture, python's `**_describe(rec)` — after `verbs`.
        for (k, v) in describe(agent.meta) { out[k] = v }
        if let rootError { out["root_error"] = .string(rootError) }
        return .object(out)
    }

    private func listFiles(agent: Agent, payload: JSON, base: String) -> JSON {
        let root = rootStr(agent)
        let relative: String
        do { relative = try pathArg("list", payload, "path") } catch { return error.json }
        let rdir: String
        let entries: [String]
        do {
            if !(try fsExists(root, relative, base: base)) {
                return notFound("path \(pyRepr(relative)) does not exist", path: relative)
            }
            if !(try fsIsDir(root, relative, base: base)) {
                return Refusal.bad("list", "path", "a directory; \(pyRepr(relative)) is not one")
            }
            rdir = try fsResolveRoot(root, base: base)
            entries = try fsListDir(root, relative, base: base)
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        let hiddenNames = hidden(agent: agent)
        var files: [JSON] = []
        for entry in entries {
            let entryName = String(entry[entry.index(after: entry.lastIndex(of: "/")!)...])
            if hiddenNames.contains(.string(entryName)) { continue }
            // python `entry.relative_to(rdir)` — relative to the RESOLVED root,
            // so `sub/../x` lists as `x/a.txt` and `.` as `a.txt`.
            let rel = String(entry.dropFirst(rdir == "/" ? 1 : rdir.count + 1))
            var record: OrderedDictionary<String, JSON> = [:]
            record["name"] = .string(entryName)
            record["path"] = .string(rel)
            if pathIsDir(entry) {
                record["type"] = .string("dir")
            } else {
                record["type"] = .string("file")
                record["size"] = .integer(pathSize(entry) ?? -1)
            }
            files.append(.object(record))
        }
        return .object(["path": .string(relative), "files": .array(files)])
    }

    /// `delete path recursive?` — a file, an EMPTY dir, or (`recursive:true`) a
    /// dir tree. A symlink is UNLINKED, never followed, so a dangling one is
    /// still a deletable name (python `fs.remove`).
    private func deleteFile(agent: Agent, payload: JSON, base: String) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let root = rootStr(agent)
        let relative: String
        do { relative = try targetArg("delete", root, payload, "path", base: base) } catch {
            return error.json
        }
        do {
            // `exists` follows symlinks — a DANGLING link is still a deletable name.
            let present = try fsExists(root, relative, base: base)
            let link = present ? true : try fsIsSymlink(root, relative, base: base)
            if !present && !link {
                return notFound("path \(pyRepr(relative)) does not exist", path: relative)
            }
            try fsRemove(
                root, relative, recursive: pyTruthy(payload["recursive"]), base: base)
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        return .object(["path": .string(relative), "deleted": .bool(true)])
    }

    private func renameFile(agent: Agent, payload: JSON, base: String) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let root = rootStr(agent)
        let old: String
        let new: String
        do {
            old = try targetArg("rename", root, payload, "old_path", base: base)
            new = try targetArg("rename", root, payload, "new_path", base: base)
        } catch {
            return error.json
        }
        do {
            if !(try fsExists(root, old, base: base)) {
                return notFound("path \(pyRepr(old)) does not exist", path: old)
            }
            try fsRename(root, old, new, base: base)
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        return .object(["old_path": .string(old), "new_path": .string(new)])
    }

    private func makeDir(agent: Agent, payload: JSON, base: String) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let root = rootStr(agent)
        let relative: String
        do { relative = try targetArg("mkdir", root, payload, "path", base: base) } catch {
            return error.json
        }
        do { try fsMkdir(root, relative, base: base) } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        return .object(["path": .string(relative), "created": .bool(true)])
    }

    // MARK: - Helpers

    /// python `_hidden(rec)` — `list(rec.get("hidden", DEFAULT_HIDDEN))`: the
    /// stored LIST, duplicates and non-strings included (`hidden_count` is its
    /// length); a set is used only for the filtering compare.
    private func hidden(agent: Agent) -> [JSON] {
        switch agent.metaValue(forKey: "hidden") {
        case .some(.array(let a)): return a
        // `list("abc")` is its characters, `list({...})` its keys.
        case .some(.string(let s)): return s.map { .string(String($0)) }
        case .some(.object(let o)): return o.keys.map { .string($0) }
        default: return DEFAULT_HIDDEN.map { .string($0) }
        }
    }

    /// python `_root_str(rec)` — `rec.get("root", "") or ""`, as the string
    /// `fs.resolve_root` clamps (`str(root or "")`).
    private func rootStr(_ agent: Agent) -> String {
        let raw = agent.metaValue(forKey: "root") ?? .null
        return pyTruthy(raw) ? pyStr(raw) : ""
    }

    /// The kernel's RUNNING DIRECTORY — its disk workdir (the dir that holds
    /// `.fantastic`). In production this is the process cwd, matching py
    /// `Path.cwd()`; for an in-memory kernel that still touches disk we fall
    /// back to the process cwd.
    private func workdirBase(kernel: Kernel) -> String {
        if let wd = kernel.storage.workdir { return wd.path }
        return FileManager.default.currentDirectoryPath
    }

    /// python `_target_arg` — `pathArg` for a MUTATING verb: the path must name
    /// something INSIDE the root; empty/`.` is `missing_arg` and anything
    /// resolving to the root itself is `bad_arg`. `delete {path:"",
    /// recursive:true}` used to remove the whole served store (#97): the root is
    /// the boundary, not an operand.
    func targetArg(_ verb: String, _ root: String, _ payload: JSON, _ name: String, base: String)
        throws(Refused) -> String
    {
        let p = try pathArg(verb, payload, name)
        if p.isEmpty || p == "." { throw Refused(json: Refusal.missing(verb, name)) }
        let isRoot: Bool
        do {
            isRoot = try fsResolve(root, p, base: base) == (try fsResolveRoot(root, base: base))
        } catch {
            throw Refused(json: Refusal.make(Refusal.transportError, error.message))
        }
        if isRoot {
            throw Refused(
                json: Refusal.bad(verb, name, "a path inside the root, not the root itself"))
        }
        return p
    }

    private func readonlyError(agent: Agent) -> JSON? {
        // NOT a ternary with `nil`: `JSON` is ExpressibleByNilLiteral, so the
        // else-branch types as `JSON.null` and every mutating verb answered
        // `null` instead of running.
        if pyTruthy(agent.metaValue(forKey: "readonly") ?? .null) { return readonlyRefusal() }
        return nil
    }

    // MARK: - Streams

    /// SOURCE — read ONE raw chunk at `offset`. Reply `{path, offset,
    /// next_offset, eof, size}` + body = the raw bytes (binary channel; never
    /// base64). Stateless cursor: pull the next with `offset=next_offset` to eof.
    private func readStream(agent: Agent, payload: JSON, base: String) -> (JSON, Data) {
        let root = rootStr(agent)
        let path: String
        let offset: Int64
        let length: Int64
        do {
            path = try pathArg("read_stream", payload, "path")
            offset = try intArg("read_stream", payload, "offset", default: 0, minimum: 0)
            length = try intArg("read_stream", payload, "length", default: 65536, minimum: 1)
        } catch {
            return (error.json, Data())
        }
        let chunk: Data
        let size: Int64
        do {
            if !(try fsIsFile(root, path, base: base)) {
                return (notFound("file \(pyRepr(path)) not found", path: path), Data())
            }
            (chunk, size) = try fsReadChunk(
                root, path, offset: offset, length: length, base: base)
        } catch {
            return (Refusal.make(Refusal.transportError, error.message), Data())
        }
        let nextOffset = offset + Int64(chunk.count)
        return (
            .object([
                "path": .string(path),
                "offset": .integer(offset),
                "next_offset": .integer(nextOffset),
                "eof": .bool(nextOffset >= size),
                "size": .integer(size),
            ]), chunk
        )
    }

    /// SINK — write ONE raw chunk (`blob`) at `offset` (default: append at end).
    /// `truncate:true` on the first chunk starts fresh. Returns
    /// `{path, written, offset, size}` — `size` is the FILE's size after the
    /// write (py `stat().st_size`), not the write cursor (#59).
    private func writeStream(agent: Agent, payload: JSON, blob: Data, base: String) -> JSON {
        let root = rootStr(agent)
        let path: String
        let offsetIn: Int64
        do {
            path = try pathArg("write_stream", payload, "path")
            offsetIn = try intArg("write_stream", payload, "offset", default: -1, minimum: 0)
        } catch {
            return error.json
        }
        let off: Int64
        let size: Int64
        do {
            (off, size) = try fsWriteChunk(
                root, path, blob, offset: offsetIn < 0 ? nil : offsetIn,
                truncate: pyTruthy(payload["truncate"]), base: base)
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        return .object([
            "path": .string(path),
            "written": .integer(Int64(blob.count)),
            "offset": .integer(off),
            "size": .integer(size),
        ])
    }

    /// The PUMP — a server-side SOURCE→SINK copy, chunk by chunk, in ONE call.
    /// Storage-agnostic: both ends are bound BY ID + the duck-typed stream verbs
    /// over the binary channel, so a `network_bridge` SOURCE pumps to a
    /// `file_bridge` SINK the same as fs→fs. Each end SELF-gates + SELF-clamps.
    private func pump(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        let selfId = JSON.string(agent.id.value)
        // python's `or` fallbacks: a FALSY value takes the default, whatever
        // its type (`""`, `0`, `[]` all fall through).
        let src = pyTruthy(payload["source"]) ? payload["source"] : selfId
        let sink = pyTruthy(payload["sink"]) ? payload["sink"] : selfId
        let spath =
            pyTruthy(payload["source_path"])
            ? payload["source_path"] : (pyTruthy(payload["path"]) ? payload["path"] : .string(""))
        let dpath = pyTruthy(payload["sink_path"]) ? payload["sink_path"] : spath
        let length: Int64
        do { length = try intArg("pump", payload, "chunk", default: 65536, minimum: 1) } catch {
            return error.json
        }
        var offset: Int64 = 0
        var first = true
        var chunks: Int64 = 0
        while true {
            let (rmeta, body) = await kernel.sendWithBinary(
                AgentId(pyStr(src)),
                .object([
                    "type": .string("read_stream"), "path": spath,
                    "offset": .integer(offset), "length": .integer(length),
                ]), Data())
            // python: the reply must be a dict carrying a `bytes` chunk. On this
            // channel the chunk rides beside the header, so a reply that carries
            // an `error` (or is not an object at all) is the same refusal.
            guard case .object(let ro) = rmeta, ro["error"] == nil else {
                return Refusal.make(
                    Refusal.transportError,
                    "pump: read from \(pyRepr(src)) failed: \(pyStr(rmeta))")
            }
            let (wmeta, _) = await kernel.sendWithBinary(
                AgentId(pyStr(sink)),
                .object([
                    "type": .string("write_stream"), "path": dpath,
                    "offset": .integer(offset), "truncate": .bool(first),
                ]), body)
            guard case .object(let wo) = wmeta, !pyTruthy(wo["error"] ?? .null) else {
                return Refusal.make(
                    Refusal.transportError,
                    "pump: write to \(pyRepr(sink)) failed: \(pyStr(wmeta))")
            }
            first = false
            chunks += 1
            let next = rmeta["next_offset"]
            if pyTruthy(rmeta["eof"]) {
                if case .integer(let n) = next { offset = n }
                break
            }
            guard case .integer(let n) = next, n > offset else {
                // No forward progress and no eof: a SOURCE that would loop forever.
                return Refusal.make(
                    Refusal.transportError,
                    "pump: source \(pyRepr(src)) made no progress at offset \(offset)")
            }
            offset = n
        }
        return .object([
            "source": spath, "sink": dpath,
            "bytes": .integer(offset), "chunks": .integer(chunks),
        ])
    }

    /// python's `readme.md`, verbatim.
    static let readmeText = """
        # file_bridge — filesystem edge

        Verbs: `read(path)` · `write(path,content)` · `list(path?)` · `delete(path)` · `rename(old,new)` · `mkdir(path)` · `reflect`
        **Streams** (the SOURCE/SINK protocol — file_bridge is the fs implementor):
        - `read_stream(path, offset=0, length=65536)` → `{bytes, next_offset, eof, size}` — `bytes` is the RAW chunk (rides the transport as raw bytes — web_ws / bridge binary frame, never base64); pull chunks until `eof` (stateless cursor).
        - `write_stream(path, bytes, offset?, truncate?)` → push RAW chunks (`bytes` = one raw chunk; append by default; `truncate:true` on the first to start fresh). Text files use the whole-file `write`.
        - `pump(source?, source_path, sink?, sink_path?, chunk=65536)` → server-side SOURCE→SINK copy in ONE call (vs driving each chunk over the wire). Both ends bound by id + the stream verbs, each SELF-gating — so any provider (a `network_bridge` SOURCE, a `file_bridge` SINK) composes. Omitted `source`/`sink` = this bridge.
        A consumer (file serving · a pump · kernel_state) is **storage-agnostic** — it pulls/pushes by id, so a `network_bridge` answering the same verbs serves a *remote* file the same way (G1).

        **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
        **Running-dir law**: root is clamped inside the kernel's cwd — `../`, `~`, absolute paths
        outside, and outward symlinks all refuse. Even when open, the edge never leaves the dir.

        """
}

/// The `not_found` refusal python emits (`{error, reason:"not_found", path}`).
func notFound(_ error: String, path: String) -> JSON {
    Refusal.make(Refusal.notFound, error, extra: [("path", .string(path))])
}
