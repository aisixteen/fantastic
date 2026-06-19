// Persistence inversion — records persist THROUGH a discovered file_bridge.
//
// Mirrors py `kernel_state` + rust `persistence.rs`: the substrate no longer
// owns an `fs` surface for records. It persists THROUGH a DISCOVERED
// `file_bridge` provider's `read`/`write` verbs (records are JSON text, so the
// text verbs suffice — no stream codec needed). NO FALLBACKS: no provider ⇒
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
        guard let rootAgent = root else { return nil }
        let want = Self.canonPath(rootAgent.rootPath.path)
        // The kernel's running dir = the dir holding `.fantastic` (= cwd in
        // production). Resolve a relative provider root against it.
        let workdir = rootAgent.rootPath.deletingLastPathComponent()
        for cid in rootAgent.childIds() {
            guard let child = agent(cid),
                child.handlerModule == "file_bridge.tools"
            else { continue }
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

    /// Read a store-relative path's text content through the provider (`nil` on
    /// any error / missing file).
    private func readViaStore(_ storeId: AgentId, _ path: String) async -> String? {
        let got = await send(storeId, .object(["type": .string("read"), "path": .string(path)]))
        if got["error"].asString != nil { return nil }
        return got["content"].asString
    }

    /// Persist an agent's record onto its per-agent `agent.json` THROUGH the
    /// discovered provider's `write` (merge-not-overwrite: read existing, overlay
    /// the kernel-managed keys, write back — sidecar fields survive). No-op in
    /// InMemory / ephemeral / no-provider-wired (RAM). If the provider is sealed
    /// it refuses and the write doesn't land — NO fallback.
    public func persistRecord(_ agent: Agent) async {
        if storage.isInMemory || agent.ephemeral { return }
        guard let storeId = findStore(), let rootAgent = root else { return }
        let reldir = storeReldir(storeRoot: rootAgent.rootPath, agentRoot: agent.rootPath)
        let af = reldir.isEmpty ? "agent.json" : "\(reldir)/agent.json"
        // Merge: read existing through the provider, overlay kernel-managed keys.
        var existing: OrderedDictionary<String, JSON> = [:]
        if let content = await readViaStore(storeId, af),
            let data = content.data(using: .utf8),
            let parsed = try? JSON.parse(data),
            case .object(let d) = parsed
        {
            existing = d
        }
        existing["id"] = .string(agent.id.value)
        existing["handler_module"] = agent.handlerModule.map { .string($0) }
        existing["parent_id"] = agent.parentId.map { .string($0.value) }
        for (k, v) in agent.meta { existing[k] = v }
        let serialized = JSON.object(existing).serializePretty(indent: 2)
        _ = await send(
            storeId,
            .object([
                "type": .string("write"), "path": .string(af), "content": .string(serialized),
            ]))
    }

    /// Seed a `readme.md` (copy-if-missing) into the agent's dir THROUGH the
    /// provider — never clobber operator edits. No-op without a wired provider.
    public func seedReadmeViaStore(_ agent: Agent, content: String) async {
        if storage.isInMemory || agent.ephemeral { return }
        guard let storeId = findStore(), let rootAgent = root else { return }
        let reldir = storeReldir(storeRoot: rootAgent.rootPath, agentRoot: agent.rootPath)
        let path = reldir.isEmpty ? "readme.md" : "\(reldir)/readme.md"
        if await readViaStore(storeId, path) != nil { return }  // already present
        _ = await send(
            storeId,
            .object([
                "type": .string("write"), "path": .string(path), "content": .string(content),
            ]))
    }

    /// Remove an agent's dir THROUGH the provider (the `delete` verb). Never the
    /// root. No-op without a wired provider. Mirrors py `_forget_via_store`.
    public func forgetRecord(_ agent: Agent) async {
        if storage.isInMemory || agent.ephemeral { return }
        guard let storeId = findStore(), let rootAgent = root else { return }
        let reldir = storeReldir(storeRoot: rootAgent.rootPath, agentRoot: agent.rootPath)
        if reldir.isEmpty { return }  // never remove the root
        _ = await send(storeId, .object(["type": .string("delete"), "path": .string(reldir)]))
    }
}
