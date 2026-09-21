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
import OrderedCollections

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
    ///
    /// THROUGH THE BATCH (C47): ONE binary item, so the blob verbs, the message
    /// log and the ledger flush are ledger executions with call, task and settle
    /// rows — python's `persist_blob` is a plain `kernel.send([...])` and leaves
    /// exactly those. `sendWithBinary` minted none of them.
    func storeBookkeepingBinary(_ store: AgentId, _ header: JSON, _ blob: Data) async
        -> (JSON, Data?)
    {
        await asBookkeeping {
            let out = await self.send([Item(store, header, binary: blob)])[0]
            return (out.result, out.body)
        }
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
        // `name != Path(name).name` — python's own test, so a backslash (a legal
        // POSIX filename byte) is NOT a path segment here and `.` is.
        if name != pyPathName(name) {
            return (nil, "name \(pyRepr(name)) must be a single filename (no path segments)")
        }
        if name.hasPrefix(".") || name.contains("..") {
            return (nil, "name \(pyRepr(name)) must not be hidden or contain '..'")
        }
        if reservedBlobNames.contains(name.lowercased()) {
            return (nil, "name \(pyRepr(name)) is reserved (kernel-managed)")
        }
        return (name, nil)
    }

    /// `PurePosixPath(s).name` — the last component, empty parts and `.` parts
    /// dropped, `..` kept.
    static func pyPathName(_ s: String) -> String {
        let parts = s.split(separator: "/", omittingEmptySubsequences: true).filter { $0 != "." }
        return parts.last.map(String.init) ?? ""
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
            "\(pyRepr(caller.value)) may not use another agent's namespace \(pyRepr(aid)) —"
            + " an agent's blob verbs are scoped to itself; only an operator may name"
            + " a namespace it does not own"
    }

    /// Shared preamble: validate, authorize, resolve the store-relative path.
    ///
    /// PYTHON'S ORDER AND PYTHON'S TEXTS (`kernel_state._persist_blob` /
    /// `_load_blob`): agent_id, name, owner, then the store. Every refusal is a
    /// BARE `{error}` (plus `reason` on the owner denial) — these are the
    /// loader's own replies, not kernel refusals, so they carry no code and no
    /// `side`.
    private func resolveBlob(_ verb: String, _ payload: JSON) -> (
        ok: (store: AgentId, name: String, path: String)?, refusal: JSON?
    ) {
        guard let aid = payload["agent_id"].asString, !aid.isEmpty else {
            return (nil, .object(["error": .string("\(verb): agent_id (str) required")]))
        }
        let checked = Kernel.guardBlobName(payload["name"].asString)
        guard let name = checked.name else {
            return (
                nil, .object(["error": .string("\(verb): \(checked.error ?? "bad name")")])
            )
        }
        if let why = Kernel.guardBlobOwner(aid) {
            return (
                nil,
                .object([
                    "error": .string("\(verb): \(why)"), "reason": .string("unauthorized"),
                ])
            )
        }
        guard let store = findStore() else {
            return (
                nil,
                .object([
                    "error": .string("\(verb): no store wired — persistence not available")
                ])
            )
        }
        let reldir = blobReldir(aid)
        return ((store, name, reldir.isEmpty ? name : "\(reldir)/\(name)"), nil)
    }

    /// Store-relative dir for an agent's sidecars — `agents/<id>`. Prefers the
    /// LIVE agent's own dir; a record that is no longer live resolves under the
    /// root's children container, exactly as python's `_resolve_path` walk ends
    /// up for a parentless id. Python `_blob_reldir`.
    func blobReldir(_ agentId: String) -> String {
        guard let rootAgent = root else { return "" }
        let dir =
            agent(AgentId(agentId))?.rootPath
            ?? rootAgent.rootPath.appendingPathComponent(childrenDir)
            .appendingPathComponent(agentId)
        return storeReldir(storeRoot: rootAgent.rootPath, agentRoot: dir)
    }

    /// `persist_blob` — whole-file write of one sidecar.
    public func persistBlob(_ payload: JSON) async -> JSON {
        let r = resolveBlob("persist_blob", payload)
        // CONTENT IS CHECKED BETWEEN THE OWNER GUARD AND THE STORE LOOKUP in
        // python, so a contentless call with no store wired says `content (str)
        // required`, not `no store wired`. `resolveBlob` ends at the store, so
        // the content test runs on its refusal too when that refusal IS the
        // store one.
        let content = payload["content"].asString
        guard let ok = r.ok else {
            if content == nil,
                r.refusal?["error"].asString?.hasSuffix(
                    "no store wired — persistence not available") == true
            {
                return .object(["error": .string("persist_blob: content (str) required")])
            }
            return r.refusal ?? .null
        }
        guard let content else {
            return .object(["error": .string("persist_blob: content (str) required")])
        }
        // Operator authority: the loader writing to its OWN discovered store is
        // persistence bookkeeping, not the loader reaching out as an agent.
        let (reply, _) = await storeBookkeepingBinary(
            ok.store,
            .object([
                "type": .string("write_stream"), "path": .string(ok.path),
                "truncate": .bool(true),
            ]),
            Data(content.utf8))
        if pyTruthy(reply["error"]) {
            var out: OrderedDictionary<String, JSON> = [
                "error": .string("persist_blob: \(pyStr(reply["error"]))")
            ]
            // The provider's own machine field + recipe ride through ONLY when
            // truthy — a gate denial on the store must not be relabelled.
            if pyTruthy(reply["reason"]) { out["reason"] = reply["reason"] }
            if pyTruthy(reply["hint"]) { out["hint"] = reply["hint"] }
            return .object(out)
        }
        return .object([
            "ok": .bool(true), "name": .string(ok.name), "size": reply["size"],
        ])
    }

    /// `load_blob` — read one sidecar back, PUMPED TO EOF.
    ///
    /// `content: null` when the file is absent: a never-written sidecar reads
    /// empty rather than erroring, because "nothing saved yet" is the normal
    /// first state.
    public func loadBlob(_ payload: JSON) async -> JSON {
        let r = resolveBlob("load_blob", payload)
        guard let ok = r.ok else { return r.refusal ?? .null }
        var chunks = Data()
        var offset = 0
        var last: JSON = .null
        while true {
            let (reply, body) = await storeBookkeepingBinary(
                ok.store,
                .object([
                    "type": .string("read_stream"), "path": .string(ok.path),
                    "offset": .integer(Int64(offset)),
                ]), Data())
            last = reply
            // python tests `isinstance(reply.get("bytes"), bytes)` — on the swift
            // binary channel the body rides beside the header, and a refusal
            // carries none.
            if reply["error"] != .null { break }
            chunks.append(body ?? Data())
            if reply["eof"].asBool == true { return decodedBlob(ok.name, chunks) }
            guard let next = reply["next_offset"].asInt, Int(next) > offset else {
                // No forward progress: stop rather than spin. Returning what we
                // have would be the truncation this loop exists to prevent.
                return .object([
                    "error": .string(
                        "load_blob: \(ok.name) stalled at offset \(offset)"
                            + " — the store returned no forward progress"),
                    "name": .string(ok.name), "content": .null,
                ])
            }
            offset = Int(next)
        }
        if !chunks.isEmpty { return decodedBlob(ok.name, chunks) }
        if last["reason"].asString == "unauthorized" {
            var out: OrderedDictionary<String, JSON> = [
                "error": .string(
                    "load_blob: "
                        + (last["error"] == .null ? "unauthorized" : pyStr(last["error"]))),
                "reason": .string("unauthorized"),
            ]
            if pyTruthy(last["hint"]) { out["hint"] = last["hint"] }
            return .object(out)
        }
        return .object(["name": .string(ok.name), "content": .null])  // absent → empty
    }

    /// Decode a sidecar STRICTLY. A non-text file handed back as U+FFFD mojibake
    /// is indistinguishable from a real read, and writing that back destroys the
    /// file. Python `_decoded_blob`.
    private func decodedBlob(_ name: String, _ raw: Data) -> JSON {
        if let text = String(data: raw, encoding: .utf8) {
            return .object(["name": .string(name), "content": .string(text)])
        }
        return .object([
            "error": .string("load_blob: \(name) is not utf-8 at byte \(firstBadUTF8(raw))"),
            "reason": .string("bad_encoding"),
            "name": .string(name),
            "content": .null,
        ])
    }

    /// The byte offset python's `UnicodeDecodeError.start` names — the first
    /// byte of the first sequence that does not decode.
    private func firstBadUTF8(_ raw: Data) -> Int {
        var i = 0
        let bytes = [UInt8](raw)
        while i < bytes.count {
            let b = bytes[i]
            let len: Int
            if b < 0x80 {
                len = 1
            } else if b >= 0xC2 && b <= 0xDF {
                len = 2
            } else if b >= 0xE0 && b <= 0xEF {
                len = 3
            } else if b >= 0xF0 && b <= 0xF4 {
                len = 4
            } else {
                return i
            }
            if i + len > bytes.count { return i }
            if String(bytes: bytes[i..<(i + len)], encoding: .utf8) == nil { return i }
            i += len
        }
        return i
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
        // python `json.dumps(...)` DEFAULTS: ensure_ascii, compact separators —
        // `_ledger_dump` passes nothing else, so the bytes on disk are the same
        // on every runtime.
        let doc = pyJsonDumps(ledger.settledSnapshot())
        // THE FLUSH IS AN EXECUTION, and its settle re-kicks the flusher (D6 /
        // C47). Python behaves exactly so — `flush_ledger_now` writes through
        // `_persist_blob`, which is `kernel.send([...])`, whose settle calls the
        // flusher's `kick` — so with a store wired `ledger.json` is rewritten
        // continuously and a `ledger_flush` task stays in `ps`. That loop is the
        // measured python behaviour, not an accident of this port.
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
        let doc: JSON
        do {
            doc = try JSON.parse(text)
        } catch {
            // The tail is this parser's own message; python prints its
            // `json.JSONDecodeError` str, which no other parser reproduces.
            print("  [ledger] \(ledgerBlob) unreadable, starting empty: \(error)")
            return 0
        }
        // THE UPGRADE CONTRACT, from day one: an unknown version starts empty
        // LOUDLY — never guessed at, never silently reinterpreted. A v1 file (an
        // object of rows keyed by a stringified id) is one of those.
        guard doc["v"].asInt == Int64(ledgerSidecarVersion) else {
            print(
                "  [ledger] \(ledgerBlob) is version \(pyRepr(doc["v"])), this kernel "
                    + "reads \(ledgerSidecarVersion) — starting empty (no migration written)")
            return 0
        }
        let n = ledger.rehydrate(doc)
        // ONE ID SPACE: a settled execution's id is a row id too, so the
        // counter must start above every one of them. Never lowered.
        if let top = ledger.highestId() { rows.setNextId(top + 1) }
        return n
    }

    /// Rehydrate, then install the write-through.
    ///
    /// SILENT ON SUCCESS. A clean rehydrate prints nothing — python says nothing
    /// either, and only its loud lines (an unknown version, an unreadable file)
    /// survive here.
    public func installLedgerPersistence() async {
        _ = await rehydrateLedger()
        let gate = LedgerFlushGate()
        setLedgerFlusher { [weak self] in
            guard let self else { return }
            // THE WRITER IS CHOSEN BEFORE THE TASK IS SPAWNED, as python does
            // (`kick` returns under `if state["running"]` and only then reaches
            // `spawn_task`). Deciding INSIDE the task minted a `ledger_flush`
            // task row per settle; each row is a row the message pump logs, the
            // log write is an execution, its settle kicks the flusher again —
            // 127 `ledger_flush` rows against python's 7 for the same four
            // commands (measured 2026-09-20).
            guard gate.mark() else { return }
            // EACH FLUSH PASS IS A REGISTERED KERNEL TASK on the root, verb
            // `ledger_flush`, parented to the settling dispatch — python spawns
            // it through `spawn_task` (`tools.py:977-1000`), so it is in `ps`,
            // it is cancellable, and the message pump can recognise and skip its
            // own persistence traffic. A bare `Task` was invisible to all three.
            // NOT keepalive: a pass ends.
            let root = self.root?.id ?? AgentId("kernel")
            self.spawnTask(agent: root, verb: "ledger_flush") {
                // ONE writer, and always a pass AFTER the last settle.
                repeat {
                    gate.beginPass()
                    await self.flushLedger()
                } while gate.needsAnother()
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
/// One lock rather than two atomics: `mark` and `needsAnother` must decide
/// together — a settle observed after `needsAnother` releases the writer starts
/// a fresh one, and one observed before it is caught by the next lap. A LOCK and
/// not an actor, because `mark` is python's synchronous `kick`: the writer has
/// to be chosen before the flush task is spawned, or the spawn itself is a row.
final class LedgerFlushGate: @unchecked Sendable {
    private let lock = NSLock()
    private var dirty = false
    private var running = false

    /// Record a change. Returns true if THIS caller becomes the writer.
    func mark() -> Bool {
        lock.withLock {
            dirty = true
            if running { return false }
            running = true
            return true
        }
    }

    /// Clear the flag BEFORE snapshotting, so a settle landing during the pass
    /// re-dirties and is caught rather than dropped.
    func beginPass() { lock.withLock { dirty = false } }

    /// True if more arrived during the pass. When false, the writer is released.
    func needsAnother() -> Bool {
        lock.withLock {
            if dirty { return true }
            running = false
            return false
        }
    }
}
