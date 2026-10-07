// web_ws — WebSocket verb-invocation surface as a child of `web`.
//
// Mirrors Python's `web_ws` bundle: it runs no server. It is a
// route-contributor — at host boot, the parent `web` pulls this agent's
// `get_routes` (the DECLARATION, pure data) and mounts the `/{host_id}/ws`
// route with the LIVE endpoint this bundle hands over through
// `routeEndpoints` (python `route_endpoints(agent)`): the WS proxy
// (`Proxy.swift`, python `web_ws/_proxy.run`), bound to this leg and to the
// `host_id` the path captured.
//
// Opt-in: a `web` agent serves WS only when a `web_ws` child exists —
// exactly like Python. Create one with:
//   <web_id> create_agent handler_module=web_ws.tools
//
// inbound_call / inbound_emit / inbound_watch / inbound_state_subscribe — the
// INBOUND half (`Inbound`): the socket's frame lane hands each admitted frame to
// this leg as one of these, and the leg performs it AS ITSELF — gated as the leg.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "web_ws.tools"

public final class WebWSBundle: AgentBundle, WebRouteEndpoints, @unchecked Sendable {
    public let name = "web_ws"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }
    public init() {}

    /// python `route_endpoints(agent)`: the live endpoint of the one declared
    /// path — the proxy, run as THIS leg for the `host_id` the upgrade captured
    /// (python `_make_endpoint(agent.id, kernel)` → `run(ws, kernel, host_id,
    /// leg_id)`).
    public func routeEndpoints(agent: Agent, kernel: Kernel) -> [String: WebEndpoint] {
        let legId = agent.id
        return [
            "/{host_id}/ws": .websocket { upgrade in
                WebSocketProxyHandler(
                    hostId: upgrade.params["host_id"] ?? "", legId: legId, kernel: kernel,
                    bootParent: upgrade.bootParent)
            }
        ]
    }

    public var readme: String? { Self.readmeText }

    /// The leg's `ingress_rule`, refused at the WRITE when no rule implements it
    /// — the one check every leg makes (python `validate_meta` →
    /// `io_bridge.unknown_ingress_refusal`).
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        IngressRules.unknownRefusal(meta)
    }

    /// The INBOUND verbs ride the binary channel too: a handed-in call or emit
    /// may carry a request body, and a call's reply chunk comes back raw.
    public func handleBinary(
        agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
    ) async throws -> (JSON?, Blob?) {
        if Inbound.handles(header["type"].asString ?? "") {
            return await Inbound.handle(
                agentId: agentId, payload: header, body: body, rawReply: true, kernel: kernel)
        }
        return (try await handle(agentId: agentId, payload: header, kernel: kernel), nil)
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let type = payload["type"]
        if Inbound.handles(type.asString ?? "") {
            return await Inbound.handle(
                agentId: agentId, payload: payload, body: nil, rawReply: false, kernel: kernel
            ).0
        }
        switch type.asString {
        case "reflect":
            // python `_reflect` key order: id, sentence, path_pattern,
            // **_describe(rec), verbs. No `kind` — that was a swift-only key.
            var out: OrderedDictionary<String, JSON> = [
                "id": .string(agentId.value),
                "sentence": .string(
                    "WS verb-invocation surface; mounts /<host_id>/ws on the parent web."
                ),
                "path_pattern": .string("/{host_id}/ws"),
            ]
            // The leg is a CHANNEL — surface its auth posture so
            // discovery-through-denial has something to discover.
            for (k, v) in describe(kernel.agent(agentId)?.meta ?? [:]) { out[k] = v }
            return .object(out)
        case "boot":
            // python `_boot` → None. The parent `web` mounts the route by
            // pulling `get_routes`.
            return nil
        case "get_routes":
            // `{host_id}` is a path template captured by the host's route
            // matcher — one web_ws serves WS for every agent id, matching
            // python's `path: "/{host_id}/ws"`. Pure data, NO `method`: the
            // live endpoint goes over through `routeEndpoints`, never a reply.
            return .object([
                "routes": .array([
                    .object([
                        "kind": .string("websocket"),
                        "path": .string("/{host_id}/ws"),
                    ])
                ])
            ])
        default:
            // python `refusal(UNHANDLED, f"web_ws: unknown type {t!r}", verb=t)`.
            // `shutdown` is NOT a verb here — a lifecycle hook that this bundle
            // never advertised, so the shutdown walk gets the same refusal.
            return Refusal.make(
                Refusal.unhandled, "web_ws: unknown type \(pyRepr(type))",
                extra: [("verb", type)])
        }
    }

    /// python's `VERBS` table, first doc line each.
    static let verbDocs: OrderedDictionary<String, JSON> = [
        "reflect": .string(
            "Identity + the URL pattern this surface mounts + this leg's auth posture"),
        "boot": .string(
            "No-op. The parent `web` agent calls `get_routes` on this bundle during its own boot and mounts the WS endpoint onto its FastAPI app."
        ),
        "get_routes": .string("Declares this surface's HTTP/WS route mounts. No args. Returns"),
        "inbound_call": Inbound.verbDocs["inbound_call"]!,
        "inbound_emit": Inbound.verbDocs["inbound_emit"]!,
        "inbound_watch": Inbound.verbDocs["inbound_watch"]!,
        "inbound_state_subscribe": Inbound.verbDocs["inbound_state_subscribe"]!,
    ]

    /// python `PB/io/web_ws/src/web_ws/readme.md`, byte for byte.
    static let readmeText = """
        # web_ws — inbound WebSocket leg

        Mounts `ws://host/<id>/ws` on a parent `web` agent. **Sealed by default** — a bare leg
        denies all frames. Open: `update_agent <id> ingress_rule=allow_all_ingress`.
        Token on frame **envelope** (`auth_token`, sibling of `type`/`target` — never inside payload).

        Frames: `call` · `emit` · `watch` · `unwatch` · `state_subscribe` · `state_unsubscribe` · `hello`
        Binary frames supported (4-byte-BE-length header + JSON envelope + raw bytes): the
        envelope holds `null` where the bytes belong and `_binary_path`, the steps to them — a
        JSON array, a key as a string (taken literally: `["payload","a.b"]` is the key `"a.b"`),
        a list index as an int (`["payload","items",0,"data"]`).

        `{type:"call", target, payload, id?}` — the reply is the call's result.

        **One leg is one principal**: every admitted frame (call, emit, watch,
        state_subscribe) is handed to this leg (its `inbound_*` verbs), which performs it AS
        ITSELF — its `can_call` and the target's `callable_by` decide, and execution names
        are one caller's. Everyone admitted to one leg is one tenant.

        **The page as a peer kernel.** A client that is itself a kernel (the browser's ts
        kernel) sends `{type:"hello", guid}`; this leg then registers that guid with the host
        kernel, which routes `send`s to `<guid>|<agent>` over this socket as
        `{type:"call", id, target, payload}` frames — answered by
        `{type:"reply"|"error", id, data}`. One guid per connection (pinned by the first
        hello); a sealed leg carries no peer. **The tabs of one identity are one principal**
        (owner, 2026-10-03): a guid already live on another connection is another tab of it,
        and this connection joins it — a host call to `<guid>|<agent>` goes to EVERY live
        connection of that guid and settles with the first reply (the others are dropped); a
        host emit on `<guid>|<agent>` goes to every one as `{type:"emit", target, payload}`,
        and a reply this leg sends one tab is mirrored to the others as `{type:"reply_mirror", id, data}`. When a socket closes, that connection stops
        being a route (the guid stays an address while another connection holds it) and every
        call still waiting on it settles as a `transport_error`. This leg only carries the frames: the
        host kernel routes and gates the sender, the page kernel gates its own agents.

        """
}
