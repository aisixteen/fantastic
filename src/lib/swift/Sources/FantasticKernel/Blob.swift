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
        await asBookkeeping { await self.send([Item(store, payload)])[0].result }
    }

    /// The BINARY bookkeeping edge: a `write_stream`/`read_stream` to the store
    /// over the raw-bytes channel (never base64), under operator authority. The
    /// file_bridge string `read`/`write` verbs are GONE — a blob rides the stream
    /// verbs like any other file (mirrors python `persist_blob`/`load_blob`).
    func storeBookkeepingBinary(_ store: AgentId, _ header: JSON, _ blob: Data) async
        -> (JSON, Data)
    {
        await asBookkeeping { await self.sendWithBinary(store, header, blob) }
    }

    /// Run `body` as the KERNEL'S OWN BOOKKEEPING — the ONE definition of that
    /// scope, public because callers OUTSIDE the substrate (the AI core's
    /// `save_history`) do bookkeeping too.
    ///
    /// It exists as one function because it was written twice and drifted:
    /// `storeBookkeeping` cleared both locals, `save_history` cleared only the
    /// sender, and the second one was a live data-loss bug (measured 2026-08-27:
    /// cancelling a turn mid-stream swallowed the user row the backend had
    /// already committed, and the chat read back EMPTY). Two copies of a rule is
    /// one copy that will be missed on the next fix.
    public func asBookkeeping<T: Sendable>(_ body: @Sendable () async -> T) async -> T {
        await KernelTaskLocals.$currentKilled.withValue(nil) {
            await KernelTaskLocals.$currentSender.withValue(nil) {
                await body()
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
        let (reply, _) = await storeBookkeepingBinary(
            store,
            .object([
                "type": .string("write_stream"), "path": .string(path),
                "truncate": .bool(true),
            ]),
            Data(content.utf8))
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
        switch await readWholeViaStore(store, path) {
        case .refused(let r):
            // ABSENT is `content: null`; anything else is the leg's own refusal
            // and must propagate — a sealed store used to read as an empty file.
            if r["reason"].asString == "not_found" || r["reason"] == .null {
                return .object(["name": payload["name"], "content": .null])
            }
            return r
        case .bytes(let data):
            guard let text = String(data: data, encoding: .utf8) else {
                return Refusal.make(
                    Refusal.badArg,
                    "load_blob: \(path) is not valid utf-8",
                    extra: [("name", payload["name"]), ("content", .null)])
            }
            return .object(["name": payload["name"], "content": .string(text)])
        }
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
        _ = await storeBookkeepingBinary(
            store,
            .object([
                "type": .string("write_stream"), "path": .string(ledgerBlob),
                "truncate": .bool(true),
            ]),
            Data(doc.utf8))
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
        guard case .bytes(let data) = await readWholeViaStore(store, ledgerBlob),
            let text = String(data: data, encoding: .utf8), !text.isEmpty
        else { return 0 }
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
        let n = ledger.rehydrate(doc)
        // ONE ID SPACE: a settled execution's id is a row id too, so the
        // counter must start above every one of them. Never lowered.
        if let top = ledger.highestId() { rows.setNextId(top + 1) }
        return n
    }

    /// Rehydrate, then install the write-through.
    public func installLedgerPersistence() async {
        let n = await rehydrateLedger()
        if n > 0 { print("  [ledger] rehydrated \(n) settled execution(s)") }
        let gate = LedgerFlushGate()
        setLedgerFlusher { [weak self] in
            guard let self else { return }
            Task {
                // ONE writer, and always a pass AFTER the last settle.
                guard await gate.mark() else { return }
                repeat {
                    await gate.beginPass()
                    await self.flushLedger()
                } while await gate.needsAnother()
            }
        }
    }
}

/// Serialises ledger flushing: one writer at a time, and a final pass after the
/// last settle.
///
/// `flushLedger` rewrites the WHOLE settled map, so spawning one per settle —
/// which is what `Task { await flushLedger() }` did — means an agentic run
/// stacks O(n) concurrent rewrites of an O(n) snapshot. Rust had the same shape
/// with a flag it cleared before awaiting, and it saturated multiple cores on
/// `agentic.plan` until the writer was serialised.
///
/// An actor rather than two atomics: `mark` and `needsAnother` must decide
/// together, and the actor gives that for free — a settle observed after
/// `needsAnother` releases the writer starts a fresh one, and one observed
/// before it is caught by the next lap.
actor LedgerFlushGate {
    private var dirty = false
    private var running = false

    /// Record a change. Returns true if THIS caller becomes the writer.
    func mark() -> Bool {
        dirty = true
        if running { return false }
        running = true
        return true
    }

    /// Clear the flag BEFORE snapshotting, so a settle landing during the pass
    /// re-dirties and is caught rather than dropped.
    func beginPass() { dirty = false }

    /// True if more arrived during the pass. When false, the writer is released.
    func needsAnother() -> Bool {
        if dirty { return true }
        running = false
        return false
    }
}
