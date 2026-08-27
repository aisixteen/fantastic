// THE BLOB CHANNEL + the DURABLE LEDGER — sidecars, persisted through the store.
//
// Port of python `kernel_state._persist_blob`/`_load_blob` (#10) and the durable
// ledger (#37).
//
// The blob channel is a dumb BYTE authority: no data semantics — the caller owns
// its format. What the consumer gets is that it never computes a path and never
// wires a provider of its own; it names ITSELF and a filename, and the loader
// puts the bytes at `agents/<agent_id>/<name>` in whatever store is discovered.
//
// No store wired ⇒ an ERROR, not RAM. The tree's auto-flush law says "no
// provider, no persistence"; a sidecar verb that silently succeeded into nowhere
// would be the opposite of that — the caller believes it saved.

import FantasticJSON
import Foundation

/// Files the KERNEL manages. A sidecar may not overwrite them.
private let reservedBlobNames: Set<String> = ["agent.json", "readme.md"]

/// The sidecar holding SETTLED executions, store-relative at the root.
public let ledgerBlob = "ledger.json"

extension Kernel {
    /// A store I/O send made as the KERNEL'S OWN BOOKKEEPING.
    ///
    /// Two locals are cleared, and both matter:
    /// - `currentSender` = nil says the KERNEL is acting, not the agent that
    ///   asked — so the write never runs against that agent's grant.
    /// - `currentKilled` = nil says this is not part of the CALLER'S killed
    ///   subtree. Without it, cancelling a turn silently swallowed a record the
    ///   kernel had already committed to writing, and the history read back
    ///   EMPTY — a cancel must not corrupt what was already durable.
    func storeBookkeeping(_ store: AgentId, _ payload: JSON) async -> JSON {
        await KernelTaskLocals.$currentKilled.withValue(nil) {
            await KernelTaskLocals.$currentSender.withValue(nil) {
                await send([Item(store, payload)])[0].result
            }
        }
    }

    /// Validate a blob filename: one bare segment, not kernel-managed, no escape
    /// or hidden name.
    ///
    /// Defence in depth over the provider's own clamp, so a bad `name` fails AT
    /// the loader with a loader-scoped message instead of surfacing as a deep fs
    /// error the caller cannot act on.
    static func guardBlobName(_ name: String?) -> (name: String?, error: String?) {
        guard let name, !name.isEmpty else { return (nil, "name (non-empty str) required") }
        if name.contains("/") || name.contains("\\") {
            return (nil, "name \"\(name)\" must be a single filename (no path segments)")
        }
        if name.hasPrefix(".") || name.contains("..") {
            return (nil, "name \"\(name)\" must not be hidden or contain '..'")
        }
        if reservedBlobNames.contains(name.lowercased()) {
            return (nil, "name \"\(name)\" is reserved (kernel-managed)")
        }
        return (name, nil)
    }

    /// Refuse a blob verb that names SOMEONE ELSE'S namespace.
    ///
    /// `agent_id` is the namespace and it arrives in the PAYLOAD — so without
    /// this, any agent granted `persist_blob` could write any OTHER agent's
    /// sidecar. Measured 2026-08-23 (python): an agent holding exactly
    /// `can_call: {loader: [persist_blob]}` overwrote another agent's
    /// `state.yaml`, and the victim read the forged content back as its own
    /// memory.
    ///
    /// The defence was believed to be POSITION — the root accepts `$children`,
    /// and a grandchild is not one — but a DIRECT child passes that check, and
    /// every agent an operator creates at the root is a direct child. Position
    /// bounds who can REACH the verb; it says nothing about which namespace they
    /// may NAME.
    ///
    /// An OPERATOR send (origin nil: the loader's own flush, an agent persisting
    /// itself under operator authority, the CLI) is the bookkeeping this verb
    /// exists for and stays ungated — an agent cannot forge it. An AGENT may only
    /// name ITSELF.
    static func guardBlobOwner(_ aid: String) -> String? {
        guard let caller = KernelTaskLocals.currentOrigin else { return nil }
        if caller.value == aid { return nil }
        return
            "\(caller.value) may not use another agent's namespace \"\(aid)\" — an agent's "
            + "blob verbs are scoped to itself; only an operator may name a namespace it "
            + "does not own"
    }

    /// Shared preamble: validate, authorize, resolve the store-relative path.
    private func resolveBlob(_ verb: String, _ payload: JSON) -> (
        ok: (store: AgentId, path: String)?, refusal: JSON?
    ) {
        guard let aid = payload["agent_id"].asString, !aid.isEmpty else {
            return (nil, Refusal.missing(verb, "agent_id"))
        }
        let checked = Kernel.guardBlobName(payload["name"].asString)
        guard let name = checked.name else {
            return (
                nil,
                Refusal.make(
                    Refusal.badArg, "\(verb): \(checked.error ?? "bad name")",
                    extra: [("arg", .string("name"))])
            )
        }
        if let why = Kernel.guardBlobOwner(aid) {
            return (
                nil,
                Refusal.make(
                    Refusal.unauthorized, "\(verb): \(why)",
                    extra: [("side", .string("ingress"))])
            )
        }
        guard let store = findStore() else {
            return (
                nil,
                Refusal.make(
                    Refusal.notHere, "\(verb): no store wired — persistence not available")
            )
        }
        guard let rootAgent = root, let a = agent(AgentId(aid)) else {
            return (nil, Refusal.unknown(aid, live: allAgents().map { $0.id.value }))
        }
        let reldir = storeReldir(storeRoot: rootAgent.rootPath, agentRoot: a.rootPath)
        return ((store, reldir.isEmpty ? name : "\(reldir)/\(name)"), nil)
    }

