// A backend's KEY, read from a credential store at USE time.
// Python `ai_core/credentials.py`, its texts byte for byte.
//
// The record names WHERE the key lives — `credentials: {store, ref}` — never the
// value, and nothing reads the environment. Before EVERY request a provider calls
// its reader with the endpoint it is about to call; the reader asks the store for
// `api_key`, pinned to that endpoint's host (the store refuses a ref bound to
// another host). The request runs in this agent's own turn (its dispatch, or a
// task it spawned), so the read is this agent's send, gated as it. Nothing is
// cached: a key rotated in the store is used by the next request, with no
// restart.
//
// No `credentials` on the record ⇒ no key — for a keyless local server. A backend
// that cannot run without one marks the field required in its `RECORD_SCHEMA`, and
// the reader refuses its absence loud as well.
//
// Wiring, by the operator: the agent's `can_call` names the store, the store's
// `callable_by` names the agent.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// The credential field a backend sends as its key. Python `FIELD`.
public let CREDENTIAL_FIELD = "api_key"

/// What a provider awaits before each request: `read(endpoint) -> key?`. THROWS
/// a `CredentialError` (python's class, a `RuntimeError`) when the store refuses,
/// the field is missing or is not a legal HTTP header value (`headerSafe`), and
/// when a required key has no `credentials`.
public typealias KeyReader = @Sendable (_ endpoint: String) async throws -> String?

/// Whether `key` can ride `Authorization: Bearer <key>` as it is: visible ASCII,
/// spaces or tabs only BETWEEN visible characters. Python `header_safe`: an HTTP
/// client refuses anything else, so a key pasted with a trailing newline is
/// refused here, named by its ref.
public func headerSafe(_ key: String) -> Bool {
    let edge: (Unicode.Scalar) -> Bool = { $0 == " " || $0 == "\t" }
    let scalars = key.unicodeScalars
    if let f = scalars.first, edge(f) { return false }
    if let l = scalars.last, edge(l) { return false }
    return scalars.allSatisfy { edge($0) || (0x21...0x7E).contains($0.value) }
}

/// Why a `credentials` value is malformed, or nil: a map of exactly a non-empty
/// `store` id and a non-empty `ref`. Python `shape_error`.
public func credentialsShapeError(_ v: JSON) -> String? {
    guard case .object(let o) = v, Set(o.keys) == ["store", "ref"] else {
        return "credentials must be {store: <credential_store id>, ref: <ref>}"
    }
    for k in ["store", "ref"] {
        guard case .string(let s)? = o[k], !pyStrip(s).isEmpty else {
            return "credentials.\(k) must be a non-empty string"
        }
    }
    return nil
}

/// The reader a provider awaits before each request. Python `key_reader(self_id,
/// kernel, backend, required=…)`.
public func keyReader(
    _ selfId: AgentId, _ kernel: Kernel, backend: String, required: Bool
) -> KeyReader {
    { endpoint in
        func fail(_ text: String) -> PyError { PyError("CredentialError", text) }
        let cred = kernel.agent(selfId)?.metaValue(forKey: "credentials") ?? .null
        if cred == .null {
            if required {
                throw fail(
                    "\(backend): no `credentials` on \(selfId.value) — name the key's "
                        + "credential store and ref (update_agent "
                        + "\(selfId.value) credentials={store: <id>, ref: <ref>})")
            }
            return nil
        }
        if let bad = credentialsShapeError(cred) { throw fail("\(backend): \(bad)") }
        let store = cred["store"].asString ?? ""
        let ref = cred["ref"].asString ?? ""
        // The agent's own turn (its dispatch or a task it spawned): the read is
        // this agent's send, gated as it.
        let out = await kernel.send([
            Item(
                AgentId(store),
                .object([
                    "type": .string("get"), "ref": .string(ref), "host": .string(endpoint),
                ]))
        ])[0]
        let res = out.result
        if out.status != .completed || res.asObject == nil || pyTruthy(res["error"]) {
            let err = res.asObject != nil ? pyStr(res["error"]) : out.status.rawValue
            throw fail("\(backend): credentials \(store)/\(ref): \(err)")
        }
        let fields = res["fields"]
        let key = pyTruthy(fields) ? fields[CREDENTIAL_FIELD] : .null
        guard case .string(let k) = key, !k.isEmpty else {
            throw fail(
                "\(backend): credential \(pyRepr(ref)) in \(pyRepr(store)) has no "
                    + "\(pyRepr(CREDENTIAL_FIELD)) field")
        }
        // named by its ref, never by its value
        guard headerSafe(k) else {
            throw fail(
                "\(backend): credential \(pyRepr(ref)) in \(pyRepr(store)): "
                    + "\(pyRepr(CREDENTIAL_FIELD)) is not a legal HTTP header value (a control "
                    + "or non-ASCII character, or whitespace at an edge) — store it again without")
        }
        return k
    }
}
