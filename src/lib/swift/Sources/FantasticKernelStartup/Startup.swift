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

import FantasticAppleKVS
import FantasticCliBundle
import FantasticFile
import FantasticFoundationModelsBackend
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticNvidiaNimBackend
import FantasticOllamaBackend
import FantasticProxyAgent
import FantasticScheduler
import FantasticTools

#if canImport(JavaScriptCore)
    import FantasticJSRuntime
#endif
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import FantasticYamlState
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
    r.register("file_bridge.tools", FileBundle())
    r.register("yaml_state.tools", YamlStateBundle())
    r.register("proxy_agent.tools", ProxyAgentBundle())
    r.register("tools.tools", ToolsBundle())
    r.register("scheduler.tools", SchedulerBundle())
    #if canImport(JavaScriptCore)
        // In-process JS executor (durable authored agents). Apple platforms
        // ship JavaScriptCore; mirrors the Rust js_runtime bundle.
        r.register("js_runtime.tools", JsRuntimeBundle())
    #endif
    // TWO io_bridge derivations sharing one engine (mirrors py's separate
    // ws_bridge + relay_connector bundles): ws/memory transports vs the relay-kernel router.
    r.register("ws_bridge.tools", KernelBridgeBundle(family: .ws))
    r.register("relay_connector.tools", KernelBridgeBundle(family: .relay))
    r.register("web.tools", WebBundle())
    r.register("web_ws.tools", WebWSBundle())
    r.register("web_rest.tools", WebRestBundle())
    r.register("ollama_backend.tools", OllamaBackendBundle())
    r.register("nvidia_nim_backend.tools", NvidiaNimBundle())
    r.register("foundation_models_backend.tools", FoundationModelsBackendBundle())
    // apple_kvs — synced KV (iCloud KVS). Apple-only: the bundle reports
    // unavailable on non-Apple, mirroring foundation_models_backend.
    r.register("apple_kvs.tools", AppleKVSBundle())
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
    // Mint an ephemeral guid into the root meta so reflect always carries one.
    // No-disk boot → not persisted, just present in RAM (persistence-scoped
    // field, but the in-memory kernel still needs a value to reflect). Mirrors
    // the disk path's mint; explicit is impossible here (virgin RAM root).
    let ephemeralGuid = UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
    let root = Agent(
        id: AgentId("core"),
        handlerModule: nil,
        parentId: nil,
        meta: ["guid": .string(ephemeralGuid)]
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

/// Boot a disk-backed kernel that hydrates from
/// `<workdir>/.fantastic/agents/<id>/agent.json` files written by
/// any Fantastic implementation (Python is canonical; Swift writes
/// the same shape via `Persistence.persist`).
///
/// Mirrors Python's daemon-mode startup contract: the kernel loads
/// whatever persisted state is on disk + weak-loads (skips agents
/// whose handler_module isn't installed in this runtime). No agents
/// are auto-created — composition is explicit, the operator decides
/// what to seed.
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
    guard fm.fileExists(atPath: url.path, isDirectory: &isDir), isDir.boolValue
    else {
        throw KernelStartupError.workdirInvalid(workdir)
    }
    let kernel = Kernel(
        storage: .disk(url),
        bundles: defaultBundleRegistry()
    )
    let dotFantastic = url.appendingPathComponent(".fantastic")
    try? fm.createDirectory(at: dotFantastic, withIntermediateDirectories: true)
    // Seed `.fantastic/readme.md` (the substrate doc) if missing — the
    // root has no handler_module, so per-bundle readme seeding skips it.
    // Mirrors Rust's `seed_root_readme` / Python's `Core._seed_root_readme`.
    RootReadme.seed(workdir: url)
    // Seed the keyed doc TREE `.fantastic/docs/<key>.md` too — `reflect
    // readme=index`/`readme=<key>` serves it back (mirrors Rust's DOCS seeding).
    RootDocs.seed(workdir: url)

    // COLD primitive: seed the root record `.fantastic/agent.json` on a virgin
    // dir. This is the chicken-egg bring-up — it runs BEFORE any agent (and thus
    // any file_bridge store provider) exists, so it cannot route through a
    // provider. Ongoing persistence (after boot) flows through the discovered
    // provider (see PersistenceProvider). Mirrors py write_record / rust
    // write_record_at, so a swift workdir handed to py/rust carries the root.
    let rootRecordFile = dotFantastic.appendingPathComponent("agent.json")
    // Determine the kernel's stable instance `guid`. Persistence-scoped (unlike
    // run-scoped runtime/env/version): read it back verbatim from the persisted
    // root record if present; otherwise MINT a 32-hex guid ONCE and persist it
    // through the SAME cold serializePretty primitive. Explicit/existing always
    // wins — a minted guid is never regenerated (next boot reads it back). Covers
    // BOTH the virgin-dir and already-exists-but-guidless cases. Mirrors the relay
    // guid idiom in KernelBridge.bootRelay + py write_record / rust.
    let rootGuid: String
    if fm.fileExists(atPath: rootRecordFile.path),
        let data = try? Data(contentsOf: rootRecordFile),
        let parsed = try? JSON.parse(data),
        case .object(var obj) = parsed
    {
        if let existing = obj["guid"]?.asString, !existing.isEmpty {
            rootGuid = existing
        } else {
            rootGuid = UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
            obj["guid"] = .string(rootGuid)
            try? JSON.object(obj).serializePretty(indent: 2).data(using: .utf8)?
                .write(to: rootRecordFile)
        }
    } else {
        rootGuid = UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
        let rec: JSON = .object([
            "id": .string("core"), "handler_module": .null, "parent_id": .null,
            "guid": .string(rootGuid),
        ])
        try? rec.serializePretty(indent: 2).data(using: .utf8)?.write(to: rootRecordFile)
    }

    // Register a bare `core` root agent carrying the persisted guid. If the
    // workdir has a persisted `core` record under agents/, `kernel.load()` below
    // will replace this one with the disk-backed shape (carrying meta etc.).
    let root = Agent(
        id: AgentId("core"),
        handlerModule: nil,
        parentId: nil,
        meta: ["guid": .string(rootGuid)],
        rootPath: dotFantastic
    )
    kernel.register(root)
    kernel.setRoot(root)

    // Hydrate persisted children. Reads every
    // <workdir>/.fantastic/agents/<id>/agent.json into an
    // AgentRecord, wraps them in a KernelState, and calls
    // `kernel.load(_:)`. `kernel.load` already implements weak-load
    // — agents whose handler_module isn't in the current bundle
    // registry are skipped silently. Same byte-shape guarantee
    // Python provides.
    let agentsDir = dotFantastic.appendingPathComponent("agents")
    let records = Persistence.readAllAgentRecords(from: agentsDir)
    if !records.isEmpty {
        // Ensure a root exists in the snapshot — `kernel.load`
        // requires exactly one parent_id == nil entry. If the
        // workdir lacks one (rare; happens if a user wipes core
        // but leaves children), reuse the in-memory `core` we
        // just registered.
        var fullRecords = records
        let hasRoot = records.contains { $0.parentId == nil }
        if !hasRoot {
            fullRecords.insert(
                AgentRecord(
                    id: "core", handlerModule: nil, parentId: nil, meta: [:]),
                at: 0)
        }
        let state = KernelState(agents: fullRecords)
        do {
            try kernel.load(state)
        } catch {
            FileHandle.standardError.write(
                "[kernel] hydration from \(agentsDir.path) failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            throw error
        }
    }

    // Ensure the in-memory root always carries the guid — hydration/load may
    // have swapped in a synthesized root with empty meta (rare agents/-has-root
    // path). Sourced from the persisted root record, never regenerated.
    if let r = kernel.root, r.metaValue(forKey: "guid")?.asString == nil {
        _ = r.updateMeta(["guid": .string(rootGuid)])
    }

    _ = portHint  // unused now; CLI / daemon mode passes port via
    // meta on the persisted `web` record. portHint was a vestige
    // of the old auto-create-web path.

    return kernel
}
