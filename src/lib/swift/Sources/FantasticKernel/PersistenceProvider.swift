// Persistence inversion — records persist THROUGH a discovered file_bridge.
//
// Mirrors py `kernel_state` + rust `persistence.rs`: the substrate no longer
// owns an `fs` surface for records. It persists THROUGH a DISCOVERED
// `file_bridge` provider's `read_stream`/`write_stream` verbs over the raw-bytes
// binary channel. NO FALLBACKS: no provider ⇒
// the live tree stays in RAM (lost on restart until a store is wired). Cold
// primitives (boot read, root seed) stay direct — the chicken-egg bring-up.

import FantasticJSON
import Foundation
import OrderedCollections

extension Kernel {
    /// Canonicalize a filesystem path for comparison: resolve symlinks, then
    /// normalize the macOS `/private/var` ≡ `/var` alias. Temp dirs resolve to
    /// one form and constructed agent rootPaths to the other, so a raw prefix
    /// check fails and records land doubly-nested under the absolute path. No-op
    /// for ordinary paths (`/Users/…`, Linux). THE one path-comparison helper —
    /// every containment check (store discovery, store-relative sidecar dirs,
    /// the file_bridge running-dir clamp) goes through it; never compare raw
    /// `.path` strings across differently-derived URLs.
    public static func canonPath(_ path: String) -> String {
        let resolved = (path as NSString).resolvingSymlinksInPath
        return resolved.hasPrefix("/private/") ? String(resolved.dropFirst(8)) : resolved
    }

    /// DISCOVER the persistence provider — the first `file_bridge.tools` CHILD of
    /// root whose `root` resolves to the root loader's own store dir (its
    /// `.fantastic`). Bound by MATCH, operator-wired, not composed. Returns its
    /// id, or `nil` when none is wired (⇒ RAM). No fallback.
    public func findStore() -> AgentId? {
        guard let rootAgent = root, let workdir = self.workdir else { return nil }
        let want = Self.canonPath(rootAgent.rootPath.path)
        // A relative provider root resolves under the kernel's running dir (the
        // ONE source — `Kernel.workdir`).
        // SUBTREE, not direct children, BREADTH-FIRST.
        //
        // One level was scanned for a long time and the consequence was
        // invisible: a provider declared through a `static_loader` manifest lands
        // as a GRANDCHILD (a child of the roster loader, not of the root), was
        // never discovered, and the flush then no-opped with no error anywhere —
        // the tree silently stayed in RAM. Declaring it was never the problem;
        // FINDING it was.
        //
        // Breadth-first so the shallowest match still wins, which keeps the
        // previous behaviour whenever a direct child matches.
        var queue = rootAgent.childIds()
        var seen = Set<AgentId>()
        while !queue.isEmpty {
            let cid = queue.removeFirst()
            if !seen.insert(cid).inserted { continue }
            guard let child = agent(cid) else { continue }
            queue.append(contentsOf: child.childIds())
            guard child.handlerModule == "file_bridge.tools" else { continue }
            let r = child.metaValue(forKey: "root")?.asString ?? ""
            let candidateURL =
                r.hasPrefix("/") ? URL(fileURLWithPath: r) : workdir.appendingPathComponent(r)
            if Self.canonPath(candidateURL.path) == want { return cid }
        }
        return nil
    }

    /// An agent's dir RELATIVE to the loader's store root (`.fantastic`). The
    /// root loader itself → `""`; a child → `agents/<id>` (recursively).
    func storeReldir(storeRoot: URL, agentRoot: URL) -> String {
        // resolveSymlinks on BOTH so the prefix check holds across the macOS
        // `/var`→`/private/var` symlink (else this falls through to the absolute
        // path and records land doubly-nested under the store root).
        let s = Self.canonPath(storeRoot.path)
        let a = Self.canonPath(agentRoot.path)
        if a == s { return "" }
        if a.hasPrefix(s + "/") { return String(a.dropFirst(s.count + 1)) }
        return a
    }

    // ── loud persistence (audit 2026-09-28, d2) ────────────────────────
    // A refused/failed persist write is LOUD: stderr, and `persistence.error` on
    // the root reflect (which then reports no provider) until a write lands.

