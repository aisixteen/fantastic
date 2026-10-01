// Public startup entry points — `startKernel` + `startKernelInMemory`.
//
// Lives in its own target (FantasticKernelStartup) because it composes
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
import FantasticJSON
import OrderedCollections
import FantasticKernel
import FantasticKernelState
import FantasticScenarioProbes
import FantasticKernelBridge
import FantasticAnthropicBackend
import FantasticResponsesBackend
import FantasticLlamaCppBackend
import FantasticVllmBackend
import FantasticNvidiaNimBackend
import FantasticOllamaBackend
import FantasticRamStore
import FantasticScheduler
import FantasticStaticLoader

import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation

#if os(macOS)
    import FantasticExecBridge
#endif

/// Build the default bundle registry — every bundle that ships in
/// the Apple-platform Swift kernel. Mirrors the Rust CLI's
/// `register_default_bundles` exactly.
public func defaultBundleRegistry() -> BundleRegistry {
    let r = BundleRegistry()
    // THE ROOT LOADER. `kernel_state` IS the tree root, and its handler_module is
    // this bundle — so it is registered first and every embedder gets it (there
    // is no kernel without a root).
    r.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    r.register("file_bridge.tools", FileBundle())
    // TEST-ONLY. The shared e2e scenarios declare stateful fixture roles that
    // python and js register in process; a compiled kernel can only run what is
    // compiled in, so without these the scenarios naming them are SKIPPED.
    // UNLISTED: python registers them as importable modules with no
    // `fantastic.bundles` entry point, so they dispatch but never show up in
    // the `bundles` catalog a model reflects.
    #if SCENARIO_PROBES
        r.registerUnlisted(DISPENSER_MODULE, DispenserProbe())
        r.registerUnlisted(PLANT_MODULE, PlantProbe())
        r.registerUnlisted(PURCHASE_MODULE, PurchaseProbe())
        // THE HUMAN, as an agent. Every AI probe tasks its model through this
        // one, so the task is a gated `user -> ai` send with a ledger row
        // instead of an operator injection from outside the kernel.
        r.registerUnlisted(USER_MODULE, UserProbe())
        // A fixed body of knowledge per solver: `ask` is keyword search over
        // the `lines` on its record; `asked` is the tally the scorer reads.
        r.registerUnlisted(ORACLE_MODULE, OracleProbe())
        // In-flight, failing and timed work: what a timeout, a stop or a
        // reboot lands on.
        r.registerUnlisted(SLOW_MODULE, SlowProbe())
    #endif
    // THE MEMORY STORE in RAM — what the root's `memory_store_id` and every AI
    // agent's name (python `memory/ram_store`).
    r.register(RamStoreBundle.handlerModule, RamStoreBundle())
    // The render sink the AI backends stream their output to (python `cli`).
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
    r.register("responses_backend.tools", ResponsesBundle())
    r.register("llamacpp_backend.tools", LlamaCppBundle())
    r.register("vllm_backend.tools", VllmBundle())
    #if os(macOS)
        r.register("exec_bridge.tools", ExecBridgeBundle())
    #endif
    return r
}

/// Boot an in-memory kernel with the default bundle set. No disk
/// persistence; the kernel dies cleanly when its reference is
/// dropped. Used by the Apple app for the brain kernel (which lives
/// alongside the app process, not in a workdir).
///
/// `portHint` is the preferred HTTP port the listener should bind
/// to; 0 means "any free port". The bound port is `kernel.httpPort()`
/// after boot.
public func startKernelInMemory(portHint: UInt16 = 0) async throws -> Kernel {
    let kernel = Kernel(
        storage: .inMemory,
        bundles: defaultBundleRegistry()
    )
    // NO GUID HERE. The instance guid is minted ONCE by the root loader's `boot`
    // (python `kernel_state._boot`), so an in-memory kernel gets one the same way
    // a disk one does — and never two different ones.
    // THE RUNNING DIR, NAMED: an in-memory kernel runs in the process cwd, and its
    // root's address says so — `Kernel.workdir` (the one clamp base) is the dir
    // holding it. Nothing is written there.
    let root = Agent(
        id: AgentId(ROOT_ID),
        handlerModule: ROOT_HANDLER_MODULE,
        parentId: nil,
        rootPath: URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".fantastic")
    )
    kernel.register(root)
    kernel.setRoot(root)

    // Create the web agent: `create_agent` fires its `boot`, which binds the
    // listener on the port hint stashed in its meta.
    _ = await kernel.send([Item(AgentId("kernel"), .object([
            "type": .string("create_agent"),
            "parent_id": .string(ROOT_ID),
            "handler_module": .string("web.tools"),
            "id": .string("web"),
            "port": .integer(Int64(portHint)),
        ]))])[0].result

    return kernel
}

