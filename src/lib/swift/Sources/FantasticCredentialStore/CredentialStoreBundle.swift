// credential_store — secrets an agent USES but never keeps.
//
// Port of python `bundled_agents/security/credential_store/src/credential_store/
// tools.py`, which is canonical: wire shapes, field order, refusal reasons and
// error texts byte for byte.
//
// A credential is addressed by a REF (`openai/main`) and is bound to ONE HOST: a
// value plus the host it may be sent to. A bundle that needs one names it on its
// own record (`credentials: {store, ref}`), reads it at USE time and refuses to
// send it to any other host — so rotating a key is rewriting the ref, and no
// restart is ever needed.
//
// WHERE THE VALUES LIVE — `backend` on the record, one of:
//   * `file` (the default): a YAML file at an ABSOLUTE path —
//     `~/.fantastic/credentials/<store id>.yaml` unless `file` names another one
//     (`/…` or `~…`; a relative path is refused at the write). It is written 0600
//     and READ ONLY IF it is a regular file owned by this user with no group/other
//     bits — anything else is refused, loud. The bytes are the kernel's YAML
//     writer (`YamlIO`) over `{refs: {ref: {host, fields: {field: value}}}}`, keys
//     sorted, so either runtime reads the other's file.
//   * `keychain`: the OS keychain (`KeychainStore`), one item per ref, under the
//     namespace `keychain_namespace` (default `fantastic` — macOS calls it the
//     item's "service"), account `<store id>/<ref>`, `{host, fields}` as a JSON
//     object. The explicit choice for the strongest at-rest protection.
//   * `env`: read-only; each ref maps its fields to environment variables
//     (`refs: {ref: {host, env: {api_key: VAR}}}`), for a container or CI.
//
// THE HOST IS STORED WITH THE VALUE: `set` writes the ref's
// declared host beside its fields — the file entry and the keychain item are
// `{host, fields}` — and `get` refuses a value whose stored host is not the one its
// declaration names, or that carries none: a store elsewhere declaring the same id
// and ref with another host reads nothing. The `env` backend keeps no value at rest.
//
// A REF IS DECLARED ONLY BY `update_agent` on the store's own record (the
// operator, or the store's parent): `refs: {<ref>: {host, fields?/env?}}` (it
// replaces the whole map; `host` is required — the binding). Only `update_agent`
// writes or removes a declaration: `set` stores the
// value of a DECLARED ref, never creates one and takes no `host` (an undeclared ref
// answers "no such ref"; a `host` on the call is refused); `forget` drops the value
// and keeps the declaration. `fields` on a ref are the stored value's NAMES: `set`
// writes them, `forget` drops them.
//
// THE RECORD holds only metadata — the host and the field NAMES. The CALL carries
// the value: a `set` payload and a `get` reply are recorded like any call's (the
// ledger, the memory store, the state stream), in clear.
//
// WHO MAY CALL is the kernel's call gate alone: any caller it admits may
// `set`/`get`/`forget` a declared ref — there are no per-ref access lists.
//
// ONE CALLER AT A TIME (swift only). Python's verbs run on one asyncio thread and
// none awaits, so a load-modify-save of the file never interleaves with another.
// Swift dispatches on a thread pool, so every verb body runs under the bundle's
// ONE lock; no verb awaits, so the lock is never held across a suspension.

import FantasticFile
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

let NAME = "credential_store"
let BACKENDS = ["file", "keychain", "env"]
let DEFAULT_DIR = "~/.fantastic/credentials"
let KEYCHAIN_NAMESPACE = "fantastic"

public final class CredentialStoreBundle: AgentBundle, @unchecked Sendable {
    public static let handlerModule = "credential_store.tools"

    public let name = NAME
    /// The OS keychain the `keychain` backend uses — the real one by default; a
    /// test hands in an in-memory double.
    let keychain: any KeychainStore
    private let lock = NSLock()

    public init(keychain: any KeychainStore = SystemKeychain()) {
        self.keychain = keychain
    }

