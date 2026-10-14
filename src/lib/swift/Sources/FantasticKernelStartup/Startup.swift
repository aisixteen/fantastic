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
import FantasticResponsesBackend
import FantasticAnthropicBackend
import FantasticMcpClient
import FantasticVllmBackend
import FantasticOllamaBackend
import FantasticFMCore
import FantasticFoundationModelsBackend
import FantasticCredentialStore
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
    // UNLISTED: python registers them with `register_unlisted` (no
    // `fantastic.bundles` entry point), so they dispatch but never show up in
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
        // The `docs_mechanics` vault: opens with a code only its detailed docs teach.
        r.registerUnlisted(VAULT_MODULE, VaultProbe())
    #endif
    // THE MEMORY STORE in RAM — what the root's `memory_store_id` and every AI
    // agent's name (python `memory/ram_store`).
    r.register(RamStoreBundle.handlerModule, RamStoreBundle())
    // SECRETS BY REF (python `security/credential_store`) — where every AI
    // backend's key lives: its record names `credentials: {store, ref}`.
    r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle())
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
    r.register("responses_backend.tools", ResponsesBundle())
    r.register("vllm_backend.tools", VllmBundle())
    r.register("anthropic_backend.tools", AnthropicBundle())
    // a remote MCP server (protocol 2026-07-28) as an agent: its tools are the
    // agent's verbs
    r.register(MCP_CLIENT_HANDLER_MODULE, McpClientBundle())
    // Apple's on-device model (FantasticFMCore — the same code the rust kernel
    // links through FantasticFMBridge). Registered on every platform: where the
    // model cannot run, a send fails naming why.
    r.register("foundation_models_backend.tools", FoundationModelsBackendBundle(engine: SystemFMEngine()))
    #if os(macOS)
        r.register("exec_bridge.tools", ExecBridgeBundle())
        // the base a folder bundle (`<root>/bundles/<name>/`, `base: py_agent`)
        // extends — it runs the folder's program
        r.register(PY_AGENT_HANDLER_MODULE, PyAgentBundle())
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
/// the same shape via `Persistence.recordBytes`).
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
    // THROWS, as python's seed raises: a `.fantastic` that cannot be made left a
    // kernel booting "fine" with nothing persisted (audit 2026-10-03, ARCH2-9).
    try fm.createDirectory(at: dotFantastic, withIntermediateDirectories: true)
    // THE ROOT RECORD, or a fresh seed. When `.fantastic/agent.yaml` is MISSING
    // **or not a YAML mapping**, python's `_bootstrap` seeds a fresh root and
    // overwrites the file — and its `read_tree` still walks the children under the
    // default `agents/` layout, parented to the id the reseed mints, so the
    // reseeded root keeps its children (see the hydration below).
    // CLAMPED, as python's `read_tree` reads it through `fs`: a root `agent.yaml`
    // linked outside `.fantastic` throws `fs: path 'agent.yaml' escapes root`
    // HERE, before the seed below could write over the link (audit ARCH2-6).
    let rootRecordFile = dotFantastic.appendingPathComponent(RECORD_FILE)
    var rootId = ROOT_ID
    var rootHandler: String? = ROOT_HANDLER_MODULE
    var rootMeta: OrderedDictionary<String, JSON> = [:]
    var seeded = true
    if let obj = try Persistence.readRecordClamped(dotFantastic, RECORD_FILE) {
        seeded = false
        // A PERSISTED RECORD IS AUTHORITATIVE, id and handler_module included —
        // the tree on disk says what its root is called and what answers for it.
        if let id = obj["id"]?.asString, !id.isEmpty { rootId = id }
        rootHandler = obj["handler_module"]?.asString
        // EVERYTHING ELSE the root record carries — `callable_by`, `can_call`,
        // `guid`, and any operator-authored field. This used to read the file for
        // its `guid` ALONE and drop the rest, so a root whose record granted
        // `callable_by: {rest: "*"}` came up with only the `$self` baseline: the
        // grant was on disk, ignored in memory, and every non-kernel
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
    // disk carries the grant baseline `Agent.init` stamps, in its
    // order (`can_call, callable_by`), and the loader's `boot` appends `guid`
    // after them. COLD primitive — it runs before any provider exists.
    if seeded {
        // THROWS, as python's `write_record` raises: a root record that never
        // landed minted a new guid every boot, breaking peers pinned to it.
        try Persistence.recordBytes(root.record().ordered)
            .write(to: rootRecordFile, options: .atomic)
    }

    // Hydrate persisted children. Reads every
    // <workdir>/.fantastic/agents/<id>/agent.yaml into an AgentRecord, wraps them
    // in a KernelState, and calls `kernel.load(_:)`, which already implements
    // weak-load — agents whose handler_module isn't in the current bundle
    // registry are skipped silently. A RESEEDED ROOT KEEPS ITS CHILDREN (python
    // `read_tree`, 2026-09-20): the walk runs whether or not the root parsed.
    // THROWS where python's clamped walk raises: a `children_dir` or a record
    // that resolves outside `.fantastic` refuses the boot.
    var records = try Persistence.readTree(rootDir: dotFantastic, fallbackRootId: rootId)
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
        // FOLDER BUNDLES before the tree loads, so an instance's handler resolves
        // (python main._bootstrap → kernel_state.load_bundles).
        Folders.loadBundles(kernel: kernel, root: dotFantastic)
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
