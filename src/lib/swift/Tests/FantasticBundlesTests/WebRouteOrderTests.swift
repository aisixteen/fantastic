// A LITERAL PATH BEATS A PATTERN, and EVERY DECLARED ROUTE IS SERVED BY THE
// ENDPOINT ITS LEG HANDED OVER.
//
// `web` resolves first-match-wins, and legs mount in the children's order on a
// hydrated host — so a `web_ws` leg's catch-all `/{host_id}/ws`, mounted first,
// swallowed a literal `/…/ws` another leg declared. The live table is ordered by
// parameter count (fewer first, stable), after every mount and unmount.
//
// A leg declares its routes as data (`get_routes`) and hands the live endpoint
// per path through `WebRouteEndpoints` (python `route_endpoints(agent)`); an
// upgrade dispatches to the matched route's own endpoint (web_ws's is its
// proxy), and a leg declaring a route without an endpoint fails web boot loud.
//
// Twin of python `web/host/tests/test_mount.py::
// test_a_literal_route_beats_a_catch_all_mounted_before_it` (+ the
// `route_endpoints` refusal of `web/tools.py::_mount_surface`).

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import NIOCore
import NIOWebSocket
import Testing

@testable import FantasticWeb

/// The literal leg's own websocket endpoint: it answers every upgrade with ONE
/// marker frame (python's stub `websocket.send_text("literal")`).
private final class LiteralEndpoint: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = WebSocketFrame
    typealias OutboundOut = WebSocketFrame

    func handlerAdded(context: ChannelHandlerContext) {
        var buf = context.channel.allocator.buffer(capacity: 7)
        buf.writeString("literal")
        context.writeAndFlush(
            wrapOutboundOut(WebSocketFrame(fin: true, opcode: .text, data: buf)), promise: nil)
    }
}

/// A leg that declares ONE literal websocket path and hands over its endpoint.
private struct LiteralLeg: AgentBundle, WebRouteEndpoints {
    let name = "literal_leg"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard payload["type"].asString == "get_routes" else { return nil }
        return .object([
            "routes": .array([
                .object(["kind": .string("websocket"), "path": .string("/z_literal/ws")])
            ])
        ])
    }
    func routeEndpoints(agent: Agent, kernel: Kernel) -> [String: WebEndpoint] {
        ["/z_literal/ws": .websocket { _ in LiteralEndpoint() }]
    }
}

/// A leg that DECLARES a websocket route and hands over no endpoint for it.
private struct NoHookLeg: AgentBundle {
    let name = "no_hook_leg"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard payload["type"].asString == "get_routes" else { return nil }
        return .object([
            "routes": .array([
                .object(["kind": .string("websocket"), "path": .string("/nohook/ws")])
            ])
        ])
    }
}

private func send(_ k: Kernel, _ target: String, _ payload: JSON) async -> Outcome {
    await k.send([Item(AgentId(target), payload)])[0]
}

private func create(_ k: Kernel, _ id: String, _ module: String) async {
    let out = await send(
        k, "kernel",
        .object([
            "type": .string("create_agent"), "parent_id": .string("web"),
            "handler_module": .string(module), "id": .string(id),
            "ingress_rule": .string("allow_all_ingress"),
        ])
    ).result
    #expect(out["id"].asString == id, "\(out.serialize())")
}

/// A booted web host with the catch-all web_ws leg `a_ws` mounted BEFORE the
/// literal leg `z_literal`.
private func catchAllFirst() async throws -> Kernel {
    let k = try await startWebKernel()
    k.bundles.register("literal_leg.tools", LiteralLeg())
    _ = await send(k, "web", .object(["type": .string("boot")]))
    await create(k, "a_ws", "web_ws.tools")
    await create(k, "z_literal", "literal_leg.tools")
    for id in ["a_ws", "z_literal"] {
        _ = await send(k, "web", .object(["type": .string("mount"), "child_id": .string(id)]))
    }
    return k
}

/// One frame off a real upgrade, BOUNDED (a receive ignores task cancellation;
/// past `seconds` the socket is cancelled so the receive throws).
private func firstFrame(_ k: Kernel, _ path: String, seconds: UInt64 = 10) async throws -> String {
    let task = URLSession(configuration: .ephemeral)
        .webSocketTask(with: URL(string: "ws://127.0.0.1:\(k.httpPort())\(path)")!)
    task.resume()
    let timer = Task {
        try await Task.sleep(nanoseconds: seconds * 1_000_000_000)
        task.cancel(with: .goingAway, reason: nil)
    }
    defer {
        timer.cancel()
        task.cancel(with: .normalClosure, reason: nil)
    }
    switch try await task.receive() {
    case .string(let s): return s
    case .data(let d): return String(decoding: d, as: UTF8.self)
    @unknown default: return ""
    }
}

