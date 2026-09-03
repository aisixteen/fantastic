// Filesystem-as-agent bundle — the fs-transport member of the io family.
//
// Mirrors py `file_bridge.tools` (the spec) and rust `fantastic-file`. Each
// agent owns a `root` directory (from agent meta) and a `readonly` flag;
// every verb resolves the supplied path within `root` and refuses if the
// result escapes — LEXICALLY (`..`, absolute) and CANONICALLY (an in-root
// symlink pointing outside).
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

/// Default hidden-file names hidden from `list`. Matches py/rust so
/// cross-runtime workdirs stay symmetric.
let DEFAULT_HIDDEN: Set<String> = [
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

/// The `not_found` refusal shape python emits (`{error, reason:"not_found",
/// path}`). Python's closed set carries `NOT_FOUND`; swift's `Refusal.codes`
/// does not yet, and `Refusal.make` TRAPS on a code outside its set — so the
/// shape is spelled here until the kernel adds the constant. Same code, same
/// keys as python; nothing else differs.
func notFound(_ error: String, path: String) -> JSON {
    .object([
        "error": .string(error),
        "reason": .string("not_found"),
        "path": .string(path),
    ])
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
            return "\(field)=\(spec.serialize()) is not applicable to the \(edge): "
                + "a leg with no wire envelope can never present a token — "
                + "legal values: allow_all_ingress, deny_ingress"
        }
    }
    return nil
}

/// Clamp `raw` under `base`, or throw. THE running-dir law, exported.
///
/// Mirror of rust's `fantastic_file::clamp_root`. Public because the cold
/// bring-up readers need it too: `static_loader` reads its manifest before any
/// file_bridge agent can exist, and that read is NOT exempt from the clamp —
/// python does the same through `fs.read_text(Path.cwd(), path)`.
///
/// Compared LEXICALLY (no canonicalize) so the check does not require the path
/// to exist, and so both sides stay in the same form — canonicalizing only the
/// base would mismatch a non-canonical absolute root on macOS (`/var` vs
/// `/private/var`). The per-path symlink check is `FileBundle.resolvePath`,
/// which canonicalizes BOTH sides of the deepest existing ancestor.
public func clampRoot(_ raw: String, base: URL) throws -> URL {
    struct Escape: Error, CustomStringConvertible {
        let raw: String
        var description: String { "path \(raw) escapes the running directory" }
    }
    // Absoluteness is a property of the STRING, not of the URL:
    // `URL(fileURLWithPath:)` absolutizes a relative path against the CWD, so
    // asking the URL always says "absolute" and the base would be discarded —
    // a relative `agents/x/agent.json` then resolved against the cwd instead of
    // the store root and was refused as an escape.
    let combined =
        raw.hasPrefix("/")
        ? URL(fileURLWithPath: raw).standardizedFileURL
        : base.appendingPathComponent(raw).standardizedFileURL
    let root = base.standardizedFileURL.path
    if combined.path == root || combined.path.hasPrefix(root + "/") { return combined }
    throw Escape(raw: raw)
}

/// A refusal carried as an error, so argument helpers compose with `try`.
struct Refused: Error {
    let json: JSON
}

