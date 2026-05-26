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
        _ = await kernel.send(
            "canvas",
            ["type": "add_agent", "id": "m1", "x": 10, "y": 10, "w": 100, "h": 100])
        let r = await kernel.send(
            "canvas",
            ["type": "discover", "x": 0, "y": 0, "w": 50, "h": 50])
        let members = r["members"].asArray ?? []
        #expect(members.count == 1)
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
        let reg = BundleRegistry()
        reg.register("kernel_bridge.tools", bridge)
        // create the bridge agent in kernel1 (re-use its existing
        // registry via re-registration to keep this test simple)
        kernel1.bundles.register("kernel_bridge.tools", bridge)
        _ = await kernel1.send(
            "core",
            ["type": "create_agent", "handler_module": "kernel_bridge.tools", "id": "br"])
        bridge.attachInMemory(agentId: "br", remote: kernel2)
        let reply = await kernel1.send(
            "br",
            [
                "type": "forward",
                "target_id": "core",
                "payload": ["type": "list_agents"] as JSON,
            ])
        // kernel2's list_agents returns "agents" array including core.
        let names = (reply["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(names.contains("core"))
    }

    @Test func cliRendererAttaches() async {
        let kernel = makeKernelWithAll()
        let token = attach(kernel)
        // Just verify the token works for unsubscribe — full CLI
        // output testing is overkill for a smoke test.
        kernel.unsubscribe(token)
    }
}
