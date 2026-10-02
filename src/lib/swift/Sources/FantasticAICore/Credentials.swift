// A backend's KEY, read from a credential store at USE time (owner, 2026-10-01).
// Python `ai_core/credentials.py`, its texts byte for byte.
//
// The record names WHERE the key lives — `credentials: {store, ref}` — never the
// value, and nothing reads the environment. Before EVERY request a provider calls
// its reader with the endpoint it is about to call; the reader asks the store AS
// THIS AGENT for `api_key`, the answer marked secret (the value lands in no row,
// ledger or stream) and pinned to that endpoint's host (the store refuses a ref
// bound to another host). Nothing is cached: a key rotated in the store is used by
// the next request, with no restart.
//
// No `credentials` on the record ⇒ no key — for a keyless local server. A backend
// that cannot run without one marks the field required in its `RECORD_SCHEMA`, and
// the reader refuses its absence loud as well.
//
// Wiring, by the operator: the agent's `can_call` names the store, the store's
// `callable_by` names the agent, and the ref's `readers` name it — the store
// answers "no such ref" otherwise.
//
// A MODEL NEVER RECEIVES A SECRET (owner, 2026-10-02, option B). The grant that lets
// the backend read its key would let the agent's MODEL send the same `get` and read
// the value into its prompt and memory. So an agent whose record carries
// `holds_secrets: true` (a `credential_store` stamps it on itself; an operator may
// set it on any agent) is unreachable from a model turn: `routeReply` — the ONE
// place a model-parsed call is dispatched — asks `secretHolderRefusal` for every
// call's target and `reply_to` and refuses a holder, loud, before anything is sent.
// The address is resolved the way the kernel's `send` resolves it first, so
// `<this kernel's guid>|<holder>` is the holder, not an unknown id.
// The reader below is untouched: it calls `kernel.send` itself, never through the
// model's tool dispatch.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// The credential field a backend sends as its key. Python `FIELD`.
public let CREDENTIAL_FIELD = "api_key"

/// The record field that marks an agent a SECRET HOLDER — reachable by backends,
/// never by a model turn. Python `HOLDS_SECRETS`.
public let HOLDS_SECRETS = "holds_secrets"

/// What the model reads when a call of its names a secret holder; `{agent}` is the
/// holder's id. Python `SECRET_HOLDER_TEXT`.
public let SECRET_HOLDER_TEXT =
    "{agent} holds secrets — a secret holder answers backends, never a model "
    + "turn; this call was not dispatched"

/// The agent (and so the record) `address` names on THIS kernel, resolved the
/// way `send` resolves a target: a local id is itself; the reserved `kernel` (no
/// agent may take that id) is the root; `<this kernel's guid>|<id>` is that local
/// agent (`<this kernel's guid>|kernel` the root). A connected peer's
/// `<guid>|<id>` and an unknown id have no record here: `nil`. Python
/// `_local_record`.
func localRecord(_ kernel: Kernel, _ address: String) -> Agent? {
    if let agent = kernel.resolveAgent(address) { return agent }
    guard let bar = address.firstIndex(of: "|") else { return nil }
    let guid = String(address[..<bar])
    let local = String(address[address.index(after: bar)...])
    if guid.isEmpty || local.isEmpty || local.contains("|") { return nil }
    guard let root = kernel.root, root.metaValue(forKey: "guid")?.asString == guid else {
        return nil
    }
    return local == "kernel" ? root : kernel.agent(AgentId(local))
}

/// The refusal for a model-parsed call that names `agentId` (as its target or its
/// `reply_to`), or nil when the call may go. Record-driven: only an agent whose
/// record says `holds_secrets: true` — exactly the JSON boolean — is refused.
/// Python `secret_holder_refusal`.
///
/// The address is resolved FIRST, as the kernel resolves it (`localRecord`):
/// `<this kernel's guid>|<holder>` reaches the holder, so it is refused like the
/// bare id. Only what THIS kernel can see is judged: a connected peer's
/// `<guid>|<agent>` lives on another kernel and its record is not here, so it
/// passes; that kernel's own AI layer guards its own holders, and a holder's value
/// is only ever sent to a caller that marked it secret. The refusal names the
/// address as the model wrote it.
public func secretHolderRefusal(_ kernel: Kernel, _ agentId: String) -> JSON? {
    guard localRecord(kernel, agentId)?.metaValue(forKey: HOLDS_SECRETS) == .bool(true)
    else { return nil }
    return Refusal.make(
        Refusal.unauthorized,
        SECRET_HOLDER_TEXT.replacingOccurrences(of: "{agent}", with: agentId),
        extra: [("agent", .string(agentId))])
}

/// What a provider awaits before each request: `read(endpoint) -> key?`. THROWS
/// a `CredentialError` (python's class, a `RuntimeError`) when the store refuses
/// or the field is missing, and when a required key has no `credentials`.
public typealias KeyReader = @Sendable (_ endpoint: String) async throws -> String?

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
        var item = Item(
            AgentId(store),
            .object(["type": .string("get"), "ref": .string(ref), "host": .string(endpoint)]))
        item.secret = .array([.string("result")])
        // AS ITSELF: the store's per-ref `readers` name this agent, whatever task
        // the request runs in.
        let out = await KernelTaskLocals.$currentSender.withValue(selfId) {
            await kernel.send([item])[0]
        }
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
        return k
    }
}