    /// The last persist failure, or nil while writes land.
    public var persistError: OrderedDictionary<String, JSON>? {
        persistErrorLock.lock()
        defer { persistErrorLock.unlock() }
        return _persistError
    }

    /// python `_persist_failed`.
    public func persistFailed(_ storeId: AgentId, _ path: String, _ why: JSON) {
        let msg: JSON
        if case .object(let o) = why, let e = o["error"] { msg = e } else { msg = why }
        let who = root?.id.value ?? ""
        RuntimeLog.warn(
            "  [\(who)] persist through \(pyRepr(.string(storeId.value))) failed: \(path): \(pyStr(msg))")
        persistErrorLock.lock()
        _persistError = [
            "store": .string(storeId.value), "path": .string(path), "error": msg,
        ]
        persistErrorLock.unlock()
    }

    /// python `_persist_landed`.
    public func persistLanded() {
        persistErrorLock.lock()
        _persistError = nil
        persistErrorLock.unlock()
    }

    /// python `_covers` — does a store's `allow` list expose `path` (a listed
    /// path, or under a listed directory)? The file_bridge's own rule, read off
    /// its record.
    static func allowCovers(_ allow: JSON?, _ path: String) -> Bool {
        for entry in allow?.asArray ?? [] {
            guard let s = entry.asString, !s.hasPrefix("/") else { continue }
            let e = normRelative(s)
            if e == "." || path == e || path.hasPrefix(e + "/") { return true }
        }
        return false
    }

    /// `posixpath.normpath(p or ".")` of a relative path.
    private static func normRelative(_ p: String) -> String {
        var parts: [Substring] = []
        for seg in p.split(separator: "/", omittingEmptySubsequences: true) {
            if seg == "." { continue }
            if seg == "..", let last = parts.last, last != ".." {
                parts.removeLast()
                continue
            }
            parts.append(seg)
        }
        return parts.isEmpty ? "." : parts.joined(separator: "/")
    }

    /// python `_check_store_allow` — at boot: a discovered store whose `allow`
    /// does not expose the records (`agent.yaml` and the children dir) would
    /// refuse every write; say so now.
    public func checkStoreAllow() {
        guard let storeId = findStore() else { return }
        let allow = agent(storeId)?.metaValue(forKey: "allow")
        let missing = [RECORD_FILE, childrenDir].filter { !Self.allowCovers(allow, $0) }
        if missing.isEmpty { return }
        persistFailed(
            storeId, missing.joined(separator: ", "),
            .string(
                "the store's `allow` does not expose the records "
                    + "(add them, or `.`): allow=\(pyRepr(allow ?? .null))"))
    }

    /// A `secrets.token_hex(4)`-shaped tmp-file suffix: 4 random bytes as 8 lowercase
    /// hex digits (`agent.yaml.tmp.361406e7`, python's own spelling).
    private static func randomHex8() -> String {
        (0..<4).map { _ -> String in
            let h = String(UInt8.random(in: 0...255), radix: 16)
            return h.count < 2 ? "0" + h : h
        }.joined()
    }

    /// Read a store-relative path's text content through the provider (`nil` on
    /// any error / missing file).
    /// A store I/O send made with OPERATOR authority — the substrate persisting to
    /// its OWN discovered store is bookkeeping (like create_agent's child boot),
    /// not an agent reaching out, so it never runs against a grant.
    private func storeSend(_ storeId: AgentId, _ payload: JSON) async -> JSON {
        await KernelTaskLocals.$currentSender.withValue(nil) {
            await send([Item(storeId, payload)])[0].result
        }
    }

