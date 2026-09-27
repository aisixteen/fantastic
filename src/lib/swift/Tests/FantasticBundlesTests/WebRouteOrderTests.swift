// A LITERAL PATH BEATS A PATTERN. `web` resolves first-match-wins, and legs
// mount in the children's order on a hydrated host — so a `web_ws` leg's
// catch-all `/{host_id}/ws`, mounted first, swallowed a literal `/…/ws` another
// leg declared. The live table is ordered by parameter count (fewer first,
// stable), after every mount and unmount.
//
// Twin of python `web/host/tests/test_mount.py::
// test_a_literal_route_beats_a_catch_all_mounted_before_it`.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticWeb

/// A leg that declares ONE literal websocket path.
private struct LiteralLeg: AgentBundle {
    let name = "literal_leg"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard payload["type"].asString == "get_routes" else { return nil }
        return .object([
            "routes": .array([
                .object(["kind": .string("websocket"), "path": .string("/z_literal/ws")])
            ])
        ])
    }
}

private func send(_ k: Kernel, _ target: String, _ payload: JSON) async -> JSON {
    await k.send([Item(AgentId(target), payload)])[0].result
}

@Suite("web — route order", .serialized)
struct WebRouteOrderTests {
    @Test func aLiteralRouteBeatsACatchAllMountedBeforeIt() async throws {
        let k = try await startWebKernel()
        k.bundles.register("literal_leg.tools", LiteralLeg())
        _ = await send(k, "web", .object(["type": .string("boot")]))
        defer { Task { _ = await send(k, "web", .object(["type": .string("stop")])) } }
        for (id, module) in [("a_ws", "web_ws.tools"), ("z_literal", "literal_leg.tools")] {
            let out = await send(
                k, "kernel",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("web"),
                    "handler_module": .string(module), "id": .string(id),
                    "ingress_rule": .string("allow_all_ingress"),
                ]))
            #expect(out["id"].asString == id, "\(out.serialize())")
        }
        let web = try #require(k.bundles.get("web.tools") as? WebBundle)
        let server = try #require(web.serverFor(AgentId("web")))

        func resolved() -> AgentId? {
            server.matchWebSocket(segments: rawSegments("/z_literal/ws"))?.0.ownerId
        }
        func tableOrder() -> [String] {
            server.routeTable().compactMap {
                if case .leg(let owner) = $0.target { return owner.value }
                return nil
            }
        }

        // the catch-all leg mounts FIRST
        for id in ["a_ws", "z_literal"] {
            _ = await send(k, "web", .object(["type": .string("mount"), "child_id": .string(id)]))
        }
        #expect(tableOrder() == ["z_literal", "a_ws"], "\(tableOrder())")
        #expect(resolved() == AgentId("z_literal"))
        // …and after an unmount + re-mount (the literal now mounted LAST)
        _ = await send(k, "web", .object(["type": .string("unmount"), "child_id": .string("z_literal")]))
        #expect(resolved() == AgentId("a_ws"), "the pattern serves it while the literal is gone")
        _ = await send(k, "web", .object(["type": .string("mount"), "child_id": .string("z_literal")]))
        #expect(resolved() == AgentId("z_literal"))
        // mount order is still what `routes_by_child` reports
        #expect(server.routesByChild().keys.map { $0.value } == ["a_ws", "z_literal"])
    }

    /// Equal parameter counts keep their mount order (a STABLE sort).
    @Test func equalParameterCountsKeepMountOrder() {
        let specs = [
            RouteSpec(kind: .http, method: "GET", path: "/{a}/x", ownerId: AgentId("one")),
            RouteSpec(kind: .http, method: "GET", path: "/lit", ownerId: AgentId("two")),
            RouteSpec(kind: .http, method: "GET", path: "/{b}/y", ownerId: AgentId("three")),
            RouteSpec(kind: .http, method: "GET", path: "/{c}/{d:path}", ownerId: AgentId("four")),
            RouteSpec(kind: .http, method: "GET", path: "/lit2", ownerId: AgentId("five")),
        ]
        #expect(
            orderRoutes(specs).map { $0.ownerId.value } == ["two", "five", "one", "three", "four"])
    }
}