/// A string argument that is a ROOT-RELATIVE path: absent/null ⇒ `""`; a
/// non-string or an absolute path is `bad_arg` (the absolute form is refused,
/// never silently re-rooted — python used to `lstrip("/")` against its own
/// docstring; #117).
func pathArg(_ verb: String, _ payload: JSON, _ name: String) throws -> String {
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
    throws -> Int64
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

    public var readme: String? {
        "file_bridge — the gated filesystem edge of the io family. Each agent owns `root` (clamped"
            + " INSIDE the running dir — the running-dir law); verbs are path-safe."
    }

    /// Refuse an `ingress_rule` no resolver implements. An unknown rule now SEALS
    /// rather than throwing, so the edge is safe either way — but reporting
    /// `{"updated": true}` for a security control that does not exist is its own
    /// failure, and the complaint belongs at the write, next to the typo.
    /// Mirrors py `file_bridge.validate_meta` / rust `FileBundle::validate_meta`.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        if let spec = meta["ingress_rule"], let rule = try? IngressRules.resolve(spec),
            let deny = rule as? IngressRules.DenyInbound, let bad = deny.unknown
        {
            return "ingress_rule=\"\(bad)\" is not a rule this bundle implements — "
                + "legal values: allow_all_ingress, deny_ingress"
        }
        return passwordRefusal(meta, edge: "fs edge")
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        // GATE — the fs edge is an io_bridge leg: SEALED by default. Every verb
        // except discovery/lifecycle is denied until opened (ingress_rule=allow_all).
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
        switch verb {
        case "reflect":
            // Discovery shows the configured root verbatim (no clamp — py parity).
            return reflect(agent: agent, kernel: kernel)
        case "boot", "shutdown":
            return .null
        case "read_stream", "write_stream":
            // Stream verbs carry RAW BYTES — binary channel only. A CODED
            // refusal: a consumer that lands here must be able to branch (#85).
            return Refusal.bad(verb, "bytes", "raw bytes on the binary channel (send_with_binary)")
        case "pump":
            // The PUMP coordinates a SOURCE→SINK copy over the binary channel
            // (it never touches bytes itself), so it rides the text channel.
            return await pump(agent: agent, payload: payload, kernel: kernel)
        default:
            // Every DATA verb clamps the root to the running dir FIRST — the
            // running-dir law: a root that escapes the kernel's workdir refuses.
            guard let root = clampedRoot(agent: agent, kernel: kernel) else {
                return Refusal.make(Refusal.transportError, rootEscapeError(agent: agent))
            }
            switch verb {
            case "list":
                return listFiles(root: root, agent: agent, payload: payload)
            case "delete":
                return deleteFile(root: root, agent: agent, payload: payload)
            case "rename":
                return renameFile(root: root, agent: agent, payload: payload)
            case "mkdir":
                return makeDir(root: root, agent: agent, payload: payload)
            default:
                return Refusal.make(
                    Refusal.unhandled, "file_bridge: unknown type \"\(verb)\"",
                    extra: [("verb", .string(verb))])
            }
        }
    }

    /// Binary channel: `read_stream` (empty request → reply BODY = one raw chunk),
    /// `write_stream` (request BODY = one raw chunk → status reply, no body), and
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
        guard let root = clampedRoot(agent: agent, kernel: kernel) else {
            return (Refusal.make(Refusal.transportError, rootEscapeError(agent: agent)), Data())
        }
        if verb == "read_stream" {
            return readStream(root: root, header: header)
        }
        if agent.metaValue(forKey: "readonly")?.asBool == true {
            return (readonlyRefusal(), Data())
        }
        return (writeStream(root: root, header: header, blob: blob), Data())
    }

    // MARK: - Verbs

    private func reflect(agent: Agent, kernel: Kernel) -> JSON {
        let root = rootPath(agent: agent)
        let readonly = agent.metaValue(forKey: "readonly")?.asBool ?? false
        // The auth POSTURE, python's `describe(rec)`: rule NAMES (never config),
        // keyed by the field a caller WRITES (read-key == write-key), + `sealed`.
        let ingress = ruleName(
            manifoldSpec(
                agent.metaValue(forKey: "ingress_rule"), agent.metaValue(forKey: "auth"),
                "_ingress"),
            default: "deny_ingress")
        let egress = ruleName(
            manifoldSpec(
                agent.metaValue(forKey: "egress_rule"), agent.metaValue(forKey: "auth"),
                "_egress"),
            default: "silent_egress")
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(agent.id.value),
            "sentence": .string(
                "Filesystem edge of the io family — sealed by default, running-dir only."),
            "root": .string(root.path),
            "readonly": .bool(readonly),
            "hidden_count": .integer(Int64(hidden(agent: agent).count)),
            "ingress_rule": .string(ingress),
            "egress_rule": .string(egress),
            "sealed": .bool(ingress != "allow_all_ingress"),
            "verbs": [
                "reflect":
                    "Identity + filesystem root + readonly flag + auth posture (ingress/egress/sealed). No args.",
                "boot": "No-op. Returns None.",
                "list": "args: path?. Returns {path, files}.",
                "read_stream":
                    "args: path, offset?, length?. SOURCE — one raw chunk; reply {bytes, next_offset, eof, size} on the binary channel.",
                "write_stream":
                    "args: path, bytes, offset?, truncate?. SINK — one raw chunk (a model composes it via send_binary's fenced form); reply {written, offset, size}. Refuses non-bytes.",
                "pump":
                    "args: source:str? + source_path:str (req) + sink:str? + sink_path:str? + chunk:int=65536. Server-side SOURCE→SINK copy.",
                "delete":
                    "args: path (req), recursive:bool=false. Returns {path, deleted: true}.",
                "rename": "args: old_path, new_path. Returns {old_path, new_path}.",
                "mkdir": "args: path. Returns {path, created: true}.",
            ],
        ]
        if clampedRoot(agent: agent, kernel: kernel) == nil {
            // Reflect NEVER refuses — the configured root is shown + the violation.
            out["root_error"] = .string(rootEscapeError(agent: agent))
        }
        return .object(out)
    }

    private func listFiles(root: URL, agent: Agent, payload: JSON) -> JSON {
        let relative: String
        let target: URL
        do {
            relative = try pathArg("list", payload, "path")
            target = try resolvePath("list", root: root, path: relative)
        } catch let r as Refused {
            return r.json
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
        let fm = FileManager.default
        // Python's branches, in python's order: absent ⇒ not_found (+path);
        // present but not a dir ⇒ bad_arg; the OS refusing the walk ⇒ transport_error.
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: target.path, isDirectory: &isDir) else {
            return notFound("path \"\(relative)\" does not exist", path: relative)
        }
        guard isDir.boolValue else {
            return Refusal.bad("list", "path", "a directory; \"\(relative)\" is not one")
        }
        let entries: [URL]
        do {
            // No `.skipsHiddenFiles`: dotfiles are listed unless the record's
            // `hidden` names them — python/rust hide ONLY that list (#60).
            entries = try fm.contentsOfDirectory(
                at: target, includingPropertiesForKeys: [.isDirectoryKey, .fileSizeKey],
                options: [])
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
        let hiddenNames = hidden(agent: agent)
        var files: [JSON] = []
        for url in entries {
            let name = url.lastPathComponent
            if hiddenNames.contains(name) { continue }
            let resourceValues = try? url.resourceValues(forKeys: [
                .isDirectoryKey, .fileSizeKey,
            ])
            let isDir = resourceValues?.isDirectory ?? false
            let rel = (relative.isEmpty ? name : "\(relative)/\(name)")
            var entry: OrderedDictionary<String, JSON> = [:]
            entry["name"] = .string(name)
            entry["path"] = .string(rel)
            entry["type"] = .string(isDir ? "dir" : "file")
            if !isDir, let size = resourceValues?.fileSize {
                entry["size"] = .integer(Int64(size))
            }
            files.append(.object(entry))
        }
        files.sort {
            ($0["name"].asString ?? "") < ($1["name"].asString ?? "")
        }
        return .object([
            "path": .string(relative),
            "files": .array(files),
        ])
    }

    /// `delete path recursive?` — a file, an EMPTY dir, or (`recursive:true`) a
    /// dir tree. A symlink is UNLINKED, never followed (`attributesOfItem` does
    /// not follow, so a dangling one is still a deletable name). A bare delete
    /// on a populated dir refuses like python's `rmdir` (it used to `removeItem`
    /// regardless, #57).
    private func deleteFile(root: URL, agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let relative: String
        let target: URL
        do {
            (relative, target) = try targetArg("delete", root: root, payload, "path")
        } catch let r as Refused {
            return r.json
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
        let fm = FileManager.default
        let recursive = payload["recursive"].asBool ?? false
        guard let attrs = try? fm.attributesOfItem(atPath: target.path) else {
            return notFound("path \"\(relative)\" does not exist", path: relative)
        }
        let kind = attrs[.type] as? FileAttributeType
        do {
            if kind == .typeDirectory && !recursive {
                let contents = try fm.contentsOfDirectory(atPath: target.path)
                if !contents.isEmpty {
                    return Refusal.make(
                        Refusal.transportError,
                        "delete \"\(relative)\": Directory not empty (pass recursive:true)")
                }
            }
            try fm.removeItem(at: target)
            return .object([
                "path": .string(relative),
                "deleted": .bool(true),
            ])
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
    }

    private func renameFile(root: URL, agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let oldRel: String
        let oldURL: URL
        let newRel: String
        let newURL: URL
        do {
            (oldRel, oldURL) = try targetArg("rename", root: root, payload, "old_path")
            (newRel, newURL) = try targetArg("rename", root: root, payload, "new_path")
        } catch let r as Refused {
            return r.json
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
        guard FileManager.default.fileExists(atPath: oldURL.path) else {
            return notFound("path \"\(oldRel)\" does not exist", path: oldRel)
        }
        do {
            try FileManager.default.createDirectory(
                at: newURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            // py `os.replace` / rust `fs::rename` semantics: an existing
            // destination is atomically CLOBBERED — the tmp+rename commit every
            // consumer (text_file among them) builds its atomicity on.
            if FileManager.default.fileExists(atPath: newURL.path) {
                _ = try FileManager.default.replaceItemAt(newURL, withItemAt: oldURL)
            } else {
                try FileManager.default.moveItem(at: oldURL, to: newURL)
            }
            return .object([
                "old_path": .string(oldRel),
                "new_path": .string(newRel),
            ])
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
    }

    private func makeDir(root: URL, agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let relative: String
        let target: URL
        do {
            (relative, target) = try targetArg("mkdir", root: root, payload, "path")
        } catch let r as Refused {
            return r.json
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
        do {
            try FileManager.default.createDirectory(
                at: target, withIntermediateDirectories: true)
            return .object([
                "path": .string(relative),
                "created": .bool(true),
            ])
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
    }

    // MARK: - Helpers

    /// The record's `hidden` list (names hidden from `list`), or DEFAULT_HIDDEN.
    private func hidden(agent: Agent) -> Set<String> {
        if let arr = agent.metaValue(forKey: "hidden")?.asArray {
            return Set(arr.compactMap { $0.asString })
        }
        return DEFAULT_HIDDEN
    }

    /// The configured root verbatim (reflect shows this; data verbs clamp it).
    private func rootPath(agent: Agent) -> URL {
        if let rootStr = agent.metaValue(forKey: "root")?.asString {
            return URL(fileURLWithPath: rootStr)
        }
        return agent.rootPath
    }

    private func rootEscapeError(agent: Agent) -> String {
        "fs: root \"\(agent.metaValue(forKey: "root")?.asString ?? "")\" escapes the running dir"
    }

    /// The kernel's RUNNING DIRECTORY — its disk workdir (the dir that holds
    /// `.fantastic`). In production this is the process cwd, matching py
    /// `Path.cwd()`; for an in-memory kernel that still touches disk we fall
    /// back to the process cwd.
    private func workdirBase(kernel: Kernel) -> URL {
        if let wd = kernel.storage.workdir { return wd }
        return URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
    }

    /// THE RUNNING-DIR LAW — clamp the configured root to the running dir
    /// (`base`). A relative root resolves under it; an absolute root is allowed
    /// ONLY if it lies inside it; outside escapes refuse. Mirrors py
    /// `fs.resolve_root` (whose base is `Path.cwd()`). Returns the clamped root,
    /// or nil on escape.
    private func clampedRoot(agent: Agent, kernel: Kernel) -> URL? {
        let base = workdirBase(kernel: kernel).standardizedFileURL
        let rawStr = agent.metaValue(forKey: "root")?.asString
        let candidate: URL
        if let rawStr, !rawStr.isEmpty {
            // Test the raw STRING for absoluteness — `URL(fileURLWithPath:)` on a
            // relative path resolves it against the process cwd, which would
            // wrongly read a workdir-relative root (".fantastic") as outside.
            candidate =
                rawStr.hasPrefix("/")
                ? URL(fileURLWithPath: rawStr).standardizedFileURL
                : base.appendingPathComponent(rawStr).standardizedFileURL
        } else {
            // No root meta ⇒ the agent's own dir (already inside the workdir).
            candidate = agent.rootPath.standardizedFileURL
        }
        // Must lie inside the running dir. Compare CANONICALIZED paths
        // (Kernel.canonPath: symlinks + the macOS `/private/var` ≡ `/var` alias)
        // — `agent.rootPath` is often the `/private`-resolved form while the
        // workdir base is not, and a raw string compare would wrongly refuse a
        // root that IS inside (the sibling of the storeReldir double-nest bug).
        let c = Kernel.canonPath(candidate.path)
        let b = Kernel.canonPath(base.path)
        if c == b || c.hasPrefix(b + "/") {
            return candidate
        }
        return nil
    }

    /// Resolve `path` under `root`, refusing escapes — the one refusal shape.
    ///
    /// Two checks, both required:
    /// 1. LEXICAL — `clampRoot` folds `..` and refuses a climb above the root
    ///    (`transport_error`, "… escapes root" — the code python's `fs`
    ///    ValueError maps to). An absolute path is `bad_arg` (via `pathArg`).
    /// 2. CANONICAL — the deepest EXISTING ancestor of the joined path is
    ///    canonicalized and re-checked against the canonical root. This is what
    ///    follows symlinks: an in-root `docs -> /etc` used to pass the lexical
    ///    check and be followed at open time (#30 — python refused it, the ports
    ///    served `/etc/passwd`). BOTH sides go through `Kernel.canonPath` so the
    ///    macOS `/var` ≡ `/private/var` alias cannot mismatch. A root that does
    ///    not exist yet has nothing under it to follow, so the check is skipped;
    ///    a dangling symlink LEAF does not "exist" (fileExists follows), so its
    ///    parent is checked instead — which is what makes it deletable.
    func resolvePath(_ verb: String, root: URL, path: String) throws -> URL {
        if path.hasPrefix("/") {
            throw Refused(json: Refusal.bad(verb, "path", "a root-relative path (no leading '/')"))
        }
        let escape = Refused(
            json: Refusal.make(Refusal.transportError, "fs: path \"\(path)\" escapes root"))
        guard let joined = try? clampRoot(path, base: root) else { throw escape }
        let fm = FileManager.default
        if fm.fileExists(atPath: root.path) {
            var probe = joined
            while probe.path != "/" && !fm.fileExists(atPath: probe.path) {
                probe = probe.deletingLastPathComponent()
            }
            let c = Kernel.canonPath(probe.path)
            let r = Kernel.canonPath(root.path)
            if !(c == r || c.hasPrefix(r + "/")) { throw escape }
        }
        return joined
    }

    /// `pathArg` + `resolvePath` for a MUTATING verb: the path must name
    /// something INSIDE the root — empty/`.` is `missing_arg`, anything
    /// resolving to the root itself is `bad_arg`. `delete {path:"",
    /// recursive:true}` used to remove the whole served store (#97): the root
    /// is the boundary, not an operand.
    func targetArg(_ verb: String, root: URL, _ payload: JSON, _ name: String) throws -> (
        String, URL
    ) {
        let p = try pathArg(verb, payload, name)
        if p.isEmpty || p == "." { throw Refused(json: Refusal.missing(verb, name)) }
        let target = try resolvePath(verb, root: root, path: p)
        if target.standardizedFileURL.path == root.standardizedFileURL.path {
            throw Refused(
                json: Refusal.bad(verb, name, "a path inside the root, not the root itself"))
        }
        return (p, target)
    }

    private func readonlyError(agent: Agent) -> JSON? {
        if agent.metaValue(forKey: "readonly")?.asBool == true {
            return readonlyRefusal()
        }
        return nil
    }

    private func fileSize(_ url: URL) -> Int64 {
        let n = try? FileManager.default.attributesOfItem(atPath: url.path)[.size] as? NSNumber
        return n.flatMap { $0 }?.int64Value ?? 0
    }

    // MARK: - Streams (binary channel)

    /// SOURCE — read ONE raw chunk at `offset`. Reply `{path, offset,
    /// next_offset, eof, size}` + body = the raw bytes (binary channel; never
    /// base64). Stateless cursor: pull the next with `offset=next_offset` to eof.
    private func readStream(root: URL, header: JSON) -> (JSON, Data) {
        let path: String
        let offset: Int64
        let length: Int64
        let target: URL
        do {
            path = try pathArg("read_stream", header, "path")
            offset = try intArg("read_stream", header, "offset", default: 0, minimum: 0)
            length = try intArg("read_stream", header, "length", default: 65536, minimum: 1)
            target = try resolvePath("read_stream", root: root, path: path)
        } catch let r as Refused {
            return (r.json, Data())
        } catch {
            return (Refusal.make(Refusal.transportError, "\(error)"), Data())
        }
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: target.path, isDirectory: &isDir),
            !isDir.boolValue
        else {
            // Python parity: the refusal names its reason and path (consumers —
            // text_file checkpoints among them — branch on `reason`).
            return (notFound("file \"\(path)\" not found", path: path), Data())
        }
        guard let handle = try? FileHandle(forReadingFrom: target) else {
            return (Refusal.make(Refusal.transportError, "cannot open \"\(path)\""), Data())
        }
        defer { try? handle.close() }
        let size = fileSize(target)
        // Read only what EXISTS: the request bounds the chunk, the file bounds
        // the request — a caller-chosen `length` never sizes a buffer.
        let want = min(length, max(0, size - offset))
        var chunk = Data()
        if want > 0 {
            do {
                try handle.seek(toOffset: UInt64(offset))
                chunk = try handle.read(upToCount: Int(want)) ?? Data()
            } catch {
                return (Refusal.make(Refusal.transportError, "read \"\(path)\": \(error)"), Data())
            }
        }
        let nextOffset = offset + Int64(chunk.count)
        let meta: JSON = .object([
            "path": .string(path),
            "offset": .integer(offset),
            "next_offset": .integer(nextOffset),
            "eof": .bool(nextOffset >= size),
            "size": .integer(size),
        ])
        return (meta, chunk)
    }

    /// SINK — write ONE raw chunk (`blob`) at `offset` (default: append at end).
    /// `truncate:true` on the first chunk starts fresh. Returns
    /// `{path, written, offset, size}` — `size` is the FILE's size after the
    /// write (py `stat().st_size`), not the write cursor (#59).
    private func writeStream(root: URL, header: JSON, blob: Data) -> JSON {
        let path: String
        let offsetIn: Int64?
        let target: URL
        do {
            path = try pathArg("write_stream", header, "path")
            offsetIn =
                header["offset"].isNull
                ? nil
                : try intArg("write_stream", header, "offset", default: 0, minimum: 0)
            target = try resolvePath("write_stream", root: root, path: path)
        } catch let r as Refused {
            return r.json
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
        let truncate = header["truncate"].asBool ?? false
        do {
            try FileManager.default.createDirectory(
                at: target.deletingLastPathComponent(), withIntermediateDirectories: true)
        } catch {
            return Refusal.make(Refusal.transportError, "mkdir parent: \(error)")
        }
        if !FileManager.default.fileExists(atPath: target.path) {
            FileManager.default.createFile(atPath: target.path, contents: nil)
        }
        guard let handle = try? FileHandle(forWritingTo: target) else {
            return Refusal.make(Refusal.transportError, "cannot open \"\(path)\" for writing")
        }
        defer { try? handle.close() }
        let off: UInt64
        do {
            if truncate { try handle.truncate(atOffset: 0) }
            if let o = offsetIn {
                off = UInt64(o)
                try handle.seek(toOffset: off)
            } else {
                off = try handle.seekToEnd()
            }
            try handle.write(contentsOf: blob)
            try handle.synchronize()
        } catch {
            return Refusal.make(Refusal.transportError, "write \"\(path)\": \(error)")
        }
        return .object([
            "path": .string(path),
            "written": .integer(Int64(blob.count)),
            "offset": .integer(Int64(off)),
            "size": .integer(fileSize(target)),
        ])
    }

    /// The PUMP — a server-side SOURCE→SINK copy, chunk by chunk, in ONE call.
    /// Storage-agnostic: both ends are bound BY ID + the duck-typed stream verbs
    /// over the binary channel, so a `network_bridge` SOURCE pumps to a
    /// `file_bridge` SINK the same as fs→fs. Each end SELF-gates + SELF-clamps.
    private func pump(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        let selfId = agent.id.value
        let src = payload["source"].asString ?? selfId
        let sink = payload["sink"].asString ?? selfId
        let spath = payload["source_path"].asString ?? payload["path"].asString ?? ""
        let dpath = payload["sink_path"].asString ?? spath
        let chunk: Int64
        do {
            chunk = try intArg("pump", payload, "chunk", default: 65536, minimum: 1)
        } catch let r as Refused {
            return r.json
        } catch {
            return Refusal.make(Refusal.transportError, "\(error)")
        }
        var offset: Int64 = 0
        var first = true
        var chunks: Int64 = 0
        while true {
            let (rmeta, body) = await kernel.sendWithBinary(
                AgentId(src),
                .object([
                    "type": .string("read_stream"), "path": .string(spath),
                    "offset": .integer(offset), "length": .integer(chunk),
                ]), Data())
            if rmeta["error"].asString != nil {
                return Refusal.make(
                    Refusal.transportError, "pump: read from \"\(src)\" failed: \(rmeta.serialize())")
            }
            let (wmeta, _) = await kernel.sendWithBinary(
                AgentId(sink),
                .object([
                    "type": .string("write_stream"), "path": .string(dpath),
                    "offset": .integer(offset), "truncate": .bool(first),
                ]), body)
            if wmeta["error"].asString != nil {
                return Refusal.make(
                    Refusal.transportError, "pump: write to \"\(sink)\" failed: \(wmeta.serialize())")
            }
            first = false
            chunks += 1
            let next = rmeta["next_offset"].asInt
            if rmeta["eof"].asBool ?? true {
                offset = next ?? offset
                break
            }
            guard let n = next, n > offset else {
                // No forward progress and no eof: a SOURCE that would loop forever.
                return Refusal.make(
                    Refusal.transportError,
                    "pump: source \"\(src)\" made no progress at offset \(offset)")
            }
            offset = n
        }
        return .object([
            "source": .string(spath), "sink": .string(dpath),
            "bytes": .integer(offset), "chunks": .integer(chunks),
        ])
    }
}