/// Boot a disk-backed kernel that hydrates from
/// `<workdir>/.fantastic/agents/<id>/agent.yaml` files written by
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
    // THE ROOT RECORD, or a fresh seed. Python's `read_tree` returns `[]` when
    // `.fantastic/agent.yaml` is MISSING **or not a YAML mapping**, and `_bootstrap` then
    // seeds a fresh root, overwrites the file, and hydrates NO CHILDREN on that
    // boot — a tree whose root is unreadable has no walk to start from. A corrupt
    // root used to be overwritten here while the children were hydrated anyway.
    let rootRecordFile = dotFantastic.appendingPathComponent(RECORD_FILE)
    var rootId = ROOT_ID
    var rootHandler: String? = ROOT_HANDLER_MODULE
    var rootMeta: OrderedDictionary<String, JSON> = [:]
    var seeded = true
    if let obj = Persistence.readRecord(rootRecordFile) {
        seeded = false
        // A PERSISTED RECORD IS AUTHORITATIVE, id and handler_module included —
        // the tree on disk says what its root is called and what answers for it.
        if let id = obj["id"]?.asString, !id.isEmpty { rootId = id }
        rootHandler = obj["handler_module"]?.asString
        // EVERYTHING ELSE the root record carries — `callable_by`, `can_call`,
        // `guid`, and any operator-authored field. This used to read the file for
        // its `guid` ALONE and drop the rest, so a root whose record granted
        // `callable_by: {rest: "*"}` came up with only the three `$` baseline
        // tokens: the grant was on disk, ignored in memory, and every non-kernel
        // verb from that agent was refused at ingress (measured 2026-08-29).
        for (k, v) in obj where !["id", "handler_module", "parent_id"].contains(k) {
            rootMeta[k] = v
        }
    }

    let root = Agent(
        id: AgentId(rootId),
        handlerModule: rootHandler,
        parentId: nil,
        meta: rootMeta,
        rootPath: dotFantastic
    )
    kernel.register(root)
    kernel.setRoot(root)

    // THE VIRGIN SEED IS THE CONSTRUCTED ROOT, not a bare literal: the record on
    // disk carries the structural grant baseline `Agent.init` stamps, in its
    // order (`can_call, callable_by`), and the loader's `boot` appends `guid`
    // after them. COLD primitive — it runs before any provider exists.
    if seeded {
        try? Persistence.recordBytes(root.record().ordered)
            .write(to: rootRecordFile, options: .atomic)
        // ONLY ON THE SEED, as python does: `_bootstrap` calls `write_record`
        // (which is what seeds `readme.md` + the keyed `docs/` tree) under `if
        // seeded`. Seeding on EVERY invocation put back a doc the operator had
        // deleted — measured 2026-09-20: `docs/memory.md` and `docs/frontend/`
        // removed by hand came back on the next `reflect`, where python leaves
        // the tree as the operator left it. Copy-if-missing per file still
        // holds, so an edited doc is never clobbered.
        RootReadme.seed(workdir: url)
        RootDocs.seed(workdir: url)
    }

    // Hydrate persisted children. Reads every
    // <workdir>/.fantastic/agents/<id>/agent.yaml into an AgentRecord, wraps them
    // in a KernelState, and calls `kernel.load(_:)`, which already implements
    // weak-load — agents whose handler_module isn't in the current bundle
    // registry are skipped silently. A RESEEDED ROOT KEEPS ITS CHILDREN (python
    // `read_tree`, 2026-09-20): the walk runs whether or not the root parsed.
    var records = Persistence.readTree(rootDir: dotFantastic, fallbackRootId: rootId)
    if !records.isEmpty {
        // `readTree` yields the root record first when `.fantastic/agent.yaml`
        // parsed; a reseeded root is not on disk as a record it yields, so the
        // one `parent_id == nil` entry `kernel.load` requires is prepended
        // (python `main.py`, 2026-09-20).
        if !records.contains(where: { $0.parentId == nil }) {
            records.insert(
                AgentRecord(id: rootId, handlerModule: rootHandler, parentId: nil, meta: [:]),
                at: 0)
        }
        let state = KernelState(records: records)
        do {
            try kernel.load(state)
        } catch {
            FileHandle.standardError.write(
                "[kernel] hydration from \(dotFantastic.path) failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            throw error
        }
    }

    // NOTE: `portHint` is unused in disk mode — CLI / daemon mode passes the
    // port via meta on the persisted `web` record (a vestige of the old
    // auto-create-web path, kept in the signature for the daemon entrypoint).

    return kernel
}
