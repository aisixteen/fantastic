// Smoke tests for every Phase 3 bundle.
//
// Each bundle gets a smoke test covering reflect + the most
// important verb. Detailed bundle-specific behavior tests live in
// per-bundle test targets in subsequent commits; this target proves
// every bundle compiles, registers, and answers its primary verb.

import FantasticCliBundle
import FantasticFile
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticKernelStartup
import FantasticProxyAgent
import FantasticScheduler
import FantasticTools
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation
import Testing

func makeKernelWithAll() -> Kernel {
    let registry = BundleRegistry()
    registry.register("file_bridge.tools", FileBundle())
    registry.register("proxy_agent.tools", ProxyAgentBundle())
    registry.register("tools.tools", ToolsBundle())
    registry.register("scheduler.tools", SchedulerBundle())
    registry.register("ws_bridge.tools", KernelBridgeBundle())
    registry.register("web_ws.tools", WebWSBundle())
    registry.register("web_rest.tools", WebRestBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

/// A disk-mode kernel rooted at `workdir/.fantastic`, with the full registry —
/// for the persistence-inversion test (records persist THROUGH a file_bridge).
func makeDiskKernel(workdir: URL) -> Kernel {
    let registry = BundleRegistry()
    registry.register("file_bridge.tools", FileBundle())
    registry.register("tools.tools", ToolsBundle())
    let kernel = Kernel(storage: .disk(workdir), bundles: registry)
    let root = Agent(
        id: "core", handlerModule: nil, parentId: nil,
        rootPath: workdir.appendingPathComponent(".fantastic"))
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

@Suite("Phase 3 bundle smoke tests")
struct BundleSmokeTests {
    @Test func persistInversionThroughDiscoveredStore() async throws {
        // The keystone: records persist THROUGH a discovered file_bridge
        // provider (py/rust parity), NOT a direct substrate write. No provider ⇒
        // RAM (no fallback); a wired store self-persists + carries the rest.
        let fm = FileManager.default
        let workdir = fm.temporaryDirectory.appendingPathComponent(
            "fantastic-inv-\(UUID().uuidString)")
        try fm.createDirectory(
            at: workdir.appendingPathComponent(".fantastic"), withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: workdir) }
        let kernel = makeDiskKernel(workdir: workdir)
        func file(_ rel: String) -> String {
            workdir.appendingPathComponent(".fantastic/\(rel)").path
        }

        // No store wired ⇒ the create stays in RAM (NO direct-fs fallback).
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "tools.tools", "id": "ram_only"])
        #expect(!fm.fileExists(atPath: file("agents/ram_only/agent.json")))
        #expect(kernel.agent("ram_only") != nil)  // live in RAM

        // Wire the store (file_bridge @ .fantastic, open) — it self-persists.
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "file_bridge.tools", "id": "store",
                "root": ".fantastic", "ingress_rule": "allow_all",
            ])
        #expect(
            fm.fileExists(atPath: file("agents/store/agent.json")),
            "the store must persist itself through itself")

        // Now a created agent persists THROUGH the provider.
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "tools.tools", "id": "foo",
                "display_name": "Foo",
            ])
        #expect(fm.fileExists(atPath: file("agents/foo/agent.json")))
        let content = try String(contentsOf: URL(fileURLWithPath: file("agents/foo/agent.json")))
        #expect(content.contains("\"foo\"") && content.contains("Foo"))

        // Delete removes the record THROUGH the provider's recursive delete.
        _ = await kernel.send("core", ["type": "delete_agent", "id": "foo"])
        #expect(!fm.fileExists(atPath: file("agents/foo")))
    }

    @Test func fileBridgeStreamsRoundTripRawBytes() async throws {
        // The "send+stream handle" — read_stream/write_stream carry RAW BYTES
        // over the symmetric binary channel (never base64), same as py/rust.
        let kernel = makeKernelWithAll()
        let fm = FileManager.default
        let rel = "fantastic-stream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "file_bridge.tools", "id": "fs",
                "root": .string(rel), "ingress_rule": "allow_all",
            ])
        // Non-UTF-8 bytes — proves the channel is raw, not text/base64.
        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // SINK.
        let (w, _) = await kernel.sendWithBinary(
            "fs",
            .object([
                "type": .string("write_stream"), "path": .string("blob.bin"),
                "truncate": .bool(true),
            ]), payload)
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // SOURCE.
        let (m, body) = await kernel.sendWithBinary(
            "fs",
            .object(["type": .string("read_stream"), "path": .string("blob.bin")]), Data())
        #expect(body == payload, "bytes must round-trip verbatim")
        #expect(m["eof"].asBool == true)
        #expect(m["size"].asInt == Int64(payload.count))
    }

    @Test func fileBundleReflect() async throws {
        let kernel = makeKernelWithAll()
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent(
            "fantastic-file-test-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmp) }
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "id": "fs1",
                "root": .string(tmp.path),
            ])
        let r = await kernel.send("fs1", ["type": "reflect"])
        #expect(r["kind"].asString == nil || r["sentence"].asString?.contains("Filesystem") == true)
    }

    @Test func proxyAgentNoHostReturnsStructuredError() async {
        clearHosts()
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "proxy_agent.tools", "id": "p1"])
        let r = await kernel.send("p1", ["type": "render", "x": 1])
        #expect(r["reason"].asString == "no_host")
    }

    @Test func proxyAgentReflectShowsHostRegisteredFalse() async {
        clearHosts()
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "proxy_agent.tools", "id": "p2"])
        let r = await kernel.send("p2", ["type": "reflect"])
        #expect(r["host_registered"].asBool == false)
        #expect(r["kind"].asString == "proxy_agent")
    }

    @Test func toolsRegisterAndDispatch() async {
        _ = FantasticTools.clear()
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "tools.tools", "id": "tools"])
        let r = await kernel.send(
            "tools",
            [
                "type": "register",
                "name": "ping",
                "agent_id": "health",
                "description": "Returns pong.",
                "parameters_schema": ["type": "object"] as JSON,
            ])
        #expect(r["ok"].asBool == true)
        let list = await kernel.send("tools", ["type": "list_for_llm"])
        let arr = list["tools"].asArray ?? []
        #expect(arr.count == 1)
        #expect(arr[0]["name"].asString == "ping")
    }

    @Test func toolsDispatchUnknownReturnsToolNotFound() async {
        _ = FantasticTools.clear()
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "tools.tools", "id": "tools"])
        let r = await kernel.send(
            "tools",
            ["type": "dispatch", "name": "nope", "arguments": [:] as JSON])
        #expect(r["reason"].asString == "tool_not_found")
    }

    @Test func schedulerScheduleListUnschedule() async throws {
        // py/rust contract: schedule {target, payload, interval_seconds} → mints a
        // schedule_id, persisted THROUGH file_bridge_id (store-relative
        // agents/<id>/schedules.json); schedule/boot FAILFAST until wired.
        let kernel = makeKernelWithAll()
        let fm = FileManager.default
        let rel = "fantastic-sched-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        // Open file_bridge store — the scheduler persists through it.
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "file_bridge.tools", "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all",
            ])
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "scheduler.tools", "id": "sched",
                "file_bridge_id": "store",
            ])
        let r = await kernel.send(
            "sched",
            [
                "type": "schedule", "target": "core", "interval_seconds": .integer(60),
                "payload": ["type": "list_agents"] as JSON,
            ])
        let sid = r["schedule_id"].asString
        #expect(sid != nil, "\(r)")
        // store-relative sidecar landed under the store root.
        #expect(
            fm.fileExists(atPath: dir.appendingPathComponent("agents/sched/schedules.json").path))
        let list = await kernel.send("sched", ["type": "list"])
        #expect((list["schedules"].asArray ?? []).count == 1)
        // FAILFAST: scheduling without a provider refuses (aligned error string).
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "scheduler.tools", "id": "bare"])
        let bare = await kernel.send(
            "bare", ["type": "schedule", "target": "core", "payload": ["type": "boot"] as JSON])
        #expect(bare["error"].asString == "scheduler: file_bridge_id required")
        // unschedule removes it.
        let u = await kernel.send(
            "sched", ["type": "unschedule", "schedule_id": .string(sid ?? "")])
        #expect(u["removed"].asBool == true)
        let after = await kernel.send("sched", ["type": "list"])
        #expect((after["schedules"].asArray ?? []).isEmpty)
    }

    @Test func schedulerTickNowFiresAndHistory() async throws {
        // The FIRE path: tick_now dispatches the schedule's payload to its
        // target, bumps run_count, appends a `schedule_fired` event to the
        // history.jsonl sidecar (through the provider), and `history` reads it
        // back. Mirrors the py/rust contract.
        let kernel = makeKernelWithAll()
        let fm = FileManager.default
        let rel = "fantastic-schedfire-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "file_bridge.tools", "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all",
            ])
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "scheduler.tools", "id": "sched",
                "file_bridge_id": "store",
            ])
        let r = await kernel.send(
            "sched",
            [
                "type": "schedule", "target": "core", "interval_seconds": .integer(3600),
                "payload": ["type": "list_agents"] as JSON,
            ])
        let sid = try #require(r["schedule_id"].asString)
        // Fire it NOW (no tick-loop wait).
        let fired = await kernel.send("sched", ["type": "tick_now", "schedule_id": .string(sid)])
        #expect(fired["fired"].asBool == true, "\(fired)")
        // run_count bumped + persisted.
        let list = await kernel.send("sched", ["type": "list"])
        #expect(list["schedules"][0]["run_count"].asInt == 1, "\(list)")
        // history returns the schedule_fired event with the dispatch RESULT
        // (list_agents reply) — proving the target was actually called.
        let h = await kernel.send("sched", ["type": "history", "schedule_id": .string(sid)])
        #expect((h["count"].asInt ?? 0) >= 1, "\(h)")
        let ev = h["history"][0]
        #expect(ev["type"].asString == "schedule_fired")
        #expect(ev["scheduler_id"].asString == "sched")
        #expect(ev["error"].isNull, "\(ev)")
        #expect(!ev["result"]["agents"].isNull, "fire must carry the target's reply: \(ev)")
        // The history sidecar landed store-relative, next to schedules.json.
        #expect(
            fm.fileExists(atPath: dir.appendingPathComponent("agents/sched/history.jsonl").path))
    }

    @Test func kernelBridgeForwardInMemory() async {
        let kernel1 = makeKernelWithAll()
        let kernel2 = makeKernelWithAll()
        // Wire bridge from kernel1 → kernel2.
        let bridge = KernelBridgeBundle()
        kernel1.bundles.register("ws_bridge.tools", bridge)
        _ = await kernel1.send(
            "core",
            ["type": "create_agent", "handler_module": "ws_bridge.tools", "id": "br"])
        bridge.attachInMemory(agentId: "br", remote: kernel2, localKernel: kernel1)
        let reply = await kernel1.send(
            "br",
            [
                "type": "forward",
                "target": "core",
                "payload": ["type": "list_agents"] as JSON,
            ])
        // kernel2's list_agents returns "agents" array including core.
        let names = (reply["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(names.contains("core"))
    }

    @Test func kernelBridgeBinaryForwardStreamsRawBytes() async throws {
        // Cross-kernel STREAMING — forward a write_stream/read_stream over the
        // (in-memory) bridge to a remote file_bridge; raw bytes both ways.
        let kernel1 = makeKernelWithAll()
        let kernel2 = makeKernelWithAll()
        let bridge = KernelBridgeBundle()
        kernel1.bundles.register("ws_bridge.tools", bridge)
        _ = await kernel1.send(
            "core", ["type": "create_agent", "handler_module": "ws_bridge.tools", "id": "br"])
        bridge.attachInMemory(agentId: "br", remote: kernel2, localKernel: kernel1)
        // kernel2: an OPEN file_bridge in a cwd-relative dir.
        let fm = FileManager.default
        let rel = "fantastic-brstream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel2.send(
            "core",
            [
                "type": "create_agent", "handler_module": "file_bridge.tools", "id": "fs",
                "root": .string(rel), "ingress_rule": "allow_all",
            ])
        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // forward write_stream (binary) → kernel2.fs
        let (w, _) = await kernel1.sendWithBinary(
            "br",
            .object([
                "type": .string("forward"), "target": .string("fs"),
                "payload": .object([
                    "type": .string("write_stream"), "path": .string("blob.bin"),
                    "truncate": .bool(true),
                ]),
            ]), payload)
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // forward read_stream (binary) → bytes back over the bridge
        let (_, body) = await kernel1.sendWithBinary(
            "br",
            .object([
                "type": .string("forward"), "target": .string("fs"),
                "payload": .object([
                    "type": .string("read_stream"), "path": .string("blob.bin"),
                ]),
            ]), Data())
        #expect(body == payload, "raw bytes must round-trip cross-kernel over the bridge")
    }

    @Test func wsBinaryForwardStreamsRawBytesOverWire() async throws {
        // The WIRE half of cross-kernel streaming — a bridge's
        // `WebSocketTransport.binaryForward` ships a codec binary frame over a
        // real loopback WS to a remote `web_ws`, which decodes it, gates, and
        // dispatches `sendWithBinary` to a `file_bridge`. Raw bytes both ways,
        // never base64. (The in-memory variant is
        // `kernelBridgeBinaryForwardStreamsRawBytes`.)
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(AgentId("web"), .object(["type": .string("boot")]))
        // web_ws SEALS by default — open it for the inbound binary call.
        let rec = await kernel.send(
            AgentId("web"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("web_ws.tools"),
                "id": .string("web_ws"),
                "ingress_rule": .string("allow_all"),
            ]))
        let wsId = rec["id"].asString ?? "web_ws"
        _ = await kernel.send(
            AgentId("web"),
            .object(["type": .string("mount"), "child_id": .string(wsId)]))
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
            }
        }
        // An OPEN file_bridge in a cwd-relative dir on the SAME kernel.
        let fm = FileManager.default
        let rel = "fantastic-wsbrstream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath)
            .appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("file_bridge.tools"),
                "id": .string("fs"), "root": .string(rel),
                "ingress_rule": .string("allow_all"),
            ]))

        let port = kernel.httpPort()
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let transport = WebSocketTransport(endpoint: url)
        await transport.connect()

        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // write_stream (binary) over the wire → kernel.fs
        let (w, _) = await transport.binaryForward(
            target: AgentId("fs"),
            header: .object([
                "type": .string("write_stream"), "path": .string("blob.bin"),
                "truncate": .bool(true),
            ]),
            blob: payload)
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // read_stream (binary) over the wire → raw bytes back
        let (_, body) = await transport.binaryForward(
            target: AgentId("fs"),
            header: .object([
                "type": .string("read_stream"), "path": .string("blob.bin"),
            ]),
            blob: Data())
        #expect(body == payload, "raw bytes must round-trip over the WS wire")
    }

    @Test func kernelBridgeWatchRemoteReEmitsOnLocalInbox() async {
        // Two kernels in process. A's bridge subscribes to B.core's
        // emits via watch_remote. We trigger an emit on B.core, then
        // verify the payload arrives on A's bridge inbox via a
        // synthetic watcher.
        let kernelA = makeKernelWithAll()
        let kernelB = makeKernelWithAll()
        let bridge = KernelBridgeBundle()
        kernelA.bundles.register("ws_bridge.tools", bridge)
        _ = await kernelA.send(
            "core",
            ["type": "create_agent", "handler_module": "ws_bridge.tools", "id": "br"])
        bridge.attachInMemory(agentId: "br", remote: kernelB, localKernel: kernelA)

        // Give attachInMemory's async setLocalSink Task a tick to land.
        try? await Task.sleep(nanoseconds: 50_000_000)

        // Subscribe via the bridge: A.br.watch_remote(target=core)
        let w = await kernelA.send(
            "br",
            ["type": "watch_remote", "target": "core"])
        #expect(w["ok"].asBool == true)

        // Synthetic local watcher of the bridge agent's inbox. Every
        // re-emitted event lands here.
        let watcherId: AgentId = "test_local_watcher"
        kernelA.watch(src: "br", watcher: watcherId)
        let inbox = kernelA.ensureInbox(watcherId)

        // Trigger an emit on B.core. The substrate's `emit` fans out
        // to watcher inboxes — our synthetic in-memory relay drains
        // the watcher inbox and re-emits on A.br, which fans out to
        // the local watcher.
        await kernelB.emit(
            "core",
            ["type": "token", "text": "hi"])

        // Wait for the event with a short timeout.
        let event = await withTaskGroup(of: JSON?.self) { group in
            group.addTask {
                for await ev in inbox {
                    if ev["type"].asString == "token" { return ev }
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                return nil
            }
            let first = await group.next() ?? nil
            group.cancelAll()
            return first
        }
        #expect(event != nil, "expected re-emitted token event on bridge inbox")
        #expect(event?["text"].asString == "hi")

        _ = await kernelA.send("br", ["type": "unwatch_remote", "target": "core"])
    }

    @Test func cliRendererAttaches() async {
        let kernel = makeKernelWithAll()
        let token = attach(kernel)
        // Just verify the token works for unsubscribe — full CLI
        // output testing is overkill for a smoke test.
        kernel.unsubscribe(token)
    }

    @Test func webWSGetRoutesShape() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "web_ws.tools", "id": "ws"])
        let r = await kernel.send("ws", ["type": "get_routes"])
        let routes = r["routes"].asArray ?? []
        #expect(routes.count == 1)
        #expect(routes.first?["kind"].asString == "websocket")
        #expect(routes.first?["path"].asString == "/{host_id}/ws")
    }

    @Test func webRestGetRoutesNamespacedBySelf() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "web_rest.tools", "id": "rest"])
        let r = await kernel.send("rest", ["type": "get_routes"])
        let paths = (r["routes"].asArray ?? []).compactMap { $0["path"].asString }
        #expect(paths.contains("/rest/{target}"))
        #expect(paths.contains("/rest/_reflect"))
        #expect(paths.contains("/rest/_reflect/{target}"))
    }

    @Test func webRestHandleRoutePostDispatchesBodyVerb() async {
        let kernel = makeKernelWithAll()
        // The web_rest leg SEALS by default — open it for the inbound POST.
        _ = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "web_rest.tools", "id": "rest",
                "ingress_rule": "allow_all",
            ])
        // Simulate the host calling handle_route for
        // POST /rest/core  body={type:list_agents}.
        let reply = await kernel.send(
            "rest",
            [
                "type": "handle_route",
                "method": "POST",
                "params": ["target": "core"] as JSON,
                "query": [:] as JSON,
                "body": "{\"type\":\"list_agents\"}",
            ])
        #expect(reply["status"].asInt == 200)
        #expect(reply["content_type"].asString == "application/json")
        let body = reply["body"].asString ?? ""
        #expect(body.contains("\"core\""))
    }

    @Test func webRestHandleRouteBadJsonIs400() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "web_rest.tools", "id": "rest"])
        let reply = await kernel.send(
            "rest",
            [
                "type": "handle_route",
                "method": "POST",
                "params": ["target": "core"] as JSON,
                "query": [:] as JSON,
                "body": "not json",
            ])
        #expect(reply["status"].asInt == 400)
    }
}