    /// ONE STORE IO, THROUGH THE BATCH (C47).
    ///
    /// Every persistence byte — the record merge read, the record write, the
    /// readme probe and seed — crosses this function, and it is a `send` of ONE
    /// BINARY ITEM under operator authority. So each is a ledger execution with
    /// a call row, a `ps` row, a settle row and a timeout, exactly like python's
    /// `kernel_state`, which does all of this with `kernel.send([...])`.
    /// `sendWithBinary` minted none of it, so the substrate's own writes were
    /// the one class of work the message log could not see.
    ///
    /// Operator authority: the substrate reading and writing its OWN store is
    /// persistence bookkeeping, not an agent reaching out — it never runs
    /// against a grant.
    ///
    /// `body` is the request's chunk (the payload's `bytes`, a `write_stream`);
    /// `nil` asks for the reply's chunk raw (a `read_stream`). The chunk comes
    /// back when the reply holds it at `bytes` — python reads `reply["bytes"]`.
    private func storeSendBinary(_ storeId: AgentId, _ header: JSON, _ body: Data?) async
        -> (JSON, Data?)
    {
        await KernelTaskLocals.$currentSender.withValue(nil) {
            let item = Item(
                storeId, header, binary: body.map { Blob("bytes", $0) }, rawReply: body == nil)
            let out = await send([item])[0]
            return (out.result, bodyAt(out.body, "bytes"))
        }
    }

    /// The WHOLE existing record, paged to EOF — python `_persist_via_store`'s
    /// merge read: `read_stream {path, offset}` until `eof` (or no int
    /// `next_offset`). One page caps at the store's chunk (64 KiB by default),
    /// so a larger record failed to parse and silently lost its unknown keys on
    /// the next write. A page that is not a chunk — a refusal, absent or not —
    /// empties the read: the merge starts from `{}`, as python's does.
    private func readRecordViaStore(_ storeId: AgentId, _ path: String) async -> Data {
        var buf = Data()
        var offset: Int64 = 0
        while true {
            let (got, body) = await storeSendBinary(
                storeId,
                .object([
                    "type": .string("read_stream"), "path": .string(path),
                    "offset": .integer(offset),
                ]), nil)
            guard case .object(let o) = got, let chunk = body else { return Data() }
            buf.append(chunk)
            // python `got.get("eof", True) or not isinstance(next_offset, int)`.
            if pyTruthy(o["eof"] ?? .bool(true)) { break }
            guard case .integer(let n)? = o["next_offset"] else { break }
            offset = n
        }
        return buf
    }

    /// Is a store path present? python `_seed_doc_via_store`'s probe: ONE
    /// `read_stream {path}` whose answer carries a chunk.
    private func presentViaStore(_ storeId: AgentId, _ path: String) async -> Bool {
        let (got, body) = await storeSendBinary(
            storeId, .object(["type": .string("read_stream"), "path": .string(path)]), nil)
        if case .object = got, body != nil { return true }
        return false
    }

    /// Persist an agent's record onto its per-agent `agent.yaml` THROUGH the
    /// discovered provider's `write` (merge-not-overwrite: read existing, overlay
    /// the kernel-managed keys, write back — sidecar fields survive). No-op in
    /// InMemory / ephemeral / no-provider-wired (RAM). If the provider is sealed
    /// it refuses and the write doesn't land — NO fallback.
    ///
    /// ONLY A WRITE THAT LANDED SEEDS THE SIDECARS (python `_persist_via_store`
    /// returns before `_seed_readme_via_store` on a failed write or rename):
    /// the bundle's `readme.md` and its `docs/` tree, copy-if-missing.
    public func persistRecord(_ agent: Agent) async {
        if storage.isInMemory || agent.ephemeral { return }
        guard let storeId = findStore(), let rootAgent = root else { return }
        let reldir = storeReldir(storeRoot: rootAgent.rootPath, agentRoot: agent.rootPath)
        let af = reldir.isEmpty ? RECORD_FILE : "\(reldir)/\(RECORD_FILE)"
        // Merge: read existing through the provider, overlay kernel-managed keys.
        var existing: OrderedDictionary<String, JSON> = [:]
        let buf = await readRecordViaStore(storeId, af)
        if !buf.isEmpty, let content = String(data: buf, encoding: .utf8),
            case .object(let d)? = try? YamlIO.load(content)
        {
            existing = d
        }
        // `existing.update(record)` — python `dict.update`: the record's keys in
        // RECORD order (`id, handler_module?, <meta>, parent_id`), a key already
        // on disk keeping the position it has, and a key the record does not
        // carry (no handler_module, the root's absent parent_id) left alone.
        // Assigning `nil` here DELETED such a key instead.
        for (k, v) in agent.record().ordered { existing[k] = v }
        let serialized = YamlIO.dump(.object(existing))
        // ATOMIC COMMIT (C43): stream into a tmp beside the target, then
        // RENAME — never `write_stream` the live path directly. A
        // `write_stream` that dies after truncating `af` itself would leave a
        // half-written or empty `agent.yaml`: the record gone, the next boot
        // hydrating a tree with a hole in it. `rename` is the leg's own
        // single atomic op (and creates parents), so the only two outcomes
        // are "the old record survives" or "the new one is complete" — never
        // a torn file.
        let tmp = "\(af).tmp.\(Self.randomHex8())"
        let written = await storeSendBinary(
            storeId,
            .object([
                "type": .string("write_stream"), "path": .string(tmp),
                "truncate": .bool(true),
            ]),
            Data(serialized.utf8))
        if case .object = written.0, !pyTruthy(written.0["error"]) {
        } else {
            // Leave the previous record untouched; a debris tmp is visible in `list`.
            persistFailed(storeId, af, written.0)
            return
        }
        let moved = await storeSend(
            storeId,
            .object([
                "type": .string("rename"), "old_path": .string(tmp), "new_path": .string(af),
            ]))
        guard case .object = moved, !pyTruthy(moved["error"]) else {
            persistFailed(storeId, af, moved)
            return
        }
        persistLanded()
        await seedSidecarsViaStore(storeId, reldir: reldir, agent.handlerModule)
    }

