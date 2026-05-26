// Public startup entry points — `startKernel` + `startKernelInMemory`.
//
// Mirrors the Rust UniFFI free functions of the same names. Lives
// in its own target (FantasticKernelStartup) because it composes
// the entire default bundle set — every bundle target gets imported
// here. Core `FantasticKernel` stays bundle-agnostic.
//
// App-facing usage:
//   let kernel = try await startKernelInMemory(portHint: 0)
//   let port = kernel.httpPort()
//   // ... use kernel
//   kernel.shutdown()

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
import FantasticNvidiaNimBackend
import FantasticOllamaBackend
import FantasticProxyAgent
import FantasticScheduler
import FantasticTelemetryPane
import FantasticTerminalWebapp
import FantasticTools
import FantasticWeb
import Foundation

#if os(macOS)
    import FantasticLocalRunner
    import FantasticPythonRuntime
    import FantasticSshRunner
    import FantasticTerminalBackend
#endif

/// Build the default bundle registry — every bundle that ships in
/// the Apple-platform Swift kernel. Mirrors the Rust CLI's
/// `register_default_bundles` exactly.
public func defaultBundleRegistry() -> BundleRegistry {
    let r = BundleRegistry()
    r.register("file.tools", FileBundle())
    r.register("proxy_agent.tools", ProxyAgentBundle())
    r.register("tools.tools", ToolsBundle())
    r.register("html_agent.tools", HtmlAgentBundle())
    r.register("gl_agent.tools", GlAgentBundle())
    r.register("scheduler.tools", SchedulerBundle())
    r.register("canvas_backend.tools", CanvasBackendBundle())
    r.register("canvas_webapp.tools", CanvasWebappBundle())
    r.register("ai_chat_webapp.tools", AiChatWebappBundle())
    r.register("terminal_webapp.tools", TerminalWebappBundle())
    r.register("telemetry_pane.tools", TelemetryPaneBundle())
    r.register("kernel_bridge.tools", KernelBridgeBundle())
    r.register("web.tools", WebBundle())
    r.register("ollama_backend.tools", OllamaBackendBundle())
    r.register("nvidia_nim_backend.tools", NvidiaNimBundle())
    #if os(macOS)
        r.register("local_runner.tools", LocalRunnerBundle())
        r.register("python_runtime.tools", PythonRuntimeBundle())
        r.register("ssh_runner.tools", SshRunnerBundle())
        r.register("terminal_backend.tools", TerminalBackendBundle())
    #endif
    return r
}

/// Boot an in-memory kernel with the default bundle set. No disk
/// persistence; the kernel dies cleanly when its reference is
/// dropped. Used by the Apple app for the brain kernel (which lives
/// alongside the app process, not in a workdir).
///
/// `portHint` is the preferred HTTP port the listener should bind
/// to (8B+). 0 means "any free port"; the actual port is available
/// via `kernel.httpPort()` after boot. Phase 8A returns a kernel
/// whose `httpPort()` is 0 — the live listener wires in with 8B.
public func startKernelInMemory(portHint: UInt16 = 0) async throws -> Kernel {
    let kernel = Kernel(
        storage: .inMemory,
        bundles: defaultBundleRegistry()
    )
    let root = Agent(
        id: AgentId("core"),
        handlerModule: nil,
        parentId: nil
    )
    kernel.register(root)
    kernel.setRoot(root)

    // Create the web agent so phase 8B's listener has a home + the
    // app can immediately call `kernel.send("web", {boot})` to
    // start listening. Port hint stashes into the agent's meta so
    // 8B picks it up.
    _ = await kernel.send(
        AgentId("core"),
        .object([
            "type": .string("create_agent"),
            "handler_module": .string("web.tools"),
            "id": .string("web"),
            "port": .integer(Int64(portHint)),
        ])
    )

    return kernel
}

/// Boot a disk-backed kernel that hydrates from `<workdir>/.fantastic`.
/// Phase 8H wires the workdir bootstrap + flock; until then, this
/// behaves as in-memory mode rooted at the workdir path.
///
/// Throws `KernelStartupError.workdirInvalid` if the workdir doesn't
/// exist or isn't readable.
public func startKernel(
    workdir: String,
    portHint: UInt16 = 0
) async throws -> Kernel {
    let url = URL(fileURLWithPath: workdir, isDirectory: true)
    let fm = FileManager.default
    var isDir: ObjCBool = false
    guard fm.fileExists(atPath: url.path, isDirectory: &isDir), isDir.boolValue else {
        throw KernelStartupError.workdirInvalid(workdir)
    }
    let kernel = Kernel(
        storage: .disk(url),
        bundles: defaultBundleRegistry()
    )
    let dotFantastic = url.appendingPathComponent(".fantastic")
    try? fm.createDirectory(at: dotFantastic, withIntermediateDirectories: true)
    let root = Agent(
        id: AgentId("core"),
        handlerModule: nil,
        parentId: nil,
        rootPath: dotFantastic
    )
    kernel.register(root)
    kernel.setRoot(root)

    // 8H will hydrate persisted children here. Today the kernel
    // boots empty alongside the workdir.

    _ = await kernel.send(
        AgentId("core"),
        .object([
            "type": .string("create_agent"),
            "handler_module": .string("web.tools"),
            "id": .string("web"),
            "port": .integer(Int64(portHint)),
        ])
    )

    return kernel
}
