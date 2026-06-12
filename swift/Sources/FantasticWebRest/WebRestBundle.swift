// web_rest — REST verb-invocation surface as a child of `web`.
//
// Mirrors Python's `web_rest`: a route-contributor child that runs no
// server. The parent `web` pulls `get_routes` at boot and mounts the
// returned HTTP descriptors; when one matches, the host calls this
// bundle's `handle_route` verb. Canonical wire shape (Python parity):
//
//   POST /<self_id>/<target>      body={type:verb,...} → kernel.send(target, body)
//   GET  /<self_id>/_reflect              → kernel.send("kernel", {reflect})
//   GET  /<self_id>/_reflect/<target>     → kernel.send(target, {reflect})
//
// The verb travels in the request BODY's `type` (not the URL). Routes
// are namespaced by the agent's own id, so multiple web_rest instances
// coexist. Opt-in: create with
//   <web_id> create_agent handler_module=web_rest.tools

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "web_rest.tools"

public final class WebRestBundle: AgentBundle, @unchecked Sendable {
    public let name = "web_rest"
    public init() {}

    public var readme: String? {
        """
        web_rest — HTTP verb channel (diagnostic). Child of a web agent.
        POST /<self_id>/<target_id> body=payload → kernel.send → JSON. \
        Address-bar-friendly GET shortcuts: GET /<self_id>/_reflect[/<target>][?readme=1]. \
        Multiple instances coexist.
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect":
            return [
                "id": .string(agentId.value),
                "kind": .string("web_rest"),
                "sentence": .string(
                    "REST verb-invocation surface; POST /<self>/<target_id> body=payload."
                ),
                "verbs": [
                    "get_routes":
                        "Returns this surface's HTTP route descriptors (POST /<self>/<target>, GET /<self>/_reflect[/<target>]).",
                    "handle_route":
                        "Host calls this on a route match: dispatches kernel.send(target, body) and returns {status, content_type, body}.",
                    "boot": "No-op.", "shutdown": "No-op.",
                ] as JSON,
            ] as JSON
        case "get_routes":
            // Paths namespaced by this agent's own id (concrete), with
            // `{target}` as the only captured param — mirrors Python's
            // `f"/{id}/{{target_id}}"`.
            let me = agentId.value
            return .object([
                "routes": .array([
                    .object([
                        "kind": .string("http"), "method": .string("POST"),
                        "path": .string("/\(me)/{target}"),
                    ]),
                    .object([
                        "kind": .string("http"), "method": .string("GET"),
                        "path": .string("/\(me)/_reflect"),
                    ]),
                    .object([
                        "kind": .string("http"), "method": .string("GET"),
                        "path": .string("/\(me)/_reflect/{target}"),
                    ]),
                ])
            ])
        case "handle_route":
            return await handleRoute(agentId: agentId, payload: payload, kernel: kernel)
        case "boot", "shutdown", "stop":
            return .object(["ok": .bool(true)])
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    /// Serve a matched HTTP request. Returns `{status, content_type,
    /// body}` for the host to translate into an HTTPResponse.
    private func handleRoute(agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let method = payload["method"].asString ?? "GET"
        let target = payload["params"]["target"].asString
        let body = payload["body"].asString ?? ""

        if method == "POST" {
            guard let target = target else {
                return jsonResponse(400, .object(["error": .string("web_rest: missing target")]))
            }
            // Verb comes from the body's `type`.
            guard let parsed = try? JSON.parse(body), case .object = parsed else {
                return jsonResponse(
                    400,
                    .object(["error": .string("web_rest: body must be a JSON object")]))
            }
            // GATE — web_rest is an io_bridge inbound (http) derivation: SEALED by
            // default. Gate the inbound call with THIS leg's ingress_rule (the
            // credential rides the X-Fantastic-Auth header in py/rust; threading
            // request headers here is a follow-on, so password legs need it).
            if let denied = gateWebLeg(
                kernel: kernel, legId: agentId, target: target,
                verb: parsed["type"].asString ?? "", token: payload["auth_token"].asString)
            {
                return jsonResponse(403, denied)
            }
            let reply = await kernel.send(AgentId(target), parsed)
            if case .null = reply {
                return .object([
                    "status": .integer(204),
                    "content_type": .string("application/json"),
                    "body": .string(""),
                ])
            }
            return jsonResponse(200, reply)
        }

        // GET _reflect[/target] → reflect kernel or a specific agent.
        let reflectTarget = target ?? "kernel"
        var reflectPayload: OrderedDictionary<String, JSON> = ["type": .string("reflect")]
        let rr = payload["query"]["readme"].asString
        if rr == "true" || rr == "1" {
            reflectPayload["readme"] = .bool(true)
        }
        let reply = await kernel.send(AgentId(reflectTarget), .object(reflectPayload))
        return jsonResponse(200, reply)
    }

    private func jsonResponse(_ status: Int, _ body: JSON) -> JSON {
        .object([
            "status": .integer(Int64(status)),
            "content_type": .string("application/json"),
            "body": .string(body.serialize()),
        ])
    }
}
