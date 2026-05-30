// Smoke tests for every Phase 3 bundle.
//
// Each bundle gets a smoke test covering reflect + the most
// important verb. Detailed bundle-specific behavior tests live in
// per-bundle test targets in subsequent commits; this target proves
// every bundle compiles, registers, and answers its primary verb.

import FantasticAiChatWebapp
import FantasticCanvasBackend
import FantasticCanvasWebapp
import FantasticCliBundle
import FantasticFile
import FantasticGlAgent
import FantasticHtmlAgent
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticProxyAgent
import FantasticScheduler
import FantasticTelemetryPane
import FantasticTerminalWebapp
import FantasticTools
import FantasticWebRest
import FantasticWebWS
import Foundation
import Testing

func makeKernelWithAll() -> Kernel {
    let registry = BundleRegistry()
    registry.register("file.tools", FileBundle())
    registry.register("proxy_agent.tools", ProxyAgentBundle())
    registry.register("tools.tools", ToolsBundle())
    registry.register("html_agent.tools", HtmlAgentBundle())
    registry.register("gl_agent.tools", GlAgentBundle())
    registry.register("scheduler.tools", SchedulerBundle())
    registry.register("canvas_backend.tools", CanvasBackendBundle())
    registry.register("canvas_webapp.tools", CanvasWebappBundle())
    registry.register("ai_chat_webapp.tools", AiChatWebappBundle())
    registry.register("terminal_webapp.tools", TerminalWebappBundle())
    registry.register("telemetry_pane.tools", TelemetryPaneBundle())
    registry.register("kernel_bridge.tools", KernelBridgeBundle())
    registry.register("web_ws.tools", WebWSBundle())
    registry.register("web_rest.tools", WebRestBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

@Suite("Phase 3 bundle smoke tests")
struct BundleSmokeTests {
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
                "handler_module": "file.tools",
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

    @Test func htmlAgentSetAndRender() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "html_agent.tools", "id": "h1"])
        _ = await kernel.send(
            "h1", ["type": "set_html", "html": "<h1>Hello</h1>"])
        let r = await kernel.send("h1", ["type": "render_html"])
        #expect(r["html"].asString == "<h1>Hello</h1>")
    }

    @Test func glAgentSourceRoundTrip() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "gl_agent.tools", "id": "gl1"])
        _ = await kernel.send(
            "gl1", ["type": "set_source", "gl_source": "void main(){}"])
        let r = await kernel.send("gl1", ["type": "reflect"])
        #expect(r["gl_source"].asString == "void main(){}")
    }

    @Test func canvasBackendAddAndDiscover() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "canvas_backend.tools", "id": "canvas"])
        // add_agent requires a handler_module (canonical contract) and
        // the member must answer a render verb. html_agent answers
        // get_webapp, so it survives the render-probe. Geometry lives
        // on width/height keys (matches Python canvas_backend).
        let add = await kernel.send(
            "canvas",
            [
                "type": "add_agent",
                "handler_module": "html_agent.tools",
                "x": 10, "y": 10, "width": 100, "height": 100,
            ])
        #expect(add["ok"].asBool == true, "add_agent failed: \(add)")

        // Canonical discover returns {agents:[{id,x,y,width,height}]}.
        let r = await kernel.send(
            "canvas",
            ["type": "discover", "x": 0, "y": 0, "w": 50, "h": 50])
        let agents = r["agents"].asArray ?? []
        #expect(agents.count == 1, "discover returned: \(r)")
    }

    @Test func canvasWebappServesHtml() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "canvas_webapp.tools", "id": "cv"])
        let r = await kernel.send("cv", ["type": "render_html"])
        let html = r["html"].asString ?? ""
        #expect(html.contains("<") && html.count > 50)
    }

    @Test func terminalWebappServesHtml() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "terminal_webapp.tools", "id": "tw"])
        let r = await kernel.send("tw", ["type": "render_html"])
        let html = r["html"].asString ?? ""
        #expect(html.contains("<") && html.count > 50)
    }

    @Test func aiChatWebappReflect() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "ai_chat_webapp.tools", "id": "chat"])
        let r = await kernel.send("chat", ["type": "reflect"])
        #expect(r["kind"].asString == "ai_chat_webapp")
    }

    @Test func telemetryPaneReflect() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "telemetry_pane.tools", "id": "tp"])
        let r = await kernel.send("tp", ["type": "reflect"])
        #expect(r["kind"].asString == "telemetry_pane")
    }

    @Test func schedulerCanScheduleAndCancel() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "scheduler.tools", "id": "sched"])
        let r = await kernel.send(
            "sched",
            [
                "type": "schedule",
                "name": "tick",
                "interval_ms": 60_000,
                "target": "core",
                "payload": ["type": "boot"] as JSON,
            ])
        #expect(r["ok"].asBool == true)
        let c = await kernel.send("sched", ["type": "cancel", "name": "tick"])
        #expect(c["cancelled"].asBool == true)
    }

    @Test func kernelBridgeForwardInMemory() async {
        let kernel1 = makeKernelWithAll()
        let kernel2 = makeKernelWithAll()
        // Wire bridge from kernel1 → kernel2.
        let bridge = KernelBridgeBundle()
        kernel1.bundles.register("kernel_bridge.tools", bridge)
        _ = await kernel1.send(
            "core",
            ["type": "create_agent", "handler_module": "kernel_bridge.tools", "id": "br"])
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

    @Test func kernelBridgeWatchRemoteReEmitsOnLocalInbox() async {
        // Two kernels in process. A's bridge subscribes to B.core's
        // emits via watch_remote. We trigger an emit on B.core, then
        // verify the payload arrives on A's bridge inbox via a
        // synthetic watcher.
        let kernelA = makeKernelWithAll()
        let kernelB = makeKernelWithAll()
        let bridge = KernelBridgeBundle()
        kernelA.bundles.register("kernel_bridge.tools", bridge)
        _ = await kernelA.send(
            "core",
            ["type": "create_agent", "handler_module": "kernel_bridge.tools", "id": "br"])
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
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "web_rest.tools", "id": "rest"])
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
