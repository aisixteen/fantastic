// THE BLOB CHANNEL — sidecars, persisted through the store.
//
// Port of python `kernel_state._persist_blob`/`_load_blob` (#10). There is no
// `ledger.json` any more (deleted 2026-09-24): the execution ledger IS the
// `call`+`settle` rows of `messages.yaml`, rebuilt by `hydrateExecutions`.
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
private let reservedBlobNames: Set<String> = [RECORD_FILE, "readme.md"]

extension Kernel {
    /// The kernel's BOOKKEEPING CALL to the store a sidecar persists through —
    /// a plain call into the store's handler (`storeCall`), never a send: the
    /// blob verbs are the loader's own, already gated where they were called.
    func storeBookkeeping(_ store: AgentId, _ payload: JSON) async -> JSON {
        await storeCall(store, payload).0
    }

    /// The BINARY bookkeeping edge: a `write_stream`/`read_stream` to the store
    /// over the raw-bytes channel (never base64). `body` is the request's chunk
    /// (the payload's `bytes`); `nil` asks for the reply's chunk raw. The chunk
    /// comes back when the reply holds it at `bytes` — python reads
    /// `reply["bytes"]`.
    func storeBookkeepingBinary(_ store: AgentId, _ header: JSON, _ body: Data?) async
        -> (JSON, Data?)
    {
        await storeCallBinary(store, header, body)
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

    /// Shared preamble: validate, authorize, resolve the store-relative path.
    ///
    /// PYTHON'S ORDER AND PYTHON'S TEXTS (`kernel_state._persist_blob` /
    /// `_load_blob`): agent_id, name, then the store. Every refusal is a
    /// BARE `{error}` — these are the
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

    /// AN AGENT PERSISTING ITS OWN SIDECAR — a plain kernel function, no send
    /// (the kernel's persistence bookkeeping): the namespace is the agent whose
    /// handler (or spawned task) runs this, so it can name no other. Outside
    /// every dispatch there is no agent to persist for — refused.
    public func persistOwnBlob(name: String, content: String) async -> JSON {
        guard let me = KernelTaskLocals.currentSender else {
            return .object(["error": .string("persist_blob: no agent is running this — an agent persists its own sidecar")])
        }
        return await persistBlob(
            .object([
                "type": .string("persist_blob"), "agent_id": .string(me.value),
                "name": .string(name), "content": .string(content),
            ]))
    }

    /// `loadOwnBlob` — `persistOwnBlob`'s read: one of the running agent's own
    /// sidecars.
    public func loadOwnBlob(name: String) async -> JSON {
        guard let me = KernelTaskLocals.currentSender else {
            return .object(["error": .string("load_blob: no agent is running this — an agent loads its own sidecar")])
        }
        return await loadBlob(
            .object([
                "type": .string("load_blob"), "agent_id": .string(me.value),
                "name": .string(name),
            ]))
    }

    /// `persist_blob` — whole-file write of one sidecar.
    public func persistBlob(_ payload: JSON) async -> JSON {
        let r = resolveBlob("persist_blob", payload)
        // CONTENT IS CHECKED BETWEEN THE NAME GUARD AND THE STORE LOOKUP in
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
        // The loader writing to its OWN discovered store is persistence
        // bookkeeping, a plain call into the store, not the loader reaching out.
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
                ]), nil)
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
}
