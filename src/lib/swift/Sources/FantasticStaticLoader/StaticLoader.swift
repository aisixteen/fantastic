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
// re-reads + skips agents that already exist). Declare explicit ids.

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

    // MARK: - boot: hydrate the declared roster as operator

    private func boot(agentId: AgentId, agent: Agent, kernel: Kernel) async -> JSON {
        guard let path = agent.metaValue(forKey: "manifest")?.asString, !path.isEmpty else {
            return .object([
                "error": .string("static_loader: no `manifest` path on the record"),
                "booted": .bool(false),
            ])
        }
        guard let text = try? String(contentsOfFile: path, encoding: .utf8) else {
            return .object([
                "error": .string("static_loader: cannot read manifest \(path)"),
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
        var skipped: [JSON] = []
        for spec in agents {
            guard case .object(let obj) = spec, obj["handler_module"]?.asString != nil else {
                return .object([
                    "error": .string(
                        "static_loader: each roster entry needs a handler_module: \(spec.serialize())"
                    ),
                    "booted": .bool(false),
                ])
            }
            if let id = obj["id"]?.asString, kernel.agent(AgentId(id)) != nil {
                skipped.append(.string(id))
                continue
            }
            // create_agent to SELF → the declared agent becomes THIS loader's
            // child. Baseline grant is stamped + the manifest's explicit `can_call`
            // merges on top; the child boots inside create_agent. Operator
            // authority: the loader hydrating the roster it was told to build.
            var payload = obj
            payload["type"] = .string("create_agent")
            let rec = await KernelTaskLocals.$currentSender.withValue(nil) {
                await kernel.send(agentId, .object(payload))
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
            "skipped": .array(skipped),
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

    // MARK: - Yams Any → JSON (mirror of yaml_state's converter)

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
        so the manifest is the whole roster. Idempotent by id.
        """
}
