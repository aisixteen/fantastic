// The `web` host bundle and its two leg bundles, against python's
// `web/tools.py`, `web_rest/tools.py`, `web_ws/tools.py`.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation
import Testing

#if canImport(Darwin)
    import Darwin
#else
    import Glibc
#endif

/// Records what `cli` was told — python's `_boot` announces the bind by
/// sending `say` to the renderer rather than printing it itself.
final class SayProbe: AgentBundle, @unchecked Sendable {
    let name = "say_probe"
    private let lock = NSLock()
    private var said: [JSON] = []

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        lock.withLock { said.append(payload) }
        return nil  // python's cli `say` answers None
    }

    func lines() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return said
    }
}

/// A route contributor that reports whether the host's port was ALREADY
/// accepting when `get_routes` was pulled — python mounts every child surface
/// before uvicorn starts serving.
final class MountOrderProbe: AgentBundle, WebRouteEndpoints, @unchecked Sendable {
    let name = "mount_order_probe"

    /// The live endpoint of the one declared path (a leg that declares a route
    /// hands its endpoint over, or the host's boot fails).
    func routeEndpoints(agent: Agent, kernel: Kernel) -> [String: WebEndpoint] {
        ["/probe/ping": .http { _ in (.object(["status": .integer(204)]), nil) }]
    }
    private let lock = NSLock()
    private var acceptingAtMount: Bool?
    let port: UInt16

    init(port: UInt16) { self.port = port }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard payload["type"].asString == "get_routes" else { return nil }
        let seen = MountOrderProbe.canConnect(port)
        lock.withLock { acceptingAtMount = seen }
        return .object([
            "routes": .array([
                .object([
                    "kind": .string("http"), "method": .string("GET"),
                    "path": .string("/probe/ping"),
                ])
            ])
        ])
    }

    func sawListener() -> Bool? {
        lock.lock()
        defer { lock.unlock() }
        return acceptingAtMount
    }

    private static func canConnect(_ port: UInt16) -> Bool {
        let fd = socket(AF_INET, Int32(SOCK_STREAM), 0)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let ok = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        return ok == 0
    }
}

@Suite("web bundle", .serialized)
struct WebBundleTests {
    private func call(_ kernel: Kernel, _ id: String, _ payload: JSON) async -> JSON {
        await kernel.send([Item(AgentId(id), payload)])[0].result
    }

    // MARK: - readmes

    @Test func webReadmeIsPythonVerbatim() throws {
        let expected = try String(
            contentsOf: pythonSource("web/host/src/web/readme.md"), encoding: .utf8)
        #expect(WebBundle().readme == expected)
    }

    @Test func webRestReadmeIsPythonVerbatim() throws {
        let expected = try String(
            contentsOf: pythonSource("io/web_rest/src/web_rest/readme.md"), encoding: .utf8)
        #expect(WebRestBundle().readme == expected)
    }

    @Test func webWsReadmeIsPythonVerbatim() throws {
        let expected = try String(
            contentsOf: pythonSource("io/web_ws/src/web_ws/readme.md"), encoding: .utf8)
        #expect(WebWSBundle().readme == expected)
    }

    // MARK: - reflect keys

