// static_loader — a DECLARATIVE YAML roster loader.
//
// Mirror of the python canonical `static_loader.tools` (and rust
// `fantastic-static-loader`). A `fantastic.yaml` manifest declares the agents;
// on `boot` this loader reads it and hydrates them AS OPERATOR
// (`$currentSender.withValue(nil)` + the normal `create_agent` path), as its own
// children — each carrying the Unix baseline grant plus any explicit `can_call`
// the manifest names. The kernel knows nothing about YAML; this bundle owns the
// format.
//
// After boot the roster is CLOSED: deny-all means a roster agent cannot
// `create_agent`, so the manifest IS the roster. Idempotent by id (a reboot
// re-reads the manifest and RE-APPLIES the declared fields over an existing
// agent). Every entry MUST name an explicit `id` — an id-less one is refused
// loud, because nothing could match it and each reboot would mint another agent
// beside the last.

import FantasticFile
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Yams

public struct StaticLoaderBundle: AgentBundle {
    /// The `handler_module` value that routes to this bundle.
    public static let handlerModule = "static_loader.tools"

    public let name = "static_loader"

    public init() {}

    public var readme: String? { Self.readmeText }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        switch payload["type"].asString ?? "" {
        case "reflect": return reflectReply(agent)
        case "boot": return await boot(agentId: agentId, agent: agent, kernel: kernel)
        case "shutdown": return .null
        case let other:
            return .object(["error": .string("static_loader: unknown type \(other)")])
        }
    }

    /// Read the manifest THROUGH the running-dir clamp.
    ///
    /// A cold bring-up read — before any `file_bridge` agent can exist — but NOT
    /// exempt from the running-dir law: `fs` is the one disk surface. This read
    /// went straight to `String(contentsOfFile:)` until 2026-08-28, so a manifest
    /// anywhere on disk was readable here while python
    /// (`fs.read_text(Path.cwd(), path)`) and rust (`clamp_root`) both refused a
    /// path outside the running dir.
    static func readManifest(_ path: String) -> (text: String?, error: String?) {
        let cwd = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        let clamped: URL
        do { clamped = try clampRoot(path, base: cwd) } catch {
            return (nil, "static_loader: cannot read manifest \(path): \(error)")
        }
        guard let text = try? String(contentsOf: clamped, encoding: .utf8) else {
            return (nil, "static_loader: cannot read manifest \(path)")
        }
        return (text, nil)
    }

    /// Refuse a manifest that is absent or unreadable AT WRITE TIME, so a broken
    /// roster never becomes a booting record.
    ///
    /// Merged view (record + patch), mirroring python's `validate_meta` and
    /// rust's. A record with no manifest yet is allowed — `boot` reports that.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        guard let raw = meta["manifest"] ?? record["manifest"] else { return nil }
        guard let path = raw.asString, !path.isEmpty else {
            return "static_loader: manifest must be a non-empty path string"
        }
        if Self.readManifest(path).error != nil {
            return "static_loader: manifest \(path) is not readable"
        }
        return nil
    }

    // MARK: - boot: hydrate the declared roster as operator

    private func boot(agentId: AgentId, agent: Agent, kernel: Kernel) async -> JSON {
        guard let path = agent.metaValue(forKey: "manifest")?.asString, !path.isEmpty else {
            return .object([
                "error": .string("static_loader: no `manifest` path on the record"),
                "booted": .bool(false),
            ])
        }
        let read = Self.readManifest(path)
        guard let text = read.text else {
            return .object([
                "error": .string(read.error ?? "static_loader: cannot read manifest"),
                "booted": .bool(false),
            ])
        }
        let parsed: Any?
        do { parsed = try Yams.load(yaml: text) } catch {
            return .object([
                "error": .string("static_loader: manifest \(path) is not valid YAML: \(error)"),
                "booted": .bool(false),
            ])
        }
        guard case .object(let root) = anyToJSON(parsed as Any) else {
            return .object([
                "error": .string(
                    "static_loader: manifest \(path) must be a mapping with `agents:`"),
                "booted": .bool(false),
            ])
        }
        let agents = root["agents"]?.asArray ?? []
        var created: [JSON] = []
        var reapplied: [JSON] = []
        for spec in agents {
            guard case .object(let obj) = spec, obj["handler_module"]?.asString != nil else {
                return .object([
                    "error": .string(
                        "static_loader: each roster entry needs a handler_module: \(spec.serialize())"
                    ),
                    "booted": .bool(false),
                ])
            }
            // An id-less entry cannot be idempotent: the roster is keyed by id,
            // so nothing matches the agent the LAST boot minted and this boot
            // mints a SECOND one, which the flush persists — the roster grows by
            // one agent per restart and the operator is never told. The manifest
            // is truth, so it must name its agents.
            guard let id = obj["id"]?.asString, !id.isEmpty else {
                return .object([
                    "error": .string(
                        "static_loader: roster entry needs an explicit id: \(spec.serialize())"),
                    "booted": .bool(false),
                ])
            }
            if let live = kernel.agent(AgentId(id)) {
                // THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM,
                // cloud, an API). So an id that already exists is RE-APPLIED
                // from the manifest, never skipped: editing a grant in the yaml
                // used to change nothing at all, because the hydrated record
                // silently won and the operator got no word about it.
                let declaredHM = obj["handler_module"]?.asString
                if live.handlerModule != declaredHM {
                    // A bundle cannot be swapped under a live agent (its
                    // in-process state belongs to the old one).
                    return .object([
                        "error": .string(
                            "static_loader: \"\(id)\" exists as "
                                + "\"\(live.handlerModule ?? "")\" but the manifest declares "
                                + "\"\(declaredHM ?? "")\" — a handler_module is fixed at "
                                + "create; delete the agent to re-declare it"),
                        "booted": .bool(false),
                    ])
                }
                var patch = obj
                patch["id"] = nil
                patch["handler_module"] = nil
                patch["parent_id"] = nil
                // `update_agent` REPLACES a named field, so a bare re-apply of
                // `can_call` would delete the structural baseline the kernel
                // stamps at create and sever the agent from its own parent and
                // children. Merge over the baseline, as `create_agent` does.
                for field in ["can_call", "callable_by"] {
                    if case .object(let declared)? = patch[field],
                        case .object(var merged) = defaultCanCall()
                    {
                        for (k, v) in declared { merged[k] = v }
                        patch[field] = .object(merged)
                    }
                }
                if !patch.isEmpty {
                    patch["type"] = .string("update_agent")
                    patch["id"] = .string(id)
                    let r = await KernelTaskLocals.$currentSender.withValue(nil) {
                        await kernel.send([Item(AgentId("kernel"), .object(patch))])[0].result
                    }
                    if r["error"] != .null {
                        return .object([
                            "error": .string(
                                "static_loader: re-applying \"\(id)\": \(r["error"].serialize())"),
                            "booted": .bool(false),
                        ])
                    }
                }
                reapplied.append(.string(id))
                continue
            }
            // create_agent to SELF → the declared agent becomes THIS loader's
            // child. Baseline grant is stamped + the manifest's explicit `can_call`
            // merges on top; the child boots inside create_agent. Operator
            // authority: the loader hydrating the roster it was told to build.
            var payload = obj
            // The declared agent becomes THIS loader's child ($parent = the
            // loader), but placement is stated by `parent_id`, not by who is
            // addressed: `create_agent` is kernel-owned and answered by the ROOT
            // alone. Baseline grant is stamped + the manifest's explicit
            // `can_call` merges on top; the child boots inside create_agent.
            payload["type"] = .string("create_agent")
            payload["parent_id"] = .string(agentId.value)
            let rec = await KernelTaskLocals.$currentSender.withValue(nil) {
                await kernel.send([Item(AgentId("kernel"), .object(payload))])[0].result
            }
            if let id = rec["id"].asString {
                created.append(.string(id))
            } else {
                return .object([
                    "error": .string("static_loader: create failed: \(rec.serialize())"),
                    "booted": .bool(false),
                ])
            }
        }
        return .object([
            "booted": .bool(true),
            "created": .array(created),
            "reapplied": .array(reapplied),
        ])
    }

    private func reflectReply(_ agent: Agent) -> JSON {
        .object([
            "id": .string(agent.id.value),
            "sentence": .string("declarative YAML roster loader"),
            "display_name": .string(agent.displayName ?? agent.id.value),
            "loader": .bool(true),
            "manifest": agent.metaValue(forKey: "manifest") ?? .null,
            "verbs": .object([
                "reflect": .string("No args. The loader's posture: manifest + roster-hydration."),
                "boot": .string(
                    "No args. Read the manifest + hydrate the declared roster (operator)."),
            ]),
        ])
    }

    // MARK: - Yams Any → JSON

    private func anyToJSON(_ v: Any) -> JSON {
        if v is NSNull { return .null }
        if let b = v as? Bool { return .bool(b) }
        if let i = v as? Int { return .integer(Int64(i)) }
        if let i = v as? Int64 { return .integer(i) }
        if let d = v as? Double { return .double(d) }
        if let s = v as? String { return .string(s) }
        if let a = v as? [Any] { return .array(a.map { anyToJSON($0) }) }
        if let o = v as? [String: Any] {
            var dict: OrderedDictionary<String, JSON> = [:]
            for (k, val) in o { dict[k] = anyToJSON(val) }
            return .object(dict)
        }
        return .null
    }

    static let readmeText = """
        # static_loader — declarative YAML roster

        A loader agent that hydrates a fixed roster from a `fantastic.yaml`
        manifest. `fantastic init static_loader ./fantastic.yaml` seeds it under
        the root and boots it; on boot it reads the manifest and creates each
        declared agent as a child of itself (as operator). Every declared agent
        gets the Unix baseline grant plus whatever `can_call` the manifest names.
        After boot the roster is CLOSED (deny-all: an agent cannot create_agent),
        so the manifest is the whole roster. Idempotent by id — every entry MUST
        carry an explicit `id`; an id-less one is refused loud (`booted: false`),
        because nothing could match it and each reboot would mint another agent
        beside the last one's.
        """
}