    public var verbs: [String] { Array(Self.VERBS.keys) }
    public var verbDocTable: JSON { .object(Self.VERBS.mapValues { JSON.string($0) }) }

    public var readme: String? { Self.README }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return Refusal.unknown(agentId.value, live: kernel.allAgents().map { $0.id.value })
        }
        // ONE VERB AT A TIME — see the header.
        return try lock.withLock { try dispatch(agent, kernel, payload) }
    }

    private func dispatch(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) throws(PyError)
        -> JSON?
    {
        let t = payload["type"]
        let ctx = Ctx(agent: agent, kernel: kernel, keychain: keychain)
        switch t.asString ?? "" {
        case "reflect": return ctx.reflect()
        case "boot": return ctx.boot()
        case "set": return try ctx.set(payload)
        case "get": return try ctx.get(payload)
        case "list": return try ctx.list()
        case "forget": return try ctx.forget(payload)
        default:
            return Refusal.make(
                Refusal.unhandled, "\(NAME): unknown type \(pyRepr(t))", extra: [("verb", t)])
        }
    }

    // MARK: - the record

    /// The record's own fields, refused at the write when malformed. Python
    /// `validate_meta`, its texts byte for byte.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        func given(_ k: String) -> JSON? {
            guard let v = meta[k], v != .null else { return nil }
            return v
        }
        func nonEmptyStrings(_ v: JSON) -> Bool {
            guard case .array(let a) = v else { return false }
            return a.allSatisfy { if case .string(let s) = $0 { return !s.isEmpty } else { return false } }
        }
        if let b = given("backend") {
            if !(b.asString.map(BACKENDS.contains) ?? false) {
                return "backend=\(pyRepr(b)) is not one of \(BACKENDS.joined(separator: ", "))"
            }
        }
        for k in ["file", "keychain_namespace"] {
            if let v = given(k) {
                guard case .string(let s) = v, !pyStrip(s).isEmpty else {
                    return "\(k) must be a non-empty string"
                }
            }
        }
        if case .string(let p) = given("file"), let why = relativeFile(p) {
            return why
        }
        if let refs = given("refs") {
            guard case .object(let map) = refs else {
                return "refs must be a map of ref to {host, fields, env}"
            }
            for (ref, e) in map {
                if pyStrip(ref).isEmpty { return "a ref must be a non-empty string" }
                guard case .object(let entry) = e else {
                    return "refs[\(pyRepr(ref))] must be a map"
                }
                if let v = entry["fields"], v != .null, !nonEmptyStrings(v) {
                    return "refs[\(pyRepr(ref))].fields must be a list of strings"
                }
                // python's urlsplit RAISES here for an unbalanced IPv6 bracket;
                // a `validateMeta` cannot raise, so that host is refused as blank
                guard let h = entry["host"], h != .null else {
                    return "refs[\(pyRepr(ref))].host is required — the one host this ref's value "
                        + "may be sent to"
                }
                if (try? hostOf(h)) == nil {
                    return "refs[\(pyRepr(ref))].host must be a non-empty host"
                }
                if let env = entry["env"], env != .null {
                    let ok: Bool = {
                        guard case .object(let m) = env else { return false }
                        return m.values.allSatisfy {
                            if case .string(let s) = $0 { return !s.isEmpty } else { return false }
                        }
                    }()
                    if !ok { return "refs[\(pyRepr(ref))].env must map field names to variable names" }
                }
            }
        }
        return nil
    }

    // MARK: - verb docs + readme (python's, verbatim)

    /// Python's `VERBS`, each doc's first line — what reflect's `verbs` shows.
    static let VERBS: OrderedDictionary<String, String> = [
        "reflect":
            "Identity + posture: the backend, where the file lives, how many refs; never a value or a ref name. No args.",
        "boot": "No-op: values are read from the backend when asked for.",
        "set":
            "args: ref:str, fields:{name: value} (req). Store or rotate the value of a DECLARED ref — its host is the declaration's (refs are declared by update_agent on this store's record; set never creates one and takes no host). Returns {ref, host, fields:[names], stored}.",
        "get":
            "args: ref:str (req), host:str? (where you will send it — refused unless the ref is bound to it). The credential: {ref, host, fields:{name: value}}. Send the value ONLY to `host`.",
        "list":
            "No args. The declared refs — metadata only: {refs:[{ref, host, fields:[names]}]}. Never a value.",
        "forget":
            "args: ref:str (req). Drop a ref's VALUE from the backend; its declaration (host) stays — update_agent alone undeclares a ref. Returns {forgotten, ref}.",
    ]

    /// THE PYTHON FILE, BYTE FOR BYTE (`credential_store/readme.md`).
    static let README = #"""
        # credential_store — secrets an agent uses but never keeps

        A credential is a REF (`openai/main`) holding named fields (`api_key`) and bound to
        ONE HOST. A consumer names it on its own record — `credentials: {store, ref}` —
        reads it at USE time and sends it only to that host. Rotating a key is one `set`;
        the next read picks it up, with no restart.

        | verb | what |
        |---|---|
        | `set {ref, fields}` | store or rotate the value of a DECLARED ref; takes no `host` — the declaration's is the binding |
        | `get {ref, host?}` | `{ref, host, fields}`; `host` refuses a ref bound elsewhere |
        | `list` | the declared refs: metadata, never a value |
        | `forget {ref}` | drop the VALUE; the declaration stays |

        **Backends** (`backend` on the record): `file` (default) — one YAML file at an
        ABSOLUTE path, `~/.fantastic/credentials/<id>.yaml` unless `file` names another
        (`/…` or `~…`; a relative path is refused at the write: "file='creds.yaml' must be
        an absolute path (starting with / or ~) — a relative path is refused"), written
        0600 and read only if it is a regular file owned by this user with no group/other
        bits — judged on the one open it is read from (`O_NOFOLLOW`, then `fstat` of that
        descriptor), so nothing can swap it between the check and the read; `keychain` — the OS keychain, one item per ref under the namespace
        `keychain_namespace` (default `fantastic` — macOS calls it the item's "service";
        account `<id>/<ref>`); `env` — read-only, each ref maps fields to variables:
        `refs: {<ref>: {host, env: {api_key: VAR}}}`.

        **The host is stored with the value**: the file entry and the keychain item are
        `{host, fields}`, the host written by `set` from the ref's declaration. A `get`
        whose declaration names another host than the stored one is refused with the
        same answer as a `get {ref, host}` naming another host
        ("credential_store: ref '<ref>' is bound to host '<declared>', not '<stored>' —
        refused, nothing read", `reason: bad_arg`, `arg: host`), and so is a stored value
        carrying no host (`… not None — …`) — `set` the value again. A store elsewhere declaring the same id and ref with
        another host reads nothing. `env` keeps no value at rest.

        **A ref is declared ONLY by `update_agent`** on the store's own record (the
        operator, or the store's parent):
        `refs: {<ref>: {host, fields?/env?}}` (it replaces
        the whole `refs` map — write every ref you keep; `host` is required: "refs['<ref>'].host
        is required — the one host this ref's value may be sent to"). `set` never creates a ref (an undeclared one answers "no such ref",
        the operator included) and takes no host ("credential_store set: takes no host — a
        ref's host is its declaration, written by update_agent on this store's refs");
        `forget` drops the value and keeps the declaration — only
        `update_agent` undeclares. On the `env` backend `forget` is refused like `set`
        ("credential_store forget: the env backend is read-only — unset the environment
        variable its ref names"). A ref's `fields` are its stored value's names: `set`
        writes them, `forget` drops them.
        **There are no per-ref access lists**: any caller the kernel's call gate admits may
        `set`/`get`/`forget` a declared ref.

        **The record holds the host and the field NAMES only.** The value rides the call:
        a `set` payload and a `get` reply are recorded like any call's — the ledger, the
        memory store, the state stream — in clear. Never give a credential to an AI agent's
        prompt — give it to a bundle that sends it (an AI backend reads its own key this
        way, as itself).

        """#
}

// MARK: - one verb's view of the store

/// What a verb body works with: the store agent, the kernel and the keychain.
private struct Ctx {
    let agent: Agent
    let kernel: Kernel
    let keychain: any KeychainStore

    var id: String { agent.id.value }

    func meta(_ k: String) -> JSON { agent.metaValue(forKey: k) ?? .null }

    /// `agent.record.get("backend") or "file"`.
    var backendName: String { pyTruthy(meta("backend")) ? pyStr(meta("backend")) : "file" }

    /// Python `_refs`: the record's `refs` map, or empty.
    var refs: OrderedDictionary<String, JSON> { meta("refs").asObject ?? [:] }

    func noSuchRef(_ ref: String) -> JSON {
        Refusal.make(
            Refusal.notFound, "\(NAME): no such ref \(pyRepr(ref))", extra: [("arg", .string("ref"))])
    }

    /// The backend the record names. Python `_backend`.
    func backend() throws(PyError) -> any CredentialBackend {
        switch backendName {
        case "keychain": return KeychainBackend(ctx: self)
        case "env": return EnvBackend(ctx: self)
        default: return try FileBackend(ctx: self)
        }
    }

    // ─── verbs ───

    /// Python `_boot`: no-op — values are read from the backend when asked for.
    func boot() -> JSON? { nil }

    func reflect() -> JSON {
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(id),
            "sentence": .string("Credential store — secrets by ref, each bound to one host."),
            "backend": .string(backendName),
            "refs": .integer(Int64(refs.count)),
        ]
        if backendName == "file" { out["file"] = .string(FileBackend.shown(self)) }
        return .object(out)
    }

    func set(_ payload: JSON) throws(PyError) -> JSON {
        guard case .string(let ref) = payload["ref"], !pyStrip(ref).isEmpty else {
            return Refusal.missing("\(NAME) set", "ref")
        }
        if case .object(let o) = payload, o["host"] != nil {
            // the binding is the declaration's alone
            return Refusal.make(
                Refusal.badArg,
                "\(NAME) set: takes no host — a ref's host is its declaration, written "
                    + "by update_agent on this store's refs",
                extra: [("arg", .string("host"))])
        }
        let fields: OrderedDictionary<String, JSON>
        if case .object(let f) = payload["fields"], !f.isEmpty,
            f.allSatisfy({ !$0.key.isEmpty && { if case .string(let s) = $0 { return !s.isEmpty } else { return false } }($0.value) })
        {
            fields = f
        } else {
            return Refusal.make(
                Refusal.badArg,
                "\(NAME) set: fields must be a non-empty map of field name to string value",
                extra: [("arg", .string("fields"))])
        }
        let names = pySorted(Array(fields.keys))
        if backendName == "env" {
            return Refusal.make(
                Refusal.badArg,
                "\(NAME) set: the env backend is read-only — set the environment variable "
                    + "and name it in the ref's `env`",
                extra: [("arg", .string("backend"))])
        }
        var all = refs
        // declared by update_agent alone — set never makes one
        guard let existing = all[ref] else { return noSuchRef(ref) }
        var entry = existing.asObject ?? [:]
        // the declared host is stored WITH the value
        try backend().put(ref, try hostOf(existing["host"]), fields)
        entry["fields"] = .array(names.map { .string($0) })  // the value's NAMES, never a value
        all[ref] = .object(entry)
        kernel.update(agent.id, ["refs": .object(all)])
        return .object([
            "ref": .string(ref),
            "host": (try hostOf(existing["host"])).map { .string($0) } ?? .null,
            "fields": .array(names.map { .string($0) }), "stored": .bool(true),
        ])
    }

    func get(_ payload: JSON) throws(PyError) -> JSON {
        guard case .string(let ref) = payload["ref"], !pyStrip(ref).isEmpty else {
            return Refusal.missing("\(NAME) get", "ref")
        }
        guard let entry = refs[ref] else { return noSuchRef(ref) }
        if payload["host"] != .null {
            let want = try hostOf(payload["host"])
            // python's `or`: the bound host is only read when `want` is a host
            var refused = want == nil
            if !refused {
                let bound = try hostOf(entry["host"])
                refused = want != bound
            }
            if refused {
                return hostMismatch(ref, entry["host"] ?? .null, want.map { .string($0) } ?? .null)
            }
        }
        let store = try backend()
        var stored = try store.get(ref)
        if let s = stored, !s.isEmpty, !(store is EnvBackend) {
            // THE HOST AT REST: a value is read only for the host it was
            // stored with — the declaration of a store elsewhere sharing this id and
            // ref names its own host, and reads nothing.
            if let bad = try hostAtRest(ref, s["host"] ?? .null, try hostOf(entry["host"])) {
                return bad
            }
            stored = s["fields"]?.asObject
        }
        guard let fields = stored, !fields.isEmpty else {
            return Refusal.make(
                Refusal.notFound, "\(NAME): ref \(pyRepr(ref)) has no value in the backend",
                extra: [("arg", .string("ref"))])
        }
        return .object([
            "ref": .string(ref),
            "host": (try hostOf(entry["host"])).map { .string($0) } ?? .null,
            "fields": .object(fields),
        ])
    }

    func list() throws(PyError) -> JSON {
        var out: [JSON] = []
        let all = refs
        for ref in pySorted(Array(all.keys)) {
            let entry = all[ref] ?? .null
            let row: OrderedDictionary<String, JSON> = [
                "ref": .string(ref),
                "host": (try hostOf(entry["host"])).map { .string($0) } ?? .null,
                "fields": pyTruthy(entry["fields"])
                    ? entry["fields"]
                    : .array(pySorted(Array((entry["env"].asObject ?? [:]).keys)).map { .string($0) }),
            ]
            out.append(.object(row))
        }
        return .object(["refs": .array(out)])
    }

    func forget(_ payload: JSON) throws(PyError) -> JSON {
        guard case .string(let ref) = payload["ref"], !pyStrip(ref).isEmpty else {
            return Refusal.missing("\(NAME) forget", "ref")
        }
        if backendName == "env" {
            return Refusal.make(
                Refusal.badArg,
                "\(NAME) forget: the env backend is read-only — unset the environment "
                    + "variable its ref names",
                extra: [("arg", .string("backend"))])
        }
        var all = refs
        guard let entry = all[ref] else { return noSuchRef(ref) }
        try backend().drop(ref)
        // the value's names go with the value; the declaration stays
        if case .object(var kept) = entry, kept["fields"] != nil {
            kept.removeValue(forKey: "fields")
            all[ref] = .object(kept)
            kernel.update(agent.id, ["refs": .object(all)])
        }
        return .object(["forgotten": .bool(true), "ref": .string(ref)])
    }
}

// MARK: - where the values live

private protocol CredentialBackend {
    /// The value at rest — `{host, fields}`; the env backend's fields alone.
    func get(_ ref: String) throws(PyError) -> OrderedDictionary<String, JSON>?
    /// Store `fields` WITH `host`.
    func put(_ ref: String, _ host: String?, _ fields: OrderedDictionary<String, JSON>) throws(PyError)
    func drop(_ ref: String) throws(PyError)
}

/// The value at rest, `{host, fields}` — python's `{"host": host, "fields": fields}`.
private func atRest(_ host: String?, _ fields: OrderedDictionary<String, JSON>) -> JSON {
    .object(["host": host.map { .string($0) } ?? .null, "fields": .object(fields)])
}

/// THE host-mismatch refusal: the ref is bound to `bound`, not `other`. One shape
/// for a `get {ref, host}` naming another host and a value stored for another host
/// — or for none. Python `_host_mismatch`.
private func hostMismatch(_ ref: String, _ bound: JSON, _ other: JSON) -> JSON {
    Refusal.make(
        Refusal.badArg,
        "\(NAME): ref \(pyRepr(ref)) is bound to host \(pyRepr(bound)), not \(pyRepr(other)) — "
            + "refused, nothing read",
        extra: [("arg", .string("host"))])
}

/// The refusal for a stored value whose host is not the declared one — or that
/// carries none — or nil. Python `_host_at_rest`.
private func hostAtRest(_ ref: String, _ stored: JSON, _ declared: String?) throws(PyError) -> JSON? {
    let at = try hostOf(stored)
    if let at, at == declared { return nil }
    return hostMismatch(ref, declared.map { .string($0) } ?? .null, at.map { .string($0) } ?? .null)
}

/// The default backend: one YAML document of
/// `{refs: {ref: {host, fields: {field: value}}}}`, at an absolute path.
private struct FileBackend: CredentialBackend {
    /// The clamp base: the file's own directory (python's `external=True`
    /// surface — the path is still clamped within it).
    let base: String
    /// The directory the file sits in — `base` itself.
    let root: String
    let name: String
    /// The record's `file` as written — what the refusal names.
    let shown: String

    /// The record's `file`, or the default under the user's home.
    static func shown(_ ctx: Ctx) -> String {
        pyTruthy(ctx.meta("file")) ? pyStr(ctx.meta("file")) : "\(DEFAULT_DIR)/\(ctx.id).yaml"
    }

    init(ctx: Ctx) throws(PyError) {
        let p = Self.shown(ctx)
        if let why = relativeFile(p) {  // a record written past `validateMeta`
            throw PyError("ValueError", "\(NAME): \(why)")
        }
        let full = try PurePath(p).expandUser()
        base = full.parent
        root = full.parent
        name = full.name
        shown = p
    }

    func load() throws(PyError) -> OrderedDictionary<String, JSON> {
        let present: Bool
        do { present = try fsExists(root, name, base: base) } catch { throw raised(error) }
        if !present { return [:] }
        // ONE open, judged on its own descriptor (`fsReadPrivate`): checking the
        // path and then reading the path again is a window a swapped leaf
        // slips through.
        let raw: Data
        do { raw = try fsReadPrivate(root, name, base: base) } catch {
            if let p = error.notPrivate {
                throw PyError(
                    "PermissionError",
                    "\(NAME): credential file \(shown) must be a regular file owned by this user "
                        + "with mode 0600 (is 0o\(String(p.mode, radix: 8)), uid \(p.uid)) — "
                        + "refused, nothing read")
            }
            throw raised(error)
        }
        if let bad = pyUtf8DecodeError(raw) { throw PyError("UnicodeDecodeError", bad) }
        let doc: JSON
        do { doc = try YamlIO.load(String(decoding: raw, as: UTF8.self)) } catch {
            throw PyError("YAMLError", "\(error)")
        }
        return doc["refs"].asObject ?? [:]
    }

    func get(_ ref: String) throws(PyError) -> OrderedDictionary<String, JSON>? {
        try load()[ref]?.asObject
    }

    private func save(_ refs: OrderedDictionary<String, JSON>) throws(PyError) {
        // `yaml.safe_dump({"refs": refs}, sort_keys=True, default_flow_style=False)`
        let text = YamlIO.dump(sortedKeys(.object(["refs": .object(refs)])))
        do {
            try fsWriteBytes(root, name, Data(text.utf8), base: base, mode: 0o600)
        } catch { throw raised(error) }
    }

    func put(_ ref: String, _ host: String?, _ fields: OrderedDictionary<String, JSON>) throws(PyError) {
        var refs = try load()
        refs[ref] = atRest(host, fields)
        try save(refs)
    }

    func drop(_ ref: String) throws(PyError) {
        var refs = try load()
        if refs.removeValue(forKey: ref) != nil { try save(refs) }
    }
}

/// The OS keychain: one item per ref, `{host, fields}` as a JSON object.
private struct KeychainBackend: CredentialBackend {
    let keychain: any KeychainStore
    /// `keychain_namespace` — the keychain's own word for it is the item's
    /// "service", which is what `KeychainStore` names it.
    let namespace: String
    let prefix: String

    init(ctx: Ctx) {
        keychain = ctx.keychain
        let s = ctx.meta("keychain_namespace")
        namespace = pyTruthy(s) ? pyStr(s) : KEYCHAIN_NAMESPACE
        prefix = "\(ctx.id)/"
    }

    private func call<T>(_ body: () throws -> T) throws(PyError) -> T {
        do { return try body() } catch let e as PyError { throw e } catch {
            throw PyError("KeyringError", "\(error)")
        }
    }

    func get(_ ref: String) throws(PyError) -> OrderedDictionary<String, JSON>? {
        guard let raw = try call({ try keychain.get(service: namespace, account: prefix + ref) })
        else { return nil }
        let v: JSON
        do { v = try JSON.parse(raw) } catch { throw PyError("JSONDecodeError", "\(error)") }
        return v.asObject
    }

    func put(_ ref: String, _ host: String?, _ fields: OrderedDictionary<String, JSON>) throws(PyError) {
        // `json.dumps({"host": host, "fields": fields})` — python's separators and
        // ASCII escapes
        let value = pyJsonDumps(atRest(host, fields))
        try call { try keychain.set(service: namespace, account: prefix + ref, value: value) }
    }

    func drop(_ ref: String) throws(PyError) {
        if try call({ try keychain.get(service: namespace, account: prefix + ref) }) != nil {
            try call { try keychain.delete(service: namespace, account: prefix + ref) }
        }
    }
}

/// Read-only: each ref's fields come from the environment variables its record
/// entry names (`env: {field: VAR}`). No value at rest, so no host at rest.
private struct EnvBackend: CredentialBackend {
    let ctx: Ctx

    func get(_ ref: String) throws(PyError) -> OrderedDictionary<String, JSON>? {
        guard case .object(let names) = ctx.refs[ref]?["env"] ?? .null, !names.isEmpty else {
            return nil
        }
        var out: OrderedDictionary<String, JSON> = [:]
        for (field, v) in names {
            let variable = pyStr(v)
            guard let raw = getenv(variable), case let val = String(cString: raw), !val.isEmpty
            else {
                throw PyError(
                    "LookupError",
                    "\(NAME): ref \(pyRepr(ref)) reads \(pyRepr(field)) from $\(variable), "
                        + "which is unset")
            }
            out[field] = .string(val)
        }
        return out
    }

    func put(_ ref: String, _ host: String?, _ fields: OrderedDictionary<String, JSON>) throws(PyError) {
        // unreachable: `set` refuses the env backend before it gets here
        throw PyError("RuntimeError", "\(NAME): the env backend is read-only")
    }

    func drop(_ ref: String) throws(PyError) {}
}

// MARK: - helpers

/// The refusal for a `file` that is not absolute (`/…` or `~…`), or nil. Python
/// `_relative_file`.
func relativeFile(_ p: String) -> String? {
    if p.hasPrefix("/") || p.hasPrefix("~") { return nil }
    return "file=\(pyRepr(p)) must be an absolute path (starting with / or ~) — a relative path is refused"
}

/// python's raise for an `fs` failure, as the kernel settles it: the clamp's
/// refusals are `ValueError`s, an OS failure the matching `OSError` subclass.
private func raised(_ error: Error) -> PyError {
    guard let e = error as? FsError else { return PyError("OSError", "\(error)") }
    let m = e.message
    if m.hasPrefix("[Errno "), let end = m.firstIndex(of: "]"),
        let code = Int32(m[m.index(m.startIndex, offsetBy: 7)..<end])
    {
        return PyError(pyOSErrorName(code), m)
    }
    return PyError("ValueError", m)
}

/// python `sorted(strs)` — by code point.
func pySorted(_ xs: [String]) -> [String] {
    xs.sorted { $0.unicodeScalars.lexicographicallyPrecedes($1.unicodeScalars) }
}

/// Every map's keys sorted, all the way down — PyYAML's `sort_keys=True`.
func sortedKeys(_ v: JSON) -> JSON {
    switch v {
    case .object(let o):
        var out: OrderedDictionary<String, JSON> = [:]
        for k in pySorted(Array(o.keys)) { out[k] = sortedKeys(o[k]!) }
        return .object(out)
    case .array(let a): return .array(a.map(sortedKeys))
    default: return v
    }
}

/// A host as stored and compared: the `host[:port]` of a URL or of a bare host,
/// lowercased; nil when blank. Python `_host` (`urlsplit(h).netloc`), raising the
/// `ValueError` urlsplit raises for an unbalanced IPv6 bracket.
func hostOf(_ h: JSON) throws(PyError) -> String? {
    guard case .string(let raw) = h else { return nil }
    var s = pyStrip(raw)
    if s.isEmpty { return nil }
    if s.contains("://") { s = try urlsplitNetloc(s) }
    let lower = s.lowercased()
    return lower.isEmpty ? nil : lower
}

/// python 3.11 `urllib.parse.urlsplit(url).netloc`: leading C0/space stripped,
/// TAB/CR/LF removed anywhere, an optional `scheme:` (an ASCII letter, then
/// letters, digits, `+-.`), then — only after `//` — everything up to the first
/// `/`, `?` or `#`. The bracketed-host and NFKC validations are not ported.
func urlsplitNetloc(_ url: String) throws(PyError) -> String {
    var u = Array(url.unicodeScalars)
    while let first = u.first, first.value <= 0x20 { u.removeFirst() }
    u.removeAll { $0 == "\t" || $0 == "\r" || $0 == "\n" }
    if let i = u.firstIndex(of: ":"), i > 0, u[0].isASCII,
        CharacterSet.letters.contains(u[0]),
        u[0..<i].allSatisfy({
            $0.isASCII && (CharacterSet.alphanumerics.contains($0) || "+-.".unicodeScalars.contains($0))
        })
    {
        u = Array(u[(i + 1)...])
    }
    guard u.count >= 2, u[0] == "/", u[1] == "/" else { return "" }
    let rest = u[2...]
    let end = rest.firstIndex { $0 == "/" || $0 == "?" || $0 == "#" } ?? u.endIndex
    var view = String.UnicodeScalarView()
    view.append(contentsOf: u[2..<end])
    let netloc = String(view)
    if netloc.contains("[") != netloc.contains("]") {
        throw PyError("ValueError", "Invalid IPv6 URL")
    }
    return netloc
}

/// A path as python's `PurePosixPath` holds it: empty and `.` segments dropped,
/// `..` kept; `parent` / `name` / `expanduser()`.
struct PurePath {
    let absolute: Bool
    let parts: [String]

    init(_ p: String) {
        absolute = p.hasPrefix("/")
        parts = p.split(separator: "/", omittingEmptySubsequences: true)
            .map(String.init).filter { $0 != "." }
    }

    private init(absolute: Bool, parts: [String]) {
        self.absolute = absolute
        self.parts = parts
    }

    var name: String { parts.last ?? "" }

    var parent: String {
        let head = parts.dropLast().joined(separator: "/")
        if absolute { return "/" + head }
        return head.isEmpty ? "." : head
    }

    /// python 3.11 `PosixPath.expanduser()`: a leading `~` is `$HOME` (else the
    /// password database), `~user` that user's home; anything else unchanged.
    func expandUser() throws(PyError) -> PurePath {
        guard !absolute, let first = parts.first, first.hasPrefix("~") else { return self }
        let user = String(first.dropFirst())
        var home: String?
        if user.isEmpty {
            home = ProcessInfo.processInfo.environment["HOME"]
            if home == nil, let pw = getpwuid(getuid()) { home = String(cString: pw.pointee.pw_dir) }
        } else if let pw = getpwnam(user) {
            home = String(cString: pw.pointee.pw_dir)
        }
        guard let home else { throw PyError("RuntimeError", "Can't determine home directory") }
        let h = PurePath(home)
        return PurePath(absolute: h.absolute, parts: h.parts + parts.dropFirst())
    }
}