    /// Python's `_reflect` keys, in order. No `kind`, no `emits`;
    /// `routes` is the browser-facing contract dict, not an asset table.
    @Test func webReflectKeysMatchPython() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let r = await call(kernel, "web", .object(["type": .string("reflect")]))
        // The kernel appends its own reflect tail (can_call/callable_by/
        // kernel_verbs/tree) after the bundle's body — pin the bundle's half.
        let keys = Array((r.asObject ?? [:]).keys.map { String($0) }.prefix(10))
        #expect(
            keys == [
                "id", "sentence", "port", "host", "allowed_origins", "running", "served_agent_count", "surfaces", "routes",
                "browser_reachable",
            ], "\(keys)")
        #expect(
            r["sentence"].asString
                == "HTTP host — rendering routes baked in; call surfaces mounted from sub-agents.")
        #expect(r["running"].asBool == true, "\(r)")
        #expect(r["served_agent_count"].asInt == Int64(kernel.allAgents().count))
        #expect(r["routes"].asObject?.keys.first == "GET /<agent_id>/file/<path>", "\(r)")
        #expect(
            (WebBundle().verbDocTable.asObject ?? [:]).keys.map { String($0) }
                == ["reflect", "boot", "stop", "mount", "unmount", "fetch_file"])
    }

    /// The ws leg's reflect: `[id, sentence, path_pattern, <describe>,
    /// verbs]`, no `kind`.
    @Test func webWsReflectKeysMatchPython() async {
        let kernel = makeKernelWithAll()
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("web_ws.tools"), "id": .string("ws"),
            ]))
        let r = await call(kernel, "ws", .object(["type": .string("reflect")]))
        let keys = Array((r.asObject ?? [:]).keys.map { String($0) }.prefix(6))
        #expect(
            keys == [
                "id", "sentence", "path_pattern", "ingress_rule", "egress_rule", "sealed",
            ], "\(keys)")
        #expect(
            r["sentence"].asString
                == "WS verb-invocation surface; mounts /<host_id>/ws on the parent web.")
        #expect(r["path_pattern"].asString == "/{host_id}/ws")
        #expect(r["sealed"].asBool == true, "\(r)")
    }

    /// The REST leg's reflect: URL patterns, curl examples and this
    /// leg's auth posture; no `kind`.
    @Test func webRestReflectKeysMatchPython() async {
        let kernel = makeKernelWithAll()
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("web_rest.tools"), "id": .string("rest"),
            ]))
        let r = await call(kernel, "rest", .object(["type": .string("reflect")]))
        let keys = Array((r.asObject ?? [:]).keys.map { String($0) }.prefix(13))
        #expect(
            keys == [
                "id", "sentence", "path_pattern", "method", "reflect_url", "reflect_pattern",
                "auth_header", "curl_post", "curl_reflect", "curl_reflect_target",
                "ingress_rule", "egress_rule", "sealed",
            ], "\(keys)")
        #expect(r["path_pattern"].asString == "/rest/{target_id}")
        #expect(r["reflect_pattern"].asString == "/rest/_reflect/{target_id}")
        #expect(r["auth_header"].asString == "X-Fantastic-Auth")
        #expect(
            r["curl_post"].asString
                == "curl -X POST -H \"content-type: application/json\" "
                + "-d '{\"type\":\"reflect\"}' http://<host>/rest/<target_id>")
        #expect(
            r["curl_reflect"].asString
                == "curl http://<host>/rest/_reflect           # root reflect (tree)")
    }

    // MARK: - unknown verbs

    /// The legs answer python's coded `unhandled` refusal (carrying the
    /// verb); the host answers its own plain `{error}` dict. `shutdown` is not a
    /// verb on any of the three, and `boot` answers None on both legs.
    @Test func unknownVerbIsUnhandledRefusal() async throws {
        let kernel = makeKernelWithAll()
        for (id, module, bundle) in [
            ("ws", "web_ws.tools", "web_ws"), ("rest", "web_rest.tools", "web_rest"),
        ] {
            _ = await call(
                kernel, "kernel_state",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string(module), "id": .string(id),
                    "ingress_rule": .string("allow_all_ingress"),
                ]))
            for verb in ["nope", "shutdown"] {
                let r = await call(kernel, id, .object(["type": .string(verb)]))
                #expect(r["reason"].asString == "unhandled", "\(id)/\(verb) → \(r)")
                #expect(r["error"].asString == "\(bundle): unknown type '\(verb)'", "\(r)")
                #expect(r["verb"].asString == verb, "\(r)")
            }
            #expect(await call(kernel, id, .object(["type": .string("boot")])).isNull)
        }
        // The host's unknown verb is python's coded refusal.
        let webKernel = try await startWebKernel()
        defer { Task { _ = await call(webKernel, "web", .object(["type": .string("stop")])) } }
        let r = await call(webKernel, "web", .object(["type": .string("nope")]))
        #expect(r["error"].asString == "web: unknown type 'nope'", "\(r)")
        #expect(r["reason"].asString == "unhandled" && r["verb"].asString == "nope", "\(r)")
        // A null `type` never reaches a bundle through `send` (the batch
        // refuses it), so drive the repr straight at the handler.
        let none = try await WebBundle().handle(
            agentId: AgentId("web"), payload: .object(["type": .null]), kernel: webKernel)
        #expect(none?["error"].asString == "web: unknown type None", "\(String(describing: none))")
    }

    // MARK: - lifecycle

    /// Boot/stop/mount/unmount answer python's bodies; there are no
    /// `asset`/`render`/`shutdown` cases.
    @Test func webLifecycleRepliesMatchPython() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let boot = await call(kernel, "web", .object(["type": .string("boot")]))
        #expect(
            (boot.asObject ?? [:]).keys.map { String($0) } == ["running", "surfaces"], "\(boot)")
        #expect(boot["running"].asBool == true)
        #expect(boot["surfaces"].asArray?.isEmpty == true, "\(boot)")

        _ = await call(
            kernel, "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("web"),
                "handler_module": .string("web_rest.tools"), "id": .string("rest"),
            ]))
        wire(kernel, "web", "rest")
        let mounted = await call(
            kernel, "web", .object(["type": .string("mount"), "child_id": .string("rest")]))
        #expect(
            mounted["mounted"].asArray?.compactMap { $0.asString }
                == ["/rest/{target_id}", "/rest/_reflect", "/rest/_reflect/{target_id}"],
            "\(mounted)")
        let unmounted = await call(
            kernel, "web", .object(["type": .string("unmount"), "child_id": .string("rest")]))
        #expect(unmounted["unmounted"].asString == "rest", "\(unmounted)")

        let missing = await call(kernel, "web", .object(["type": .string("mount")]))
        #expect(missing["error"].asString == "web.mount: child_id required", "\(missing)")

        let stopped = await call(kernel, "web", .object(["type": .string("stop")]))
        #expect((stopped.asObject ?? [:]).keys.map { String($0) } == ["stopped"], "\(stopped)")
        #expect(stopped["stopped"].asBool == true)
        #expect(
            await call(kernel, "web", .object(["type": .string("stop")]))["stopped"].asBool
                == false)
        // Not running any more: mount/unmount say so.
        let cold = await call(
            kernel, "web", .object(["type": .string("unmount"), "child_id": .string("rest")]))
        #expect(cold["error"].asString == "web.unmount: not running", "\(cold)")
    }

    /// `rec.port` is REQUIRED with no default: a missing or zero port
    /// fails the verb RAISED, and the record never gains `running`.
    @Test func webBootWithoutPortRefuses() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let r = await call(kernel, "web", .object(["type": .string("boot")]))
        #expect(r["raised"].asBool == true, "\(r)")
        #expect(
            r["error"].asString
                == "RuntimeError: webapp web: rec.port is required (no default). "
                + "Set port on create_agent or via update_agent.", "\(r)")
    }

    /// python binds `int(rec.port)` as named: a port outside 0-65535 fails the
    /// bind (logged, the handle dropped) — never bound modulo 2^16.
    @Test func webBootWithAnOutOfRangePortBindsNothing() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("web"), "port": .integer(70000),
            ]))
        let r = await call(kernel, "web", .object(["type": .string("boot")]))
        #expect(r["running"].asBool == true, "python answers before the serve task fails: \(r)")
        #expect(kernel.httpPort() == 0, "bound \(kernel.httpPort()) — 70000 truncated to 16 bits")
        let refl = await call(kernel, "web", .object(["type": .string("reflect")]))
        #expect(refl["running"].asBool == false, "\(refl)")
        #expect(refl["port"].asInt == 70000, "\(refl)")
    }

    /// The listener never writes `port`/`running` back into the
    /// record (swift's `list_agents` showed `running:true`).
    @Test func webBootDoesNotWriteRunningIntoRecord() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        _ = await call(kernel, "web", .object(["type": .string("boot")]))
        let listed = await call(kernel, "kernel_state", .object(["type": .string("list_agents")]))
        let web = try #require(
            (listed["agents"].asArray ?? []).first { $0["id"].asString == "web" })
        #expect(web["running"].isNull, "\(web)")
        // The port is the one the record named — a bound port is not written back.
        #expect(web["port"].asInt == Int64(kernel.httpPort()), "\(web)")
    }

    /// Every child surface is mounted BEFORE the listener accepts, so
    /// no request can arrive at a half-built route table.
    @Test func webBootMountsBeforeListening() async throws {
        let kernel = try await startWebKernel()
        let port = kernel.httpPort()
        // The kernel auto-booted `web` at create time; take the listener down so
        // the ordering is observable on the next boot.
        _ = await call(kernel, "web", .object(["type": .string("stop")]))
        let probe = MountOrderProbe(port: port)
        kernel.bundles.register("mount_order_probe.tools", probe)
        _ = await call(
            kernel, "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("web"),
                "handler_module": .string("mount_order_probe.tools"), "id": .string("probe"),
            ]))
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        wire(kernel, "web", "probe")  // the host reads its child's routes
        let boot = await call(kernel, "web", .object(["type": .string("boot")]))
        #expect(boot["surfaces"].asArray?.compactMap { $0.asString } == ["probe"], "\(boot)")
        #expect(probe.sawListener() == false, "get_routes ran after the port was accepting")
    }

    /// Boot ANNOUNCES the bind by sending `say` to `cli`; the stderr
    /// line is the fallback for when no renderer answers.
    @Test func webBootAnnouncesSayToCli() async throws {
        let kernel = try await startWebKernel()
        let port = kernel.httpPort()
        _ = await call(kernel, "web", .object(["type": .string("stop")]))
        let probe = SayProbe()
        kernel.bundles.register("say_probe.tools", probe)
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("say_probe.tools"), "id": .string("cli"),
                "callable_by": .object([
                    "$self": .string("*"), "$parent": .string("*"), "$subtree": .string("*"),
                    "web": .string("*"),
                ]),
            ]))
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("web"),
                "can_call": .object(["cli": .string("*")]),
            ]))
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        _ = await call(kernel, "web", .object(["type": .string("boot")]))
        let said = try #require(probe.lines().last)
        #expect(said["type"].asString == "say", "\(said)")
        #expect(said["source"].asString == "web", "\(said)")
        #expect(
            said["text"].asString
                == "listening on http://127.0.0.1:\(port)/"
                + " · rest/ws are children (reflect kernel for ids)", "\(said)")
    }

    /// The announcement names the host the listener BOUND (`host` on
    /// the record), not a hardcoded `127.0.0.1`: a host bound elsewhere does not
    /// answer there. Twin of python `test_boot_announces_the_bound_host`.
    @Test func webBootAnnouncesTheBoundHost() async throws {
        let kernel = try await startWebKernel()
        let port = kernel.httpPort()
        _ = await call(kernel, "web", .object(["type": .string("stop")]))
        let probe = SayProbe()
        kernel.bundles.register("say_probe.tools", probe)
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("say_probe.tools"), "id": .string("cli"),
                "callable_by": .object([
                    "$self": .string("*"), "$parent": .string("*"), "$subtree": .string("*"),
                    "web": .string("*"),
                ]),
            ]))
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("web"),
                "can_call": .object(["cli": .string("*")]), "host": .string("localhost"),
            ]))
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        _ = await call(kernel, "web", .object(["type": .string("boot")]))
        let said = try #require(probe.lines().last)
        #expect(
            said["text"].asString
                == "listening on http://localhost:\(port)/"
                + " · rest/ws are children (reflect kernel for ids)", "\(said)")
    }

    // MARK: - browser_reachable

    /// python's `_browser_reachable` asks the REAL gates: both halves of the
    /// call gate AND the served agent's own io leg.
    @Test func browserReachableAsksBothGatesAndTheLeg() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let rel = "fantastic-reach-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("files"),
                "root": .string(rel),
            ]))
        // Sealed leg + no grants: not reachable.
        #expect(
            await call(kernel, "web", .object(["type": .string("reflect")]))["browser_reachable"]
                .asArray?.isEmpty == true)
        // Open the leg AND both halves of the call gate.
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("files"),
                "ingress_rule": .string("allow_all_ingress"),
                "callable_by": .object(["web": .string("*")]),
            ]))
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("web"),
                "can_call": .object(["files": .string("*")]),
            ]))
        let opened = await call(kernel, "web", .object(["type": .string("reflect")]))
        #expect(
            opened["browser_reachable"].asArray?.compactMap { $0.asString } == ["files"],
            "\(opened["browser_reachable"])")
        // Closing ANY one of the three takes it back out.
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("files"),
                "ingress_rule": .string("deny_ingress"),
            ]))
        #expect(
            await call(kernel, "web", .object(["type": .string("reflect")]))["browser_reachable"]
                .asArray?.isEmpty == true)
    }

    /// `browser_reachable` asks the REAL gate both ways: the web's `can_call`
    /// and the bridge's `callable_by` must both grant `read_stream`.
    @Test func browserReachableNeedsBothEnds() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let rel = "fantastic-reach-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("file_bridge.tools"), "allow": ["."],
                "id": .string("files"), "root": .string(rel),
                "ingress_rule": .string("allow_all_ingress"),
                "callable_by": .object(["web": ["read_stream"]]),
            ]))
        // Ingress + leg open, no egress yet: not reachable.
        #expect(
            await call(kernel, "web", .object(["type": .string("reflect")]))["browser_reachable"]
                .asArray?.isEmpty == true)
        kernel.update(AgentId("web"), ["can_call": ["$self": "*", "files": ["read_stream"]]])
        let opened = await call(kernel, "web", .object(["type": .string("reflect")]))
        #expect(
            opened["browser_reachable"].asArray?.compactMap { $0.asString } == ["files"],
            "\(opened["browser_reachable"])")
    }
}
