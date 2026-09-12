// web_rest — REST verb-invocation surface as a child of `web`.
//
// Mirrors Python's `web_rest`: a route-contributor child that runs no
// server. The parent `web` pulls `get_routes` at boot and mounts the
// returned HTTP descriptors; when one matches, the host calls this
// bundle's `handle_route` verb. Canonical wire shape (Python parity):
//
//   POST /<self_id>/<target>      body={type:verb,...} → kernel.send([Item(target, body)])[0].result
//   GET  /<self_id>/_reflect              → kernel.send("kernel", {reflect})
//   GET  /<self_id>/_reflect/<target>     → kernel.send([Item(target, {reflect})])[0].result
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
        Multiple instances coexist. \
        Sealed by default — a bare leg replies 403 {reason:"unauthorized"}; open it with \
        update_agent <id> ingress_rule=allow_all_ingress (or password). The credential \
        rides the X-Fantastic-Auth header. Name an execution with the \
        X-Fantastic-Execution-Id header — the same id twice returns the first outcome \
        instead of running the verb again (dispatch metadata on the carrier, never in \
        the body).
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
                // Python's VERBS table. `handle_route` is the host's internal
                // edge and `shutdown` a lifecycle hook — neither is advertised.
                "verbs": [
                    "reflect": "Identity + URL patterns + curl examples + this leg's auth posture",
                    "boot": "No-op. The parent `web` agent calls `get_routes` on this bundle during its own boot and mounts the HTTP endpoint onto its FastAPI app.",
                    "get_routes": "Declares this surface's HTTP routes. Returns",
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
        case "boot":
            return .object(["ok": .bool(true)])
        case "shutdown":
            return .null
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
            // default. Gate the inbound call with THIS leg's ingress_rule. The
            // credential rides the X-Fantastic-Auth header, as in py/rust; the
            // host lowercases every header before handing them over.
            let headers = payload["headers"]
            let token = headers["x-fantastic-auth"].asString
                ?? payload["auth_token"].asString
            if let denied = gateWebLeg(
                kernel: kernel, legId: agentId, target: target,
                verb: parsed["type"].asString ?? "", token: token)
            {
                return jsonResponse(403, denied)
            }
            // NAMING an execution — py/rust parity. An execution id is dispatch
            // metadata about the call, not part of the verb payload, so it
            // rides its own header beside the credential. With it,
            // ensure-semantics reach the wire: an unknown id dispatches, an
            // in-flight id JOINS, and a settled id returns the recorded
            // outcome instead of running the verb a second time.
            var item = Item(AgentId(target), parsed)
            if let execId = headers["x-fantastic-execution-id"].asString,
               !execId.isEmpty
            {
                item.id = execId
            }
            let reply = await kernel.send([item])[0].result
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
        let reply = await kernel.send([Item(AgentId(reflectTarget), .object(reflectPayload))])[0].result
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
