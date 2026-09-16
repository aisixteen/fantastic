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
//   await kernel.shutdown()

import FantasticCliBundle
import FantasticFile
import FantasticFoundationModelsBackend
import FantasticJSON
import OrderedCollections
import FantasticKernel
import FantasticScenarioProbes
import FantasticKernelBridge
import FantasticAnthropicBackend
import FantasticLlamaCppBackend
import FantasticNvidiaNimBackend
import FantasticOllamaBackend
import FantasticScheduler
import FantasticStaticLoader

import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import FantasticTextFile
import Foundation

#if os(macOS)
    import FantasticExecBridge
    import FantasticTerminalBackend
#endif

/// Build the default bundle registry — every bundle that ships in
/// the Apple-platform Swift kernel. Mirrors the Rust CLI's
/// `register_default_bundles` exactly.
public func defaultBundleRegistry() -> BundleRegistry {
    let r = BundleRegistry()
    r.register("file_bridge.tools", FileBundle())
    // TEST-ONLY. The shared e2e scenarios declare stateful fixture roles that
    // python and js register in process; a compiled kernel can only run what is
    // compiled in, so without these the scenarios naming them are SKIPPED.
    #if SCENARIO_PROBES
        r.register(DISPENSER_MODULE, DispenserProbe())
        r.register(PLANT_MODULE, PlantProbe())
        r.register(PURCHASE_MODULE, PurchaseProbe())
        // THE HUMAN, as an agent. Every AI probe tasks its model through this
        // one, so the task is a gated `user -> ai` send with a ledger row
        // instead of an operator injection from outside the kernel.
        r.register(USER_MODULE, UserProbe())
        // A fixed body of knowledge per solver: `ask` is keyword search over
        // the `lines` on its record; `asked` is the tally the scorer reads.
        r.register(ORACLE_MODULE, OracleProbe())
    #endif
    r.register("text_file.tools", TextFileBundle())
    // The render sink the AI backends stream their turn to (python `cli`).
    r.register(CliBundle.handlerModule, CliBundle())
    r.register("scheduler.tools", SchedulerBundle())
    r.register(StaticLoaderBundle.handlerModule, StaticLoaderBundle())
    // TWO io_bridge derivations sharing one engine (mirrors py's separate
    // ws_bridge + relay_connector bundles): ws/memory transports vs the relay-kernel router.
    r.register("ws_bridge.tools", KernelBridgeBundle(family: .ws))
    r.register("relay_connector.tools", KernelBridgeBundle(family: .relay))
    r.register("web.tools", WebBundle())
    r.register("web_ws.tools", WebWSBundle())
    r.register("web_rest.tools", WebRestBundle())
    r.register("ollama_backend.tools", OllamaBackendBundle())
    r.register("nvidia_nim_backend.tools", NvidiaNimBundle())
    r.register("anthropic_backend.tools", AnthropicBundle())
    r.register("llamacpp_backend.tools", LlamaCppBundle())
    r.register("foundation_models_backend.tools", FoundationModelsBackendBundle())
    #if os(macOS)
        r.register("exec_bridge.tools", ExecBridgeBundle())
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
    _ = await kernel.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("web.tools"),
            "id": .string("web"),
            "port": .integer(Int64(portHint)),
        ]))])[0].result

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
    // EVERYTHING ELSE the root record carries — `callable_by`, `can_call`, and any
    // operator-authored field. This used to read the file for its `guid` ALONE and
    // drop the rest, so a root whose record granted `callable_by: {rest: "*"}` came
    // up with only the three `$` baseline tokens: the grant was on disk, ignored in
    // memory, and every non-kernel verb from that agent was refused at ingress.
    // Measured 2026-08-29 against the seeded e2e workdir — `reflect` on the root was
    // refused on swift and allowed on rust from a byte-identical record, which hid
    // swift's entire root identity block (`self_extension` included) behind a
    // refusal. The comment below is why it went unnoticed: `kernel.load` replaces
    // the root only from a record under `agents/`, and the ROOT's record is not
    // there — it is `.fantastic/agent.json`.
    var rootMeta: OrderedDictionary<String, JSON> = [:]
    if fm.fileExists(atPath: rootRecordFile.path),
        let data = try? Data(contentsOf: rootRecordFile),
        let parsed = try? JSON.parse(data),
        case .object(let obj0) = parsed
    {
        // Structural fields are the record's identity, not meta.
        for (k, v) in obj0 where !["id", "handler_module", "parent_id", "guid"].contains(k) {
            rootMeta[k] = v
        }
    }
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
    rootMeta["guid"] = .string(rootGuid)
    let root = Agent(
        id: AgentId("core"),
        handlerModule: nil,
        parentId: nil,
        meta: rootMeta,
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
        // requires exactly one parent_id == nil entry. The root's record is
        // NOT under `agents/` (it is `.fantastic/agent.json`), so this branch
        // is the NORMAL path, not the rare one: every boot with any child
        // rebuilds the root from here.
        //
        // It must carry `rootMeta`. With `meta: [:]` the load REPLACED the
        // hydrated root with a blank one, so the grants read off
        // `.fantastic/agent.json` moments earlier were discarded and the root
        // came back with only its `$` baseline tokens — a `callable_by` an
        // operator had written was silently inert.
        var fullRecords = records
        let hasRoot = records.contains { $0.parentId == nil }
        if !hasRoot {
            fullRecords.insert(
                AgentRecord(
                    id: "core", handlerModule: nil, parentId: nil, meta: rootMeta),
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

    // NOTE: `portHint` is unused in disk mode — CLI / daemon mode passes the
    // port via meta on the persisted `web` record (a vestige of the old
    // auto-create-web path, kept in the signature for the daemon entrypoint).

    return kernel
}