@Suite("web — route order and route endpoints", .serialized)
struct WebRouteOrderTests {
    @Test func aLiteralRouteBeatsACatchAllMountedBeforeIt() async throws {
        let k = try await catchAllFirst()
        defer { Task { _ = await send(k, "web", .object(["type": .string("stop")])) } }
        let web = try #require(k.bundles.get("web.tools") as? WebBundle)
        let server = try #require(web.serverFor(AgentId("web")))

        func resolved() -> AgentId? {
            server.matchWebSocket(segments: rawSegments("/z_literal/ws"))?.0.ownerId
        }
        func tableOrder() -> [String] {
            server.routeTable().compactMap {
                if case .leg(let spec) = $0.target { return spec.ownerId.value }
                return nil
            }
        }

        #expect(tableOrder() == ["z_literal", "a_ws"], "\(tableOrder())")
        #expect(resolved() == AgentId("z_literal"))
        // …and after an unmount + re-mount (the literal now mounted LAST)
        _ = await send(
            k, "web", .object(["type": .string("unmount"), "child_id": .string("z_literal")]))
        #expect(resolved() == AgentId("a_ws"), "the pattern serves it while the literal is gone")
        _ = await send(
            k, "web", .object(["type": .string("mount"), "child_id": .string("z_literal")]))
        #expect(resolved() == AgentId("z_literal"))
        // mount order is still what `routes_by_child` reports
        #expect(server.routesByChild().keys.map { $0.value } == ["a_ws", "z_literal"])
    }

    /// A re-mount REPLACES the leg's routes, and `unmount` removes them all — a
    /// second copy appended would keep serving after the unmount. Python
    /// `test_a_reboot_replaces_a_legs_routes_and_unmount_removes_them_all`.
    @Test func aRemountReplacesALegsRoutesAndUnmountRemovesThemAll() async throws {
        let k = try await catchAllFirst()
        defer { Task { _ = await send(k, "web", .object(["type": .string("stop")])) } }
        let web = try #require(k.bundles.get("web.tools") as? WebBundle)
        let server = try #require(web.serverFor(AgentId("web")))
        func paths() -> [String] {
            server.routeTable().compactMap {
                if case .leg(let spec) = $0.target, spec.ownerId == AgentId("z_literal") {
                    return spec.path
                }
                return nil
            }
        }
        let first = paths()
        #expect(!first.isEmpty)
        _ = await send(
            k, "web", .object(["type": .string("mount"), "child_id": .string("z_literal")]))
        #expect(paths() == first, "\(paths())")
        let out = await send(
            k, "web", .object(["type": .string("unmount"), "child_id": .string("z_literal")]))
            .result
        #expect(out == .object(["unmounted": .string("z_literal")]), "\(out.serialize())")
        #expect(paths().isEmpty, "\(paths())")
    }

    /// An actual upgrade to the literal path reaches THAT leg's own endpoint,
    /// though the web_ws catch-all was mounted before it.
    @Test(.timeLimit(.minutes(1)))
    func anUpgradeToTheLiteralPathReachesItsOwnEndpoint() async throws {
        let k = try await catchAllFirst()
        defer { Task { _ = await send(k, "web", .object(["type": .string("stop")])) } }
        #expect(try await firstFrame(k, "/z_literal/ws") == "literal")
    }

    /// Any other `/<id>/ws` still reaches the web_ws proxy: its connect-time
    /// auto-watch answers a name that is no agent with `unknown_agent`.
    @Test(.timeLimit(.minutes(1)))
    func anyOtherWsPathStillReachesTheWebWsProxy() async throws {
        let k = try await catchAllFirst()
        defer { Task { _ = await send(k, "web", .object(["type": .string("stop")])) } }
        let frame = try JSON.parse(try await firstFrame(k, "/elsewhere/ws"))
        #expect(frame["type"].asString == "error", "\(frame.serialize())")
        #expect(frame["reason"].asString == "unknown_agent", "\(frame.serialize())")
        #expect(frame["src"].asString == "elsewhere", "\(frame.serialize())")
    }

    /// A leg that declares a websocket route and hands over no endpoint fails
    /// web boot LOUD (python's RuntimeError naming `route_endpoints`).
    @Test func aLegDeclaringARouteWithoutAnEndpointFailsWebBoot() async throws {
        let k = try await startWebKernel()
        k.bundles.register("no_hook_leg.tools", NoHookLeg())
        _ = await send(k, "web", .object(["type": .string("stop")]))
        await create(k, "nohook", "no_hook_leg.tools")
        let boot = await send(k, "web", .object(["type": .string("boot")]))
        defer { Task { _ = await send(k, "web", .object(["type": .string("stop")])) } }
        #expect(boot.status == .failed, "\(boot.status) \(boot.result.serialize())")
        #expect(
            boot.result["error"].asString
                == "RuntimeError: web: nohook declares 1 route(s) but its bundle exposes no "
                + "route_endpoints(agent) hook",
            "\(boot.result.serialize())")
    }

    /// Equal parameter counts keep their mount order (a STABLE sort).
    @Test func equalParameterCountsKeepMountOrder() {
        let none: WebEndpoint = .http { _ in (.null, nil) }
        let specs = [
            RouteSpec(
                kind: .http, method: "GET", path: "/{a}/x", ownerId: AgentId("one"), endpoint: none),
            RouteSpec(
                kind: .http, method: "GET", path: "/lit", ownerId: AgentId("two"), endpoint: none),
            RouteSpec(
                kind: .http, method: "GET", path: "/{b}/y", ownerId: AgentId("three"),
                endpoint: none),
            RouteSpec(
                kind: .http, method: "GET", path: "/{c}/{d:path}", ownerId: AgentId("four"),
                endpoint: none),
            RouteSpec(
                kind: .http, method: "GET", path: "/lit2", ownerId: AgentId("five"), endpoint: none),
        ]
        #expect(
            orderRoutes(specs).map { $0.ownerId.value } == ["two", "five", "one", "three", "four"])
    }
}