    /// `persist_blob` — whole-file write of one sidecar.
    func persistBlob(_ payload: JSON) async -> JSON {
        let store: AgentId
        let path: String
        let r = resolveBlob("persist_blob", payload)
        guard let ok = r.ok else { return r.refusal ?? .null }
        (store, path) = (ok.store, ok.path)
        guard let content = payload["content"].asString else {
            return Refusal.make(
                Refusal.badArg, "persist_blob: content (str) required",
                extra: [("arg", .string("content"))])
        }
        // Operator authority: the loader writing to its OWN discovered store is
        // persistence bookkeeping, not the loader reaching out as an agent.
        let reply = await storeBookkeeping(store, .object([
                        "type": .string("write"), "path": .string(path),
                        "content": .string(content),
                    ]))
        if let err = reply["error"].asString {
            var out = Refusal.make(Refusal.transportError, "persist_blob: \(err)")
            // Carry the provider's own machine field + recipe through — a gate
            // denial on the store must not be relabelled as a transport fault.
            if case .object(var o) = out {
                if reply["reason"] != .null { o["reason"] = reply["reason"] }
                if reply["hint"] != .null { o["hint"] = reply["hint"] }
                out = .object(o)
            }
            return out
        }
        return .object([
            "ok": .bool(true), "name": payload["name"], "size": reply["size"],
        ])
    }

    /// `load_blob` — read one sidecar back.
    ///
    /// `content: null` when the file is absent: a never-written sidecar reads
    /// empty rather than erroring, because "nothing saved yet" is the normal
    /// first state.
    func loadBlob(_ payload: JSON) async -> JSON {
        let store: AgentId
        let path: String
        let r = resolveBlob("load_blob", payload)
        guard let ok = r.ok else { return r.refusal ?? .null }
        (store, path) = (ok.store, ok.path)
        let reply = await storeBookkeeping(
            store, .object(["type": .string("read"), "path": .string(path)]))
        if reply["error"].asString != nil {
            return .object(["name": payload["name"], "content": .null])
        }
        return .object([
            "name": payload["name"],
            "content": reply["content"].asString.map { JSON.string($0) } ?? .null,
        ])
    }

    // ── the DURABLE LEDGER (#37) ──────────────────────────────────

    /// Write every settled row through the discovered store.
    ///
    /// No store ⇒ RAM only. That is the AUTO-FLUSH LAW, not a fallback: with
    /// nothing wired the tree does not persist either, and the ledger follows the
    /// same rule rather than inventing a second disk surface.
    public func flushLedger() async {
        if storage.isInMemory { return }
        guard let store = findStore() else { return }
        let doc = ledger.settledSnapshot().serialize()
        _ = await storeBookkeeping(
            store,
            .object([
                "type": .string("write"), "path": .string(ledgerBlob),
                "content": .string(doc),
            ]))
    }

    /// Restore settled executions at boot, so dedup holds across a restart: a
    /// completed step is never re-executed by a fresh process and a parked dead
    /// letter stays dead.
    ///
    /// Runs BEFORE the flusher is installed, so hydration never rewrites what it
    /// just read. Every failure is LOUD and starts empty — a ledger that silently
    /// half-loads would re-arm exactly the side effects it exists to prevent.
    @discardableResult
    public func rehydrateLedger() async -> Int {
        guard let store = findStore() else { return 0 }
        let reply = await storeBookkeeping(
            store, .object(["type": .string("read"), "path": .string(ledgerBlob)]))
        guard let text = reply["content"].asString, !text.isEmpty else { return 0 }
        guard let doc = try? JSON.parse(text) else {
            print("  [ledger] \(ledgerBlob) unreadable, starting empty")
            return 0
        }
        // An unknown version is not a migration we can guess at.
        guard doc["v"].asInt == 1 else {
            print(
                "  [ledger] \(ledgerBlob) is version \(doc["v"].asInt.map(String.init) ?? "?"), "
                    + "this kernel reads 1 — starting empty (no migration written)")
            return 0
        }
        return ledger.rehydrate(doc)
    }

    /// Rehydrate, then install the write-through.
    public func installLedgerPersistence() async {
        let n = await rehydrateLedger()
        if n > 0 { print("  [ledger] rehydrated \(n) settled execution(s)") }
        setLedgerFlusher { [weak self] in
            guard let self else { return }
            Task { await self.flushLedger() }
        }
    }
}