    /// Copy-if-missing the bundle's shipped `readme.md` AND its `docs/` topic
    /// tree into the agent's dir THROUGH the provider — never clobber operator
    /// edits. Python `_seed_readme_via_store`: the source is the installed
    /// bundle, the sink the stream provider.
    private func seedSidecarsViaStore(_ storeId: AgentId, reldir: String, _ hm: String?) async {
        guard let hm, let bundle = bundles.get(hm) else { return }
        let prefix = reldir.isEmpty ? "" : "\(reldir)/"
        if let readme = bundle.readme {
            await seedDocViaStore(storeId, "\(prefix)readme.md", readme)
        }
        for (key, body) in bundle.docs {
            await seedDocViaStore(storeId, "\(prefix)docs/\(key).md", body)
        }
    }

    /// One `.md` file through the provider, copy-if-missing. A refused write is
    /// LOUD (`persistFailed`), as a record write is — python
    /// `_seed_doc_via_store`: its reply used to be dropped, so a doc that never
    /// landed was invisible (audit 2026-10-03, ARCH2-9).
    private func seedDocViaStore(_ storeId: AgentId, _ path: String, _ content: String) async {
        if await presentViaStore(storeId, path) { return }
        let written = await storeSendBinary(
            storeId,
            .object([
                "type": .string("write_stream"), "path": .string(path),
                "truncate": .bool(true),
            ]),
            Data(content.utf8))
        guard case .object = written.0, !pyTruthy(written.0["error"]) else {
            persistFailed(storeId, path, written.0)
            return
        }
    }

    /// Remove an agent's dir THROUGH the provider (the `delete` verb). Never the
    /// root. No-op without a wired provider. Mirrors py `_forget_via_store`.
    public func forgetRecord(_ agent: Agent) async {
        if agent.ephemeral { return }
        await forgetPath(agent.rootPath)
    }

    /// Remove a DIR the writer remembered for an agent that is already gone —
    /// a removed agent is no longer in the tree to resolve a path from, so the
    /// state-stream subscriber caches it at `added`/`updated` time.
    public func forgetPath(_ dir: URL) async {
        if storage.isInMemory { return }
        guard let storeId = findStore(), let rootAgent = root else { return }
        let reldir = storeReldir(storeRoot: rootAgent.rootPath, agentRoot: dir)
        if reldir.isEmpty { return }  // never remove the root
        // `recursive` is REQUIRED: an agent's dir holds `agent.yaml` (and any
        // sidecars), and a bare `delete` on a populated dir refuses — py parity
        // (`kernel_state._forget_via_store` has always passed it).
        let out = await storeSend(
            storeId,
            .object([
                "type": .string("delete"), "path": .string(reldir), "recursive": .bool(true),
            ]))
        if pyTruthy(out["error"]) { persistFailed(storeId, reldir, out) }
    }
}
