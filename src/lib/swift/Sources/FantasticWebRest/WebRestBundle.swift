// web_rest — REST verb-invocation surface as a child of `web`.
//
// Mirrors Python's `web_rest`: a route-contributor child that runs no
// server. The parent `web` pulls `get_routes` at boot and mounts the
// returned HTTP descriptors; when one matches, the host calls this
// bundle's `serveRoute` (the `WebRouteOwner` seam — python hands the host
// a callable there, and a served request is NOT an agent send on the leg).
// Canonical wire shape (Python parity):
//
//   POST /<self_id>/<target_id>           body={type:verb,...} → JSON reply
//   GET  /<self_id>/_reflect              → reflect `kernel`
//   GET  /<self_id>/_reflect/<target_id>  → reflect one agent
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

public final class WebRestBundle: AgentBundle, WebRouteOwner, @unchecked Sendable {
    public let name = "web_rest"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public init() {}

    public var readme: String? { Self.readmeText }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let type = payload["type"]
        switch type.asString {
        case "reflect":
            let me = agentId.value
            // python `_reflect` key order: id, sentence, path_pattern, method,
            // reflect_url, reflect_pattern, auth_header, curl_post,
            // curl_reflect, curl_reflect_target, **_describe(rec), verbs.
            var out: OrderedDictionary<String, JSON> = [
                "id": .string(me),
                "sentence": .string(
                    "REST verb-invocation surface; POST /<self>/<target_id> body=payload."
                ),
                "path_pattern": .string("/\(me)/{target_id}"),
                "method": .string("POST"),
                "reflect_url": .string("/\(me)/_reflect"),
                "reflect_pattern": .string("/\(me)/_reflect/{target_id}"),
                "auth_header": .string("X-Fantastic-Auth"),
                "curl_post": .string(
                    "curl -X POST -H \"content-type: application/json\" "
                        + "-d '{\"type\":\"reflect\"}' http://<host>/\(me)/<target_id>"),
                "curl_reflect": .string(
                    "curl http://<host>/\(me)/_reflect           # root reflect (tree)"),
                "curl_reflect_target": .string(
                    "curl http://<host>/\(me)/_reflect/<target_id>  # any agent"),
            ]
            // The REST face is a CHANNEL — surface its auth posture so a denied
            // client can find the door (discovery-through-denial).
            for (k, v) in describe(kernel.agent(agentId)?.meta ?? [:]) { out[k] = v }
            out["verbs"] = .object(Self.verbDocs)
            return .object(out)
        case "boot":
            // python `_boot` → None.
            return nil
        case "get_routes":
            // Paths namespaced by this agent's own id (concrete), with
            // `{target_id}` as the only captured param — python's
            // `f"/{id}/{{target_id}}"`. python's descriptors also carry
            // `endpoint` (a live callable), which JSON cannot hold.
            let me = agentId.value
            return .object([
                "routes": .array([
                    .object([
                        "kind": .string("http"), "method": .string("POST"),
                        "path": .string("/\(me)/{target_id}"),
                    ]),
                    .object([
                        "kind": .string("http"), "method": .string("GET"),
                        "path": .string("/\(me)/_reflect"),
                    ]),
                    .object([
                        "kind": .string("http"), "method": .string("GET"),
                        "path": .string("/\(me)/_reflect/{target_id}"),
                    ]),
                ])
            ])
        default:
            // python `refusal(UNHANDLED, f"web_rest: unknown type {t!r}", verb=t)`.
            // `handle_route` is deliberately absent — serving a route is the
            // `WebRouteOwner` seam, not an agent verb.
            return Refusal.make(
                Refusal.unhandled, "web_rest: unknown type \(pyRepr(type))",
                extra: [("verb", type)])
        }
    }

    // MARK: - WebRouteOwner (python's mounted endpoints)

    public func serveRoute(legId: AgentId, request: JSON, kernel: Kernel) async -> (JSON, Data?) {
        let method = request["method"].asString ?? "GET"
        let target = request["params"]["target_id"].asString
        if method == "POST" {
            return await postEndpoint(
                legId: legId, target: target, request: request, kernel: kernel)
        }
        // GET `_reflect` (root) and `_reflect/{target_id}` (one agent).
        return (
            await reflectEndpoint(
                legId: legId, target: target ?? "kernel", request: request, kernel: kernel), nil
        )
    }

    /// python `_make_post_endpoint._rest_call` (JSON leg; the octet leg is the
    /// host's, because a `{status, content_type, body}` JSON reply cannot carry
    /// raw bytes or response headers).
    private func postEndpoint(
        legId: AgentId, target: String?, request: JSON, kernel: Kernel
    ) async -> (JSON, Data?) {
        guard let target = target else {
            return (
                Self.jsonResponse(400, .object(["error": .string("web_rest: missing target")])), nil
            )
        }
        let body = request["body"].asString ?? ""
        let payload: JSON
        if body.isEmpty {
            // python `json.loads(body) if body else {}`.
            payload = .object([:])
        } else {
            guard let parsed = try? JSON.parse(body) else {
                // PYTHON PARSES THE THREE NON-FINITE CONSTANTS (`allow_nan`), so
                // a bare `NaN` / `Infinity` / `-Infinity` body is a FLOAT there
                // — valid input, just not a dict, which is the object refusal
                // below and not a parse error. Swift's codec refuses them (it
                // mirrors serde_json), so the distinction is spelled here rather
                // than by widening the codec for every reader.
                if ["NaN", "Infinity", "-Infinity"].contains(
                    body.trimmingCharacters(in: .whitespacesAndNewlines))
                {
                    return (
                        Self.jsonResponse(
                            400,
                            .object(["error": .string("web_rest: body must be a JSON object")])),
                        nil
                    )
                }
                // Don't echo the parser exception back — generic message only
                // (CodeQL: information exposure through an exception).
                return (
                    Self.jsonResponse(
                        400,
                        .object(["error": .string("web_rest: request body is not valid JSON")])),
                    nil
                )
            }
            guard case .object = parsed else {
                return (
                    Self.jsonResponse(
                        400, .object(["error": .string("web_rest: body must be a JSON object")])),
                    nil
                )
            }
            payload = parsed
        }
        // GATE — web_rest is an io_bridge inbound (http) derivation: SEALED by
        // default. The credential rides the X-Fantastic-Auth header; the host
        // lowercases every header before handing them over.
        let headers = request["headers"]
        if let denied = gateWebLeg(
            kernel: kernel, legId: legId, kind: "call", target: target,
            verb: payload["type"].asString ?? "", token: headers["x-fantastic-auth"].asString)
        {
            return (Self.jsonResponse(403, denied), nil)
        }
        // NAMING an execution. The credential rides `X-Fantastic-Auth`; an
        // execution id rides its own header for the same reason — it is dispatch
        // metadata about the call, not part of the verb payload the target agent
        // reads. With it, ensure-semantics reach the wire: an unknown id
        // dispatches, an in-flight id JOINS, and a settled id returns the
        // recorded outcome rather than running the verb twice.
        var item = Item(AgentId(target), payload)
        if let execId = headers["x-fantastic-execution-id"].asString, !execId.isEmpty {
            item.id = execId
        }
        // A BINARY-EDGE VERB RIDES THE BINARY CHANNEL even on the JSON leg
        // (A13.1): that is the only way the bundle returns — or receives — a raw
        // chunk. Python needs no such list; its payload dict simply HOLDS the
        // bytes and `_first_bytes_field` finds them in the reply. Here the
        // carrier is the item's blob, so the decision is static, by verb, and
        // identical on both legs. The request blob is EMPTY: a JSON body cannot
        // hold one, so `write_stream` with a JSON-string `bytes` reaches
        // file_bridge as a header `bytes` key and is refused `missing_arg`
        // (A13.19), which is python's answer too.
        if Self.binaryEdgeVerbs.contains(payload["type"].asString ?? "") {
            item.binary = Data()
        }
        let out = await KernelTaskLocals.$currentSender.withValue(legId) {
            await kernel.send([item])[0]
        }
        if case .null = out.result { return (Self.noContent, nil) }
        // The host turns a CHUNK — a 0-byte one included — into the octet reply;
        // a refusal carries none and stays JSON. `payload_header` is the reply
        // dict rendered with python's `json.dumps` DEFAULTS, which is what rides
        // `X-Fantastic-Payload`; the JSON body uses the compact separators
        // starlette's `JSONResponse` writes, so the two are not the same bytes.
        if let chunk = out.body {
            var route = Self.jsonResponse(200, out.result)
            if case .object(var o) = route {
                o["payload_header"] = .string(pyJsonDumps(out.result))
                route = .object(o)
            }
            return (route, chunk)
        }
        return (Self.jsonResponse(200, out.result), nil)
    }

    /// The verbs a JSON-leg POST must route down the BINARY channel, chosen
    /// STATICALLY by verb — the SAME pair the WS leg uses, so a browser and a
    /// curl client get the same answer for the same request (A11.9/A13.1).
    static let binaryEdgeVerbs: Set<String> = ["read_stream", "write_stream"]

    /// python `_make_reflect_root` / `_make_reflect_get`.
    private func reflectEndpoint(
        legId: AgentId, target: String, request: JSON, kernel: Kernel
    ) async -> JSON {
        let payload = Self.reflectPayload(request["query"])
        if let denied = gateWebLeg(
            kernel: kernel, legId: legId, kind: "call", target: target, verb: "reflect",
            token: request["headers"]["x-fantastic-auth"].asString)
        {
            return Self.jsonResponse(403, denied)
        }
        let reply = await KernelTaskLocals.$currentSender.withValue(legId) {
            await kernel.send([Item(AgentId(target), payload)])[0].result
        }
        if case .null = reply { return Self.notFound }
        return Self.jsonResponse(200, reply)
    }

    /// python `_reflect_payload` — compose the reflect payload from query flags
    /// so the browser-pastable GET shortcuts work.
    static func reflectPayload(_ query: JSON) -> JSON {
        var payload: OrderedDictionary<String, JSON> = ["type": .string("reflect")]
        if let rm = query["readme"].asString {
            if ["1", "true", "yes"].contains(rm) {
                payload["readme"] = .bool(true)
            } else if !rm.isEmpty && !["0", "false", "no"].contains(rm) {
                // a doc key: `?readme=index` / `?readme=self-extension`
                payload["readme"] = .string(rm)
            }
        }
        if let tree = query["tree"].asString, ["all", "ids", "none"].contains(tree) {
            payload["tree"] = .string(tree)
        }
        if let bundles = query["bundles"].asString, ["all", "ids", "none"].contains(bundles) {
            payload["bundles"] = .string(bundles)
        }
        return .object(payload)
    }

    /// python `JSONResponse(body, status_code=status)` — starlette renders it
    /// `json.dumps(..., ensure_ascii=False, separators=(",", ":"))`, i.e. compact
    /// raw UTF-8, which is what `serialize()` writes.
    static func jsonResponse(_ status: Int, _ body: JSON) -> JSON {
        .object([
            "status": .integer(Int64(status)),
            "content_type": .string("application/json"),
            "body": .string(body.serialize()),
        ])
    }

    /// python `Response(status_code=204)` — no content-type, no body.
    static let noContent: JSON = .object(["status": .integer(204)])
    /// python `Response(status_code=404)` for a null reflect reply.
    static let notFound: JSON = .object(["status": .integer(404)])

    /// python's `VERBS` table, first doc line each.
    static let verbDocs: OrderedDictionary<String, JSON> = [
        "reflect": .string(
            "Identity + URL patterns + curl examples + this leg's auth posture"),
        "boot": .string(
            "No-op. The parent `web` agent calls `get_routes` on this bundle during its own boot and mounts the HTTP endpoint onto its FastAPI app."
        ),
        "get_routes": .string("Declares this surface's HTTP routes. Returns"),
    ]

    /// python `PB/io/web_rest/src/web_rest/readme.md`, byte for byte.
    static let readmeText = """
        # web_rest — inbound HTTP leg

        `POST /<id>/<target>` body=`{"type":"<verb>",...}` → JSON reply.
        `GET /<id>/_reflect[/<target>][?readme=1]` — reflect shortcuts.
        **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
        Token on **`X-Fantastic-Auth` header** (not the envelope — HTTP has no envelope).
        Name an execution with **`X-Fantastic-Execution-Id`** — same id twice returns the
        first outcome instead of running the verb again (dispatch metadata on the
        carrier, never in the body).

        """
}
