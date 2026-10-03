// Filesystem-as-agent bundle — the fs-transport member of the io family.
//
// Mirrors py `file_bridge.tools` (the spec) and rust `fantastic-file`. This
// agent is the GATE (the ingress rule) + the verb shape over `Fs.swift`, the one
// disk surface; it never touches the disk itself. Each agent owns a `root`
// directory (from agent meta) and a `readonly` flag; every verb resolves its
// path within `root` through `fsResolve`, which refuses `..`, an absolute path
// and any symlink pointing out of the root.
//
// Verbs: reflect / boot / list / read / write / edit / append / undo /
//        read_stream / write_stream / pump / delete / rename / mkdir /
//        allow / disallow / allowed. The text verbs live in `TextVerbs.swift`.
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
    public var verbs: [String] { Array((Self.verbDocs.asObject ?? [:]).keys) }

    /// python's module-level `VERBS` — ONE table: reflect renders it and
    /// `advertises` reads it.
    static let verbDocs: JSON = [
        "reflect":
            "Identity + filesystem root + readonly flag + auth posture (ingress/egress/sealed/see). No args.",
        "boot":
            "No-op. Returns None.",
        "list":
            "args: path:str (default ''). Returns {path, files:[{name, path, type, size?}, ...]} — the exposed entries and the directories on the way to one.",
        "read":
            "args: path:str (req), offset:int=0 (line), limit:int=200 — a text page {path, lines:[{n,text}], total, eof, version, origin}; refuses binary.",
        "write":
            "args: path:str (req), bytes (req — the whole document; str is utf-8-encoded), if_match:str? — atomic whole-document write; {path, size, version}.",
        "edit":
            "args: edits:[{path:str, hunks:int, if_match:str?}] (req — even for ONE file), bytes (req — ALL hunks concatenated; str is utf-8-encoded), dry_run:bool? — a TRANSACTION: every hunk of every edit verified before any write, any failure writes nothing (or restores what was written); {edited:true, edits:[{path,hunks,size,version}]}.",
        "append":
            "args: path:str (req), bytes (req — one chunk; str is utf-8-encoded) — appends (creates if absent); {path, size, version}.",
        "undo":
            "args: path:str (req) — swap the document with the bytes the last write/edit/append replaced (a second undo is redo); ONE slot per path, in RAM, lost on restart; {undone:true, path, version|deleted}.",
        "read_stream":
            "args: path:str (req), offset:int=0, length:int=65536. The SOURCE half — returns ONE chunk {path, offset, bytes, next_offset, eof, size}.",
        "write_stream":
            "args: path:str (req), bytes:bytes (req — ONE raw chunk; a model writes it VERBATIM between `<bytes>` and `</bytes>`, nothing escaped), offset:int? (default = append at end), truncate:bool=False. The SINK half — a producer pushes RAW chunks by offset.",
        "pump":
            "The PUMP — a server-side stream copy SOURCE→SINK, chunk by chunk, in ONE call.",
        "delete":
            "args: path:str (req), recursive:bool=False. Returns {path, deleted:true}.",
        "rename":
            "args: old_path:str, new_path:str (both req). Returns {old_path, new_path}. Creates new parent dirs. Refused if readonly.",
        "mkdir":
            "args: path:str (req). Returns {path, created:true}. Recursive (parents ok). Refused if readonly.",
        "allow":
            "args: path:str (req — a file, or a directory exposing everything under it; `.` = the whole root). Adds it to the `allow` list (idempotent). Returns {path, allowed:true, count}.",
        "disallow":
            "args: path:str (req — an entry of the `allow` list). Removes it. Returns {path, allowed:false, count}.",
        "allowed":
            "args: limit:int?, after:str? (the last path of the previous page). The `allow` list in path order. Returns {paths, next, total}.",
    ] as JSON

    /// The text verbs' RAM state — per bundle instance, keyed by kernel (see
    /// `UndoSlots`).
    let undoSlots = UndoSlots()
    let docLocks = DocumentLocks()

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
        if let a = meta["allow"] {
            let ok = a.asArray.map { items in
                items.allSatisfy {
                    guard let s = $0.asString else { return false }
                    return !s.isEmpty && !s.hasPrefix("/") && allowNorm(s) != nil
                }
            } ?? false
            if !ok {
                return "allow must be a list of paths relative to the root (a file, or a "
                    + "directory exposing everything under it; `.` = the whole root)"
            }
        }
        // The SPEC as written, python's `{spec!r}` — naming the resolved rule
        // instead hid the typo the operator has to find.
        if let bad = IngressRules.unknownRefusal(
            meta, legal: ["allow_all_ingress", "deny_ingress"])
        {
            return bad
        }
        return passwordRefusal(meta, edge: "fs edge")
    }

    /// The verbs this bundle serves, in python's `VERBS` order.
    static let verbs: [String] = [
        "reflect", "boot", "list", "read", "write", "edit", "append", "undo",
        "read_stream", "write_stream", "pump", "delete", "rename", "mkdir", "allow", "disallow", "allowed",
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
        if verb == "reflect" { return reflect(agent: agent, base: kernel.workdir?.path) }
        guard let base = kernel.workdir?.path else { return Self.noRunningDir() }
        // DENY-ALL AT THE PATH LEVEL — after each verb's own argument checks
        // (python's order), before any disk access.
        if let refused = exposure(verb, agent: agent, payload: payload, base: base) {
            return refused
        }
        switch verb {
        case "allow", "disallow", "allowed":
            return allowVerb(verb, agent: agent, payload: payload, base: base, kernel: kernel)
        case "boot":
            return .null
        case "list":
            return keepExposed(
                listFiles(agent: agent, payload: payload, base: base), agent: agent, base: base)
        case "delete":
            return deleteFile(agent: agent, payload: payload, base: base)
        case "rename":
            return renameFile(agent: agent, payload: payload, base: base)
        case "mkdir":
            return makeDir(agent: agent, payload: payload, base: base)
        case "read":
            return textRead(agent, payload, base: base)
        case "write":
            return await textWrite(agent, payload, blob: nil, base: base, kernel: kernel)
        case "edit":
            return await textEdit(agent, payload, blob: nil, base: base, kernel: kernel)
        case "append":
            return await textAppend(agent, payload, blob: nil, base: base, kernel: kernel)
        case "undo":
            return await textUndo(agent, payload, base: base, kernel: kernel)
        // THE STREAM VERBS ON THE TEXT CHANNEL, as python serves them: a model
        // writes a document VERBATIM between `<bytes>` and `</bytes>` and the
        // tag grammar delivers it as utf-8 bytes to the same handler; a chunk
        // read back renders utf-8 in the reply. Swift refused both here with
        // "raw bytes on the binary channel" — the `subai_graph` routine's
        // program write failed on it every pass (measured 2026-09-13, the same
        // refusal rust carried in 43 e2e logs). The binary channel stays for
        // real blobs.
        case "read_stream":
            let (reply, body) = readStream(agent: agent, payload: payload, base: base)
            guard case .object(var o) = reply, o["error"] == nil else { return reply }
            // The chunk fills its own slot (python's key order: path, offset,
            // bytes, next_offset, eof, size, origin).
            o["bytes"] = .string(String(decoding: body, as: UTF8.self))
            return .object(o)
        case "write_stream":
            if pyTruthy(agent.metaValue(forKey: "readonly") ?? .null) { return readonlyRefusal() }
            // `bytes` ON THE TEXT CHANNEL IS NEVER A CHUNK (A13.19). Python
            // TYPES the argument — `isinstance(data, (bytes, bytearray))` — and
            // JSON has no bytes, so ANY present value here (a string, a number,
            // a null) is `missing_arg` and nothing is written. A raw chunk
            // reaches this bundle on the BINARY channel only; that is the
            // carrier a model's `<bytes>` document rides too. ABSENT is python's
            // `payload.get("bytes", b"")` — an empty chunk, a legal write.
            guard payload.asObject?["bytes"] == nil else {
                return Refusal.missing("write_stream", "bytes (one raw chunk)")
            }
            return writeStream(agent: agent, payload: payload, blob: Data(), base: base)
        case "pump":
            // The PUMP coordinates a SOURCE→SINK copy over the binary channel
            // (it never touches bytes itself), so it rides the text channel.
            return await pump(agent: agent, payload: payload, kernel: kernel)
        default:
            return nil
        }
    }

    /// Binary channel: `read_stream` (no request body → reply BODY = one raw
    /// chunk) and `write_stream` (the body at `bytes` = one raw chunk → status
    /// reply, no body). Symmetric with py/rust's `bytes`-in-the-dict; never
    /// base64. Every verb reads the body at ITS key, `bytes` — python's
    /// `payload.get("bytes")` — so a body placed under another key is not a
    /// chunk. Same sealed-by-default gate + running-dir clamp as the text
    /// channel. Any other verb routes through the text `handle` (its body is
    /// unused).
    public func handleBinary(
        agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
    ) async throws -> (JSON?, Blob?) {
        let verb = header["type"].asString ?? ""
        let chunk = bodyAt(body, "bytes")
        guard verb == "read_stream" || verb == "write_stream" || Self.textBodyVerbs.contains(verb)
        else {
            let reply = try await handle(agentId: agentId, payload: header, kernel: kernel)
            return (reply, nil)
        }
        guard let agent = kernel.agent(agentId) else {
            return (.object(["error": .string("no agent \(agentId)")]), nil)
        }
        // GATE — same sealed-by-default choke point as the text channel.
        if let denied = gateVerb(
            ingressRule: agent.metaValue(forKey: "ingress_rule"),
            auth: agent.metaValue(forKey: "auth"),
            authToken: nil,
            agentId: agent.id.value, verb: verb)
        {
            return (denied, nil)
        }
        guard let base = kernel.workdir?.path else { return (Self.noRunningDir(), nil) }
        // THE DOCUMENT RIDES THE BODY: `write`/`edit`/`append` take the body at
        // `bytes` verbatim, as python's handler receives it from the binary
        // frame — present even when empty, the bytes-typed carrier a model's
        // `<bytes>` document rides.
        switch verb {
        case "write":
            return (await textWrite(agent, header, blob: chunk, base: base, kernel: kernel), nil)
        case "edit":
            return (await textEdit(agent, header, blob: chunk, base: base, kernel: kernel), nil)
        case "append":
            return (await textAppend(agent, header, blob: chunk, base: base, kernel: kernel), nil)
        default: break
        }
        if let refused = exposure(verb, agent: agent, payload: header, base: base) {
            return (refused, nil)
        }
        if verb == "read_stream" {
            let (reply, chunk) = readStream(agent: agent, payload: header, base: base)
            // A REFUSAL CARRIES NO CHUNK. A success does — a 0-byte one
            // included, which is how an empty file reads back as octet instead
            // of JSON (C37/A13.1).
            guard case .object(let o) = reply, !pyTruthy(o["error"] ?? .null) else {
                return (reply, nil)
            }
            // the chunk sits at the reply's `bytes` key, as python's does
            return (reply, Blob("bytes", chunk))
        }
        if pyTruthy(agent.metaValue(forKey: "readonly") ?? .null) {
            return (readonlyRefusal(), nil)
        }
        // THE BODY AT `bytes` IS THE CHUNK, and a header `bytes` key of ANY JSON
        // value is a wire client trying to smuggle one through the envelope —
        // python OVERWRITES `payload[field]` with the body, so the key never
        // reaches this handler holding a JSON value (A13.1). Refuse it rather
        // than write whatever it spelled. Nothing at `bytes` (no body, or a body
        // under another key) is python's default `b""`: `written: 0`.
        if header.asObject?["bytes"] != nil {
            return (Refusal.missing("write_stream", "bytes (one raw chunk)"), nil)
        }
        return (writeStream(agent: agent, payload: header, blob: chunk ?? Data(), base: base), nil)
    }

    // MARK: - Verbs

    /// python `_reflect`: identity + the clamped root + the readonly flag + the
    /// auth posture. NEVER refuses — a root that escapes shows the configured
    /// string plus `root_error`.
    private func reflect(agent: Agent, base: String?) -> JSON {
        let root = rootStr(agent)
        var rootDisplay: String
        var rootError: String? = nil
        if let base {
            do {
                rootDisplay = try fsResolveRoot(root, base: base)
            } catch {
                rootDisplay = root
                rootError = error.message
            }
        } else {
            rootDisplay = root
            rootError = Self.noRunningDirText
        }
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(agent.id.value),
            "sentence": .string(
                "Filesystem edge of the io family — sealed by default, running-dir only."),
            "root": .string(rootDisplay),
            "readonly": .bool(pyTruthy(agent.metaValue(forKey: "readonly") ?? .null)),
            "allow_count": .integer(Int64(allowList(agent).count)),
            "edit_grammar": .string(Self.editGrammar),
            "compose": .string(Self.compose),
            "verbs": Self.verbDocs,
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
        let entries: [String]
        do {
            if !(try fsExists(root, relative, base: base)) {
                return notFound("path \(pyRepr(relative)) does not exist", path: relative)
            }
            if !(try fsIsDir(root, relative, base: base)) {
                return Refusal.bad("list", "path", "a directory; \(pyRepr(relative)) is not one")
            }
            entries = try fsListDir(root, relative, base: base)
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        var files: [JSON] = []
        for entry in entries {
            let entryName = String(entry[entry.index(after: entry.lastIndex(of: "/")!)...])
            // The entry's path AS A CALLER NAMES IT — under the listed dir, links not
            // followed (python `posixpath.join(_norm(path) or ".", entry.name)`), so
            // an exposed symlinked dir lists what it serves.
            let dir = allowNorm(relative) ?? "."
            let rel = dir == "." ? entryName : "\(dir)/\(entryName)"
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
        return .object([
            "path": .string(relative), "files": .array(files),
            // HOW TO GET THIS AGAIN — python `_list`'s `origin`.
            "origin": .string(
                "\(agent.id.value) list \(relative) — list {path: \(relative)}; changes as files change"),
        ])
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

    /// python `_root_str(rec)` — `rec.get("root", "") or ""`, as the string
    /// `fs.resolve_root` clamps (`str(root or "")`).
    func rootStr(_ agent: Agent) -> String {
        let raw = agent.metaValue(forKey: "root") ?? .null
        return pyTruthy(raw) ? pyStr(raw) : ""
    }

    /// The clamp base has ONE source — `Kernel.workdir`, the dir holding the
    /// root's `.fantastic` (audit 2026-09-28, c6). None ⇒ every disk verb refuses.
    static let noRunningDirText = "kernel has no root — no running dir to clamp disk to"

    static func noRunningDir() -> JSON {
        Refusal.make(Refusal.transportError, noRunningDirText)
    }

    /// python `_reflect`'s `edit_grammar`, verbatim.
    static let editGrammar =
        "one or more blocks: `<<<<<<< SEARCH` / <old lines> / `=======` / "
        + "<new lines> / `>>>>>>> REPLACE`; each SEARCH must match its "
        + "document EXACTLY ONCE. `edit` is a TRANSACTION over "
        + "`edits:[{path, hunks, if_match?}]` — even one file rides the "
        + "array; the blob holds every edit's hunks concatenated in `edits` "
        + "order, split by the per-edit `hunks` count; any failure anywhere "
        + "writes nothing"

    /// python `_reflect`'s `compose`, verbatim.
    static let compose =
        "every content-carrying verb (`write`, `edit`, `append`) takes its "
        + "document in `bytes`, and `bytes` is VERBATIM: write the document "
        + "between `<bytes>` and `</bytes>` exactly as it should land — real "
        + "newlines, real quotes, nothing escaped, no fence — then close the "
        + "call. The document ends at the first `</bytes>`. An edit names its "
        + "files as tags: <edits><item><path>a.py</path><hunks>2</hunks></item></edits>"

    /// The bridge is going away — its undo slots go with it (python `on_delete`).
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        undoSlots.clear(kernel, bridge: agentId.value)
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
                // python's reply holds the chunk HERE. The chunk rides beside
                // the reply (the outcome's body), so its slot holds `null` —
                // the head python's `encode_frame` writes (R-D16-3).
                "bytes": .null,
                "next_offset": .integer(nextOffset),
                "eof": .bool(nextOffset >= size),
                "size": .integer(size),
                // HOW TO GET THIS CHUNK AGAIN — python `_read_stream`'s `origin`.
                "origin": .string(
                    "\(agent.id.value) \(path) bytes \(offset)-\(nextOffset) of \(size) — "
                        + "read_stream {path: \(path), offset: \(offset), length: \(chunk.count)}"),
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
    ///
    /// BOTH ENDS RUN AS THE CALLER (audit AV-7, 2026-10-03): sent from this
    /// dispatch, both ends judged THIS BRIDGE — a client holding only `pump`
    /// copied a file it could never read and overwrote one it could never write.
    /// `read_stream` on the source and `write_stream` on the sink now go out under
    /// the caller's identity (`asCaller`), so the caller needs its own grants on
    /// both. A pump ONTO ITS OWN SOURCE (the sink is the source and `sink_path`
    /// names the same file) is refused.
    private func pump(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        let selfId = JSON.string(agent.id.value)
        // python's `or` fallbacks: a FALSY value takes the default, whatever
        // its type (`""`, `0`, `[]` all fall through).
        let src = pyTruthy(payload["source"]) ? payload["source"] : selfId
        let sink = pyTruthy(payload["sink"]) ? payload["sink"] : selfId
        let spath = pyTruthy(payload["source_path"]) ? payload["source_path"] : .string("")
        let dpath = pyTruthy(payload["sink_path"]) ? payload["sink_path"] : spath
        let length: Int64
        do { length = try intArg("pump", payload, "chunk", default: 65536, minimum: 1) } catch {
            return error.json
        }
        if src == sink && pumpOneFile(spath, dpath) {
            // The first write's `truncate` empties the file the next read comes
            // from: everything past the first chunk was lost under a success reply
            // — the bare `{source_path}` form did it, sink and sink_path defaulted
            // (python `_pump`, audit BV-7).
            return Refusal.bad(
                "pump", "sink_path",
                "another file than source_path when the sink is the source — a pump "
                    + "onto its own source truncates it after the first chunk")
        }
        var offset: Int64 = 0
        var first = true
        var chunks: Int64 = 0
        while true {
            // EACH CHUNK IS AN EXECUTION — python sends every read and write
            // through `kernel.send`, so each one has its call row, settle row,
            // `ps` row and ledger id. The binary item is the carrier for the raw
            // chunk python's dicts hold natively.
            let readItem = Item(
                AgentId(pyStr(src)),
                .object([
                    "type": .string("read_stream"), "path": spath,
                    "offset": .integer(offset), "length": .integer(length),
                ]), rawReply: true)
            let read = await asCaller { await kernel.send([readItem])[0] }
            let rmeta = read.result
            // python: the reply must be a dict carrying a `bytes` chunk. The
            // chunk is the outcome's BODY at `bytes`, so no body there is the
            // same refusal.
            guard case .object = rmeta, let body = bodyAt(read.body, "bytes") else {
                return Refusal.make(
                    Refusal.transportError,
                    "pump: read from \(pyRepr(src)) failed: \(pyStr(rmeta))")
            }
            let writeItem = Item(
                AgentId(pyStr(sink)),
                .object([
                    "type": .string("write_stream"), "path": dpath,
                    "offset": .integer(offset), "truncate": .bool(first),
                ]), binary: Blob("bytes", body))
            let wmeta = await asCaller { await kernel.send([writeItem])[0].result }
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

    /// Run `body`'s sends AS THIS DISPATCH'S CALLER: the operator stays the
    /// operator; a local agent is itself; a foreign `<guid>|<agent>` is the same
    /// pair its bridge leg relayed (the leg as sender, the foreign id judged by
    /// the target's `callable_by`). Python `_as_caller`.
    private func asCaller<T: Sendable>(_ body: () async -> T) async -> T {
        guard let origin = KernelTaskLocals.currentOrigin else {
            return await KernelTaskLocals.$currentSender.withValue(nil) { await body() }
        }
        if let remote = KernelTaskLocals.currentRemote {
            return await KernelTaskLocals.$currentSender.withValue(origin) {
                await KernelTaskLocals.withRemoteCaller(leg: origin, address: remote) {
                    await body()
                }
            }
        }
        return await KernelTaskLocals.$currentSender.withValue(origin) { await body() }
    }

    // MARK: - The allow list (what this bridge exposes)
    //
    // DENY-ALL AT THE PATH LEVEL — python `file_bridge` is canonical. The record's
    // `allow` list names what the bridge EXPOSES, relative to `root`: a file, or
    // everything under a directory (`.` = the whole root). No list exposes
    // NOTHING. Every path verb sees only exposed paths; anything else is
    // `not_found` whether or not it exists, and `list` also shows the directories
    // on the way to one.

    func allowList(_ agent: Agent) -> [String] {
        (agent.metaValue(forKey: "allow")?.asArray ?? []).compactMap { $0.asString.flatMap(allowNorm) }
    }

    /// The ONE answer for a path outside the list — the same whether it exists or not.
    func unexposed(_ verb: String, _ path: String) -> JSON {
        Refusal.make(
            Refusal.notFound, "\(verb): \(pyRepr(path)) is not exposed by this bridge",
            hint: "expose it first — `allow {path}` (whoever holds that grant)",
            extra: [("path", .string(path))])
    }

    /// Where `path` actually LANDS (symlinks followed) must be under the RESOLVED
    /// form of an allow entry — entries resolved now, because the disk changes.
    /// With `allowExposed` (the path as asked) this is the both-ends rule: a link
    /// inside an exposed area that points outside it serves nothing (audit
    /// 2026-09-28). Python `_lands_exposed`.
    func landsExposed(root: String, allow: [String], path: String, base: String) -> Bool {
        guard let target = try? fsResolve(root, path, base: base) else { return false }
        for s in allow {
            // an entry that itself leads out of the root exposes nothing
            guard let entry = try? fsResolve(root, s, base: base) else { continue }
            if target == entry || target.hasPrefix(entry.hasSuffix("/") ? entry : entry + "/") {
                return true
            }
        }
        return false
    }

    /// THE CLAMP FIRST, then the list, on BOTH ends — python `_gate_path`.
    func gatePath(_ verb: String, root: String, allow: [String], path: String, base: String) -> JSON? {
        do {
            _ = try fsResolve(root, path, base: base)
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        // NOT a ternary with `nil`: `JSON` is ExpressibleByNilLiteral, so `? nil :`
        // types as `JSON.null` and every EXPOSED path answered `null`.
        if allowExposed(allow, path) && landsExposed(root: root, allow: allow, path: path, base: base) {
            return Optional<JSON>.none
        }
        return unexposed(verb, path)
    }

    /// The exposure check for one path verb, where python places it: after that
    /// verb's own argument checks (an argument fault answers first — nil lets the
    /// verb report it), before any disk access.
    func exposure(_ verb: String, agent: Agent, payload: JSON, base: String) -> JSON? {
        let allow = allowList(agent)
        let root = rootStr(agent)
        let readonly = pyTruthy(agent.metaValue(forKey: "readonly") ?? .null)
        switch verb {
        case "list":
            guard let p = try? pathArg("list", payload, "path") else { return nil }
            if allowLeadsTo(allow, p) { return nil }
            return gatePath("list", root: root, allow: allow, path: p, base: base)
        case "read_stream":
            guard let p = try? pathArg("read_stream", payload, "path"),
                (try? intArg("read_stream", payload, "offset", default: 0, minimum: 0)) != nil,
                (try? intArg("read_stream", payload, "length", default: 65536, minimum: 1)) != nil
            else { return nil }
            return gatePath("read_stream", root: root, allow: allow, path: p, base: base)
        case "write_stream":
            if readonly { return nil }
            guard let p = try? pathArg("write_stream", payload, "path") else { return nil }
            return gatePath("write_stream", root: root, allow: allow, path: p, base: base)
        case "delete", "mkdir":
            if readonly { return nil }
            guard let p = try? targetArg(verb, root, payload, "path", base: base) else { return nil }
            return gatePath(verb, root: root, allow: allow, path: p, base: base)
        case "rename":
            if readonly { return nil }
            guard let old = try? targetArg("rename", root, payload, "old_path", base: base),
                let new = try? targetArg("rename", root, payload, "new_path", base: base)
            else { return nil }
            return gatePath("rename", root: root, allow: allow, path: old, base: base)
                ?? gatePath("rename", root: root, allow: allow, path: new, base: base)
        default:
            return nil
        }
    }

    /// `list` shows only exposed entries (both ends) and the directories on the
    /// way to one.
    func keepExposed(_ reply: JSON, agent: Agent, base: String) -> JSON {
        guard case .object(var o) = reply, let files = o["files"]?.asArray else { return reply }
        let allow = allowList(agent)
        let root = rootStr(agent)
        o["files"] = .array(
            files.filter { f in
                let p = f["path"].asString ?? ""
                return (allowExposed(allow, p)
                    && landsExposed(root: root, allow: allow, path: p, base: base))
                    || (f["type"].asString == "dir" && allowLeadsTo(allow, p))
            })
        return .object(o)
    }

    /// `allow` / `disallow` / `allowed` — edit and read the list one path at a
    /// time. Record state: edited through `Kernel.update`, persisted by the loader.
    func allowVerb(_ verb: String, agent: Agent, payload: JSON, base: String, kernel: Kernel) -> JSON {
        // ONE read-modify-write at a time: two concurrent `allow`s read the same
        // snapshot and the second update silently dropped the first (audit d3).
        Self.allowLock.lock()
        defer { Self.allowLock.unlock() }
        var allow = allowList(agent)
        func save() { kernel.update(agent.id, ["allow": .array(allow.map { .string($0) })]) }
        switch verb {
        case "allow":
            let raw: String
            do { raw = try pathArg("allow", payload, "path") } catch { return error.json }
            if raw.isEmpty { return Refusal.missing("allow", "path") }
            guard let path = allowNorm(raw) else {
                return Refusal.bad("allow", "path", "a path relative to the bridge's root")
            }
            do {
                _ = try fsResolve(rootStr(agent), path, base: base)
            } catch {
                return Refusal.bad("allow", "path", "a path inside the root (\(error.message))")
            }
            if !allow.contains(path) {
                allow.append(path)
                save()
            }
            return .object(["path": .string(path), "allowed": true, "count": .integer(Int64(allow.count))])
        case "disallow":
            let raw: String
            do { raw = try pathArg("disallow", payload, "path") } catch { return error.json }
            if raw.isEmpty { return Refusal.missing("disallow", "path") }
            guard let path = allowNorm(raw), allow.contains(path) else {
                return notFound("disallow: \(pyRepr(raw)) is not in the allow list", path: raw)
            }
            allow.removeAll { $0 == path }
            save()
            return .object(["path": .string(path), "allowed": false, "count": .integer(Int64(allow.count))])
        default:
            var paths = allow.sorted()
            if let after = payload["after"].asString, !after.isEmpty {
                paths = paths.filter { $0 > after }
            }
            let total = paths.count
            let limit: Int64
            do {
                limit = try intArg("allowed", payload, "limit", default: 0, minimum: 0)
            } catch { return error.json }
            let page = limit > 0 ? Array(paths.prefix(Int(limit))) : paths
            let next: JSON = (limit > 0 && total > page.count) ? .string(page.last ?? "") : .null
            return .object([
                "paths": .array(page.map { .string($0) }), "next": next,
                "total": .integer(Int64(total)),
            ])
        }
    }

    private static let allowLock = NSLock()

    /// python's `readme.md`, verbatim.
    static let readmeText = """
        # file_bridge — filesystem edge

        Verbs: `read(path,offset?,limit?)` · `write(path,bytes,if_match?)` · `edit(edits,bytes,dry_run?)` · `append(path,bytes)` · `undo(path)` · `read_stream(path,offset?,length?)` · `write_stream(path,bytes,offset?,truncate?)` · `pump(source_path,sink?,sink_path?)` · `list(path?)` · `delete(path)` · `rename(old,new)` · `mkdir(path)` · `allow(path)` · `disallow(path)` · `allowed(limit?,after?)` · `reflect`
        **Streams** (the SOURCE/SINK protocol — file_bridge is the fs implementor):
        - `read_stream(path, offset=0, length=65536)` → `{bytes, next_offset, eof, size}` — `bytes` is the RAW chunk (rides the transport as raw bytes — web_ws / bridge binary frame, never base64); pull chunks until `eof` (stateless cursor).
        - `write_stream(path, bytes, offset?, truncate?)` → push RAW chunks (`bytes` = one raw chunk; append by default; `truncate:true` on the first to start fresh). Text files use the whole-file `write`.
        - `pump(source?, source_path, sink?, sink_path?, chunk=65536)` → server-side SOURCE→SINK copy in ONE call (vs driving each chunk over the wire). Both ends bound by id + the stream verbs, each SELF-gating — so any provider (a `network_bridge` SOURCE, a `file_bridge` SINK) composes. Omitted `source`/`sink` = this bridge. **Both ends run AS YOU** (the pump's caller), never as this bridge: you need your own `read_stream` grant on the source and `write_stream` on the sink — `pump` alone reaches nothing this bridge can reach and you cannot.
        **Text documents** (any format — the bridge imposes none):
        - `read(path, offset=0, limit=200)` → a line-numbered page `{lines:[{n,text}], total, eof, version}`; refuses binary.
        - `write(path, bytes, if_match?)` → atomic whole-document write; `append(path, bytes)` → one chunk at the end.
        - `edit(edits:[{path, hunks, if_match?}], bytes)` → SEARCH/REPLACE hunks (`reflect` shows the grammar), a TRANSACTION over every listed file: any failure writes nothing.
        - `version` = the document's hash; `if_match` refuses a write when the document changed since you read it.
        - `undo(path)` → swaps the document with what the last write/edit/append replaced (a second `undo` is redo). ONE slot per path, in RAM — lost on restart, nothing kept on disk.

        A consumer (file serving · a pump · kernel_state) is **storage-agnostic** — it pulls/pushes by id, so a `network_bridge` answering the same verbs serves a *remote* file the same way (G1).

        **Deny-all at the path level** — the `allow` list on the record names what this bridge EXPOSES, relative to `root`: a file, or everything under a directory (`.` = the whole root). No list, or an empty one, exposes NOTHING. Every path verb sees only exposed paths; anything else is `not_found` whether or not it exists (nothing leaks past the list), and `list` also shows the directories on the way to an exposed path. BOTH ends count: the path as asked AND the file it lands on (symlinks followed) must be inside the list — a link that stays inside serves, a link pointing out of it serves nothing. Set it at create (`allow=["docs/a.pdf", "img"]`) or one path at a time with `allow {path}` / `disallow {path}` (gated like any verb); `allowed` pages it. Writes are still governed by `readonly`. Writing an exposed path creates its missing parent directories, even ones the list does not name (`allow:["a/b/c.txt"]` + `write a/b/c.txt` creates `a/` and `a/b/`); `list` shows them as the way to it, nothing else in them is exposed.

        **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
        **Running-dir law**: root is clamped inside the kernel's running dir (the dir holding its root's `.fantastic`) — `../`, `~`, absolute paths
        outside, and outward symlinks all refuse. Even when open, the edge never leaves the dir.

        """
}

/// The `not_found` refusal python emits (`{error, reason:"not_found", path}`).
func notFound(_ error: String, path: String) -> JSON {
    Refusal.make(Refusal.notFound, error, extra: [("path", .string(path))])
}

/// python `posixpath.normpath(path or ".")` of a ROOT-RELATIVE path; nil when it
/// is absolute or climbs out.
func allowNorm(_ path: String) -> String? {
    if path.hasPrefix("/") { return nil }
    var parts: [Substring] = []
    for seg in path.split(separator: "/", omittingEmptySubsequences: false) {
        switch seg {
        case "", ".": continue
        case "..":
            if parts.isEmpty { return nil }
            parts.removeLast()
        default: parts.append(seg)
        }
    }
    return parts.isEmpty ? "." : parts.joined(separator: "/")
}

/// Do two pump paths name ONE file — equal once normalised (`./x` is `x`)?
/// python `_one_file`: only a root-relative path to something inside the root is
/// judged; anything else (absent, not a string, absolute, climbing out, the root
/// itself) is its end's to refuse.
func pumpOneFile(_ a: JSON, _ b: JSON) -> Bool {
    guard let a = a.asString, let b = b.asString, let n = allowNorm(a), n != "." else {
        return false
    }
    return n == allowNorm(b)
}

/// Is `path` exposed — a listed path, or under a listed directory?
func allowExposed(_ allow: [String], _ path: String) -> Bool {
    guard let p = allowNorm(path) else { return false }
    return allow.contains { $0 == "." || $0 == p || p.hasPrefix($0 + "/") }
}

/// Is `path` a directory on the way to an exposed path? (`list` shows it.)
func allowLeadsTo(_ allow: [String], _ path: String) -> Bool {
    guard let p = allowNorm(path), !allow.isEmpty else { return false }
    return p == "." || allow.contains { $0.hasPrefix(p + "/") }
}
