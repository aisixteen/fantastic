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

/// python's `MANIFEST_FIELD`.
let MANIFEST_FIELD = "manifest"

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
        // `shutdown` is NOT a verb here: python's VERBS holds reflect + boot, so
        // anything else — `shutdown` included — is the coded unhandled refusal.
        default:
            return Refusal.make(
                Refusal.unhandled,
                "static_loader: unknown type \(pyRepr(payload["type"]))",
                extra: [("verb", payload["type"])])
        }
    }

    /// Read the manifest THROUGH the running-dir clamp — python
    /// `fs.read_text(Path.cwd(), path)`.
    ///
    /// A cold bring-up read (before any `file_bridge` agent can exist) but NOT
    /// exempt from the running-dir law: `fs` is the one disk surface. The clamp
    /// is `fs.resolve(cwd, path)`, not the lexical root clamp: a leading `/`
    /// escapes, symlinks are followed, and the error tail is python's own
    /// exception text (`fs: path '/etc/hosts' escapes root`, `[Errno 2] No such
    /// file or directory: '<abs>'`).
    static func readManifest(_ path: String) -> (text: String?, error: String?) {
        let cwd = FileManager.default.currentDirectoryPath
        do {
            return (try fsReadText(cwd, path, base: cwd), nil)
        } catch {
            return (nil, error.message)
        }
    }

    /// Refuse a manifest that is absent or unreadable AT WRITE TIME, so a broken
    /// roster never becomes a booting record.
    ///
    /// Merged view (record + patch), mirroring python's `validate_meta` and
    /// rust's. A record with no manifest yet is allowed — `boot` reports that.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        guard let raw = meta[MANIFEST_FIELD] ?? record[MANIFEST_FIELD] else { return nil }
        guard let path = raw.asString, !path.isEmpty else {
            return "static_loader: \(MANIFEST_FIELD) must be a non-empty path string"
        }
        if let e = Self.readManifest(path).error {
            return "static_loader: manifest \(pyRepr(path)) is not readable: \(e)"
        }
        return nil
    }

    // MARK: - the manifest

    /// python `_load_manifest` — `(agents, error)`.
    private func loadManifest(_ agent: Agent) -> (agents: [JSON], error: String?) {
        guard let path = agent.metaValue(forKey: MANIFEST_FIELD)?.asString, !path.isEmpty else {
            return ([], "static_loader: no \(pyRepr(MANIFEST_FIELD)) path on the record")
        }
        let read = Self.readManifest(path)
        guard let text = read.text else {
            return ([], "static_loader: cannot read manifest \(pyRepr(path)): \(read.error ?? "")")
        }
        let document: JSON
        do {
            // COMPOSE, not load: a mapping keeps its DOCUMENT ORDER, which every
            // `{spec!r}` text and every persisted record then carries (python's
            // dicts are ordered; a Swift Dictionary is not).
            document = try Yams.compose(yaml: text).map(nodeToJSON) ?? .null
        } catch {
            // D4 residual: the tail is the YAML library's own message; PyYAML's
            // `MarkedYAMLError` wording is not reproducible from libyaml.
            return ([], "static_loader: manifest \(pyRepr(path)) is not valid YAML: \(error)")
        }
        // `yaml.safe_load(text) or {}` — a python-FALSY document (empty, `~`,
        // `0`, `false`, `[]`, `{}`) is an empty mapping, and boots empty.
        let data: JSON = pyTruthy(document) ? document : .object([:])
        guard case .object(let root) = data else {
            return ([], "static_loader: manifest \(pyRepr(path)) must be a mapping with `agents:`")
        }
        guard let agentsValue = root["agents"] else { return ([], nil) }
        guard case .array(let agents) = agentsValue else {
            return ([], "static_loader: `agents:` in \(pyRepr(path)) must be a list")
        }
        return (agents, nil)
    }

    // MARK: - boot: hydrate the declared roster as operator

    private func boot(agentId: AgentId, agent: Agent, kernel: Kernel) async -> JSON {
        let (agents, err) = loadManifest(agent)
        if let err {
            return .object(["error": .string(err), "booted": .bool(false)])
        }
        var created: [JSON] = []
        var reapplied: [JSON] = []
        for spec in agents {
            guard case .object(let obj) = spec, pyTruthy(obj["handler_module"] ?? .null) else {
                return .object([
                    "error": .string(
                        "static_loader: each roster entry needs a handler_module: \(pyRepr(spec))"),
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
                        "static_loader: roster entry needs an explicit id: \(pyRepr(spec))"),
                    "booted": .bool(false),
                ])
            }
            if let live = kernel.agent(AgentId(id)) {
                // THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM,
                // cloud, an API). So an id that already exists is RE-APPLIED
                // from the manifest, never skipped: editing a grant in the yaml
                // used to change nothing at all, because the hydrated record
                // silently won and the operator got no word about it.
                var declared = obj
                declared["id"] = nil
                declared["handler_module"] = nil
                declared["parent_id"] = nil
                // `update_agent` REPLACES a named field, so a bare re-apply of
                // `can_call` would delete the structural baseline the kernel
                // stamps at create and sever the agent from its own parent and
                // children. Merge over the baseline, as `create_agent` does.
                for (field, baseline) in [
                    ("can_call", defaultCanCall()), ("callable_by", defaultCallableBy()),
                ] {
                    if case .object(let declaredField)? = declared[field],
                        case .object(var merged) = baseline
                    {
                        for (k, v) in declaredField { merged[k] = v }
                        declared[field] = .object(merged)
                    }
                }
                let declaredHM = obj["handler_module"] ?? .null
                if live.handlerModule != declaredHM.asString {
                    // A bundle cannot be swapped under a live agent (its
                    // in-process state belongs to the old one); the manifest is
                    // asking for a different agent under a used name.
                    return .object([
                        "error": .string(
                            "static_loader: \(pyRepr(id)) exists as "
                                + "\(pyRepr(live.handlerModule.map(JSON.string) ?? .null)) but the manifest declares "
                                + "\(pyRepr(declaredHM)) — a handler_module is fixed at "
                                + "create; delete the agent to re-declare it"),
                        "booted": .bool(false),
                    ])
                }
                if !declared.isEmpty {
                    // `**declared` LAST: a spec that names `type` overrides the
                    // verb, python's spread order.
                    var patch: OrderedDictionary<String, JSON> = [
                        "type": .string("update_agent"), "id": .string(id),
                    ]
                    for (k, v) in declared { patch[k] = v }
                    let r = await KernelTaskLocals.$currentSender.withValue(nil) {
                        await kernel.send([Item(AgentId("kernel"), .object(patch))])[0].result
                    }
                    // Only a TRUTHY `error` is a failure (python `r.get("error")`).
                    if pyTruthy(r["error"]) {
                        return .object([
                            "error": .string(
                                "static_loader: re-applying \(pyRepr(id)): \(pyStr(r["error"]))"),
                            "booted": .bool(false),
                        ])
                    }
                }
                reapplied.append(.string(id))
                continue
            }
            // The declared agent becomes THIS loader's child ($parent = the
            // loader), but placement is stated by `parent_id`, not by who is
            // addressed: `create_agent` is kernel-owned and answered by the ROOT
            // alone. Baseline grant is stamped + the manifest's explicit
            // `can_call` merges on top; the child boots inside create_agent.
            // Operator authority: the loader hydrating the roster it was told to
            // build. `**spec` sits BETWEEN `type` and `parent_id`, python's
            // spread order: a spec `type` overrides the verb, a spec `parent_id`
            // never overrides the placement.
            var payload: OrderedDictionary<String, JSON> = ["type": .string("create_agent")]
            for (k, v) in obj { payload[k] = v }
            payload["parent_id"] = .string(agentId.value)
            let rec = await KernelTaskLocals.$currentSender.withValue(nil) {
                await kernel.send([Item(AgentId("kernel"), .object(payload))])[0].result
            }
            if case .object = rec, let newId = rec["id"].asString, !newId.isEmpty {
                created.append(.string(newId))
            } else {
                return .object([
                    "error": .string(
                        "static_loader: create failed for \(pyRepr(spec)): \(pyStr(rec))"),
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
        let name = agent.displayName ?? ""
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string("declarative YAML roster loader"),
            // python `agent.display_name or id` — an EMPTY name is falsy.
            "display_name": .string(name.isEmpty ? agent.id.value : name),
            "loader": .bool(true),
            "manifest": agent.metaValue(forKey: MANIFEST_FIELD) ?? .null,
            "verbs": .object([
                "reflect": .string(
                    "The loader's posture: what manifest it hydrates, and that it IS a loader"),
                "boot": .string("Hydrate the declared roster as OPERATOR."),
            ]),
        ])
    }

    // MARK: - Yams Node → JSON (document order preserved)

    /// One composed YAML node as JSON, resolving scalars by their RESOLVED TAG
    /// (the YAML schema's own typing) and keeping every mapping in document
    /// order.
    private func nodeToJSON(_ node: Node) -> JSON {
        switch node {
        case .scalar(let scalar):
            // `Tag.name` is internal to Yams; its `rawValue` is the same string.
            switch node.tag.rawValue {
            case Tag.Name.null.rawValue: return .null
            case Tag.Name.bool.rawValue: return node.bool.map(JSON.bool) ?? .string(scalar.string)
            case Tag.Name.int.rawValue:
                return node.int.map { .integer(Int64($0)) } ?? .string(scalar.string)
            case Tag.Name.float.rawValue:
                return node.float.map(JSON.double) ?? .string(scalar.string)
            default: return .string(scalar.string)
            }
        case .sequence(let sequence):
            return .array(sequence.map(nodeToJSON))
        case .mapping(let mapping):
            var out: OrderedDictionary<String, JSON> = [:]
            for pair in mapping {
                out[pair.key.string ?? pyStr(nodeToJSON(pair.key))] = nodeToJSON(pair.value)
            }
            return .object(out)
        case .alias:
            return .null
        }
    }

    /// python's `readme.md`, verbatim.
    static let readmeText = """
        # static_loader — declarative YAML roster

        A loader agent that hydrates a **fixed roster** from a `fantastic.yaml` manifest.
        Use it when the set of agents should be declared, not composed live.

        Init a workspace with it:

        ```
        fantastic init static_loader ./fantastic.yaml
        ```

        That seeds a `static_loader` agent (record field `manifest: ./fantastic.yaml`)
        under the root and boots it. On `boot` the loader reads the manifest and creates
        each declared agent as a child of itself — under `operator_context()`, so the
        hydration is the operator's authority at boot, not standing power the loader
        holds.

        Manifest:

        ```yaml
        agents:
          - id: memory
            handler_module: text_file.tools
          - id: worker
            handler_module: exec_bridge.tools
            allowed: ['python3 .*']   # the command(s) this leg may spawn
            can_call:            # deny-all: name the lateral edges this agent may use
              memory: [set, read]
        ```

        Every declared agent gets the Unix baseline grant
        (`$self`/`$parent`/`$children`/`$loader`) plus whatever `can_call` the manifest
        names. After boot the roster is **closed**: an agent is deny-all and cannot
        `create_agent`, so the manifest is the whole roster. Every entry MUST carry an
        explicit `id` — boot is idempotent by id (a reboot re-reads the manifest and
        RE-APPLIES the declared fields over an existing agent), so the manifest and the
        persisted tree never duplicate. An entry with no `id` is refused loud
        (`booted: false`): nothing could match it, so each reboot would mint another
        agent beside the last one's and persist it.

        The kernel knows nothing about YAML; this bundle owns the format — the same way
        `kernel_state` owns the on-disk `.fantastic` layout.

        """
}
