// web_rest — REST verb-invocation surface as a child of `web`.
//
// Mirrors Python's `web_rest`: a route-contributor child that runs no
// server. The parent `web` pulls `get_routes` at boot (the DECLARATION) and
// mounts each declared path with the live endpoint this bundle hands over
// through `routeEndpoints` (python `route_endpoints(agent)`) — a served
// request is NOT an agent send on the leg.
// Canonical wire shape (Python parity):
//
//   POST /<self_id>/<target_id>           body={type:verb,...} → JSON reply
//                                         headers: X-Fantastic-Auth (credential),
//                                         X-Fantastic-Execution-Id (the item `id`),
//                                         X-Fantastic-Secret (the item `secret`, a
//                                         JSON array of marks — dotted strings
//                                         and/or arrays of steps)
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

public final class WebRestBundle: AgentBundle, WebRouteEndpoints, @unchecked Sendable {
    public let name = "web_rest"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public init() {}

    public var readme: String? { Self.readmeText }

    /// The leg's `ingress_rule`, refused at the WRITE when no rule implements it
    /// — the one check every leg makes (python `validate_meta` →
    /// `io_bridge.unknown_ingress_refusal`).
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        IngressRules.unknownRefusal(meta)
    }

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
            // `f"/{id}/{{target_id}}"`. Pure data: the live endpoints go over
            // through `routeEndpoints`, never a reply.
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
            // endpoint handed over through `routeEndpoints`, not an agent verb.
            return Refusal.make(
                Refusal.unhandled, "web_rest: unknown type \(pyRepr(type))",
                extra: [("verb", type)])
        }
    }

    // MARK: - the live endpoints (python `route_endpoints(agent)`)

    /// One endpoint per declared path, all served by `serveRoute` as THIS leg.
    public func routeEndpoints(agent: Agent, kernel: Kernel) -> [String: WebEndpoint] {
        let me = agent.id.value
        let legId = agent.id
        let endpoint: WebEndpoint = .http { [self] request in
            await self.serveRoute(legId: legId, request: request, kernel: kernel)
        }
        return [
            "/\(me)/{target_id}": endpoint,
            "/\(me)/_reflect": endpoint,
            "/\(me)/_reflect/{target_id}": endpoint,
        ]
    }

    /// Serve one matched request (see `WebEndpoint.http`).
    func serveRoute(legId: AgentId, request: JSON, kernel: Kernel) async -> (JSON, OctetReply?) {
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
    ) async -> (JSON, OctetReply?) {
        guard let target = target else {
            return (
                Self.jsonResponse(400, .object(["error": .string("web_rest: missing target")])), nil
            )
        }
        // `null` = the host could not decode the bytes as text: python's
        // `json.loads` raises `UnicodeDecodeError` there, caught with the
        // `JSONDecodeError` — the same 400.
        if request["body"] == .null, request.asObject?["body"] != nil {
            return (
                Self.jsonResponse(
                    400, .object(["error": .string("web_rest: request body is not valid JSON")])),
                nil
            )
        }
        let body = request["body"].asString ?? ""
        let payload: JSON
        if body.isEmpty {
            // python `json.loads(body) if body else {}`.
            payload = .object([:])
        } else {
            // STRICT, as python's `strict_loads` (io_bridge.loads, owner D13):
            // `NaN`/`Infinity`, an integer outside int64 and an overflowing
            // number are not valid JSON on the wire.
            guard let parsed = try? JSON.parseStrict(body) else {
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
        // GATED BEFORE THE BODY WAS READ (audit IO-4): the host ran this leg's
        // ingress rule on the request's head — its target and `X-Fantastic-Auth`
        // (`WebServer.preflight`) — and bounded the body; only an admitted
        // request reaches here. The host lowercases every header.
        let headers = request["headers"]
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
        // SECRET MARKS: this kernel records the call as shown, exactly as for a
        // local caller; a malformed mark is the kernel's own refusal.
        if let raw = headers.asObject?[Self.secretHeader] {
            guard let marks = try? JSON.parseStrict(raw.asString ?? "") else {
                return (
                    Self.jsonResponse(
                        400,
                        .object(["error": .string("web_rest: X-Fantastic-Secret is not valid JSON")])
                    ), nil
                )
            }
            item.secret = marks
        }
        // EVERY CALL MAY BE ANSWERED WITH RAW BYTES, whatever its verb (A13.1):
        // python's reply dict holds them and `_first_bytes_field` finds them,
        // with no list of verbs. This leg carries the reply's body
        // (`rawReply`), and answers octet exactly when the reply holds one.
        // There is NO request body: a JSON body cannot hold one, so
        // `write_stream` with a JSON-string `bytes` reaches file_bridge as a
        // header `bytes` key and is refused `missing_arg` (A13.19), which is
        // python's answer too.
        item.rawReply = true
        let out = await KernelTaskLocals.$currentSender.withValue(legId) {
            await kernel.send([item])[0]
        }
        if case .null = out.result { return (Self.noContent, nil) }
        // The host turns a CHUNK — a 0-byte one included — into the octet reply;
        // a refusal carries none and stays JSON. The octet reply's header is the
        // reply dict WITHOUT the chunk's key, rendered with python's `json.dumps`
        // DEFAULTS, and the key rides `X-Fantastic-Bytes-Field` (`RestOctet.split`,
        // python `_first_bytes_field`); the JSON body uses the compact separators
        // starlette's `JSONResponse` writes, so the two are not the same bytes.
        if let chunk = out.body {
            guard let octet = RestOctet.split(out.result, chunk) else {
                return (Self.jsonResponse(500, RestOctet.noJSONForm), nil)
            }
            return (Self.jsonResponse(200, out.result), octet)
        }
        return (Self.jsonResponse(200, out.result), nil)
    }

    /// The item's `secret` marks (`Secret`) ride `X-Fantastic-Secret` (the host
    /// lowercases every header) — dispatch metadata about the call, like the
    /// execution id, never part of the payload. The value is a JSON ARRAY of
    /// marks, each a dotted string (`"payload.key"`) or an array of steps
    /// (`["payload", "a.b"]` — a key holding a dot), the SAME value a ws/bridge
    /// `call` frame carries in its `secret` field: one parser (the strict wire
    /// parse), and the kernel's own check judges it. Not comma-separated — a path
    /// step is a payload key, and a key may hold a comma; a JSON string escapes
    /// any character into the ASCII a header value is safe in; and a wrong shape
    /// (a bare string, `[]`, a number) reaches the kernel's refusal verbatim
    /// instead of being re-invented by a splitter. Python
    /// `web_rest._SECRET_HEADER`.
    static let secretHeader = "x-fantastic-secret"

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
        "get_routes": .string("Declares this surface's HTTP routes. No args. Returns"),
    ]

    /// python `PB/io/web_rest/src/web_rest/readme.md`, byte for byte.
    static let readmeText = """
        # web_rest — inbound HTTP leg

        `POST /<id>/<target>` body=`{"type":"<verb>",...}` → JSON reply.
        `GET /<id>/_reflect[/<target>][?readme=1]` — reflect shortcuts.
        **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
        Token on **`X-Fantastic-Auth` header** (not the envelope — HTTP has no envelope).
        **Gate first, then the body**: a POST is judged on its target and `X-Fantastic-Auth`
        before one body byte is read — a refused client's body is never read. An admitted
        body is read up to **16 MiB** (`MAX_BODY` = io_bridge `MAX_FRAME`, the one frame cap
        every wire leg holds); larger → `413` `{reason:"transport_error", limit}` ("the
        request body exceeds 16777216 bytes …") — a declared `Content-Length` over it is
        refused unread. Send more in chunks (`write_stream`).
        Name an execution with **`X-Fantastic-Execution-Id`** — same id twice returns the
        first outcome instead of running the verb again (dispatch metadata on the
        carrier, never in the body).
        Mark secret values with **`X-Fantastic-Secret`** — a JSON array of marks, the
        item's `secret`: dotted strings (`["payload.fields"]`, `["result"]`) and/or arrays
        of steps for a key holding a dot (`[["payload","a.b"]]`): the reply is real, every
        record of the call shows `<secret>`. Not JSON → `400`; a bad mark → the kernel's
        `bad_arg` refusal.
        **One leg is one principal** (audit S6, 2026-10-03): every request admitted to this
        leg is dispatched AS the leg, so grants, a credential_store's `readers`,
        `X-Fantastic-Execution-Id` names and a secret-marked execution's join (the real
        outcome goes to its original caller — the leg) see ONE caller. Everyone admitted to
        one leg is one principal — one tenant per leg.
        ```
        curl -X POST -H 'content-type: application/json' \\
          -H 'X-Fantastic-Secret: ["payload.fields"]' \\
          -d '{"type":"set","ref":"openai/main","fields":{"api_key":"…"}}' \\
          http://<host>/<id>/<creds_id>
        ```

        """
}
