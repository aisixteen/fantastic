// The shared AI-backend test harness: a scripted provider, a mock bundle, an
// in-memory file_bridge, a payload recorder, a raising bundle, and the polling
// helpers every suite drives them with.
//
// Its own file because two suites use it — `AIBackendTests` (the verb surface,
// streaming, persistence, cancellation, context) and `RoutingTests` (the
// dispatch path) — and because the SCRIPT half of it is a prerequisite for
// routing tests rather than a nicety: see `ProviderScript`.

import FantasticJSON
import FantasticKernel
import OrderedCollections
import Foundation
import Testing

@testable import FantasticAICore

// MARK: - the scripted provider

/// The per-pass script and the request log, held OUTSIDE the provider.
///
/// `AIBackendConfig.makeProvider` runs PER SEND, so a chain builds a FRESH
/// provider for every pass: a script stored inside the provider would replay
/// pass 1 forever, and a request log inside it would only ever hold that one
/// pass. Both live here, and every provider built for the same agent shares one
/// box.
///
/// Mirrors python's `_FakeProvider` (`test_send_to.py:37-51`) and its
/// `_until_calls` helper (`:68-77`). This is what makes an over-eager chain FAIL
/// rather than pass: `calls` is asserted exactly, and an exhausted script yields
/// an EMPTY stream instead of replaying, so a runaway shows up as a count that
/// does not match rather than as more of the same text.
final class ProviderScript: @unchecked Sendable {
    private let lock = NSLock()
    private var passes: [[String]]
    private var _calls = 0
    private var _requests: [[JSON]] = []

    /// One entry per expected pass; each entry is that pass's token stream.
    init(_ passes: [[String]]) { self.passes = passes }

    /// Log the request and consume the next pass. Empty once the script runs out.
    func take(_ messages: [JSON]) -> [String] {
        lock.lock()
        defer { lock.unlock() }
        _requests.append(messages)
        _calls += 1
        return passes.isEmpty ? [] : passes.removeFirst()
    }

    /// How many passes the provider was actually asked for — python's `fp.calls`.
    var calls: Int {
        lock.lock()
        defer { lock.unlock() }
        return _calls
    }

    /// The messages of pass `i` (0-based), or nil if that pass never happened.
    func request(_ i: Int) -> [JSON]? {
        lock.lock()
        defer { lock.unlock() }
        return i < _requests.count ? _requests[i] : nil
    }

    /// Pass `i`'s messages as one searchable string — python's
    /// `str(fp.requests[i])`. Serialized, so an assertion can look for the text
    /// of a delivered reply anywhere in the turn.
    func requestText(_ i: Int) -> String {
        JSON.array(request(i) ?? []).serialize()
    }

    /// python's `_until_calls`: poll until `n` passes have been asked for.
    func waitForCalls(_ n: Int, timeout: Double = 5.0) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if calls >= n { return true }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        return calls >= n
    }
}

/// Deterministic provider: streams one scripted pass per `chat()` call, or
/// throws. Providers are pure raw-text streamers (`.token` only). Mirrors the
/// Rust `MockProvider`.
final class MockProvider: AIProvider, @unchecked Sendable {
    let model: String
    private let script: ProviderScript
    let throwsError: Bool
    /// Per-chunk delay (ns) so a generation can be observed in-flight.
    let chunkDelayNanos: UInt64

    /// A SHARED script — required whenever more than one pass is expected, since
    /// each pass gets a new provider instance.
    init(
        script: ProviderScript, model: String = "mock-1",
        throwsError: Bool = false, chunkDelayNanos: UInt64 = 0
    ) {
        self.model = model
        self.script = script
        self.throwsError = throwsError
        self.chunkDelayNanos = chunkDelayNanos
    }

    /// ONE pass yielding `tokens`, with a private log — the shape a single-send
    /// test wants, where a fresh provider per send is exactly right.
    convenience init(
        model: String = "mock-1", tokens: [String] = ["Hel", "lo"],
        throwsError: Bool = false, chunkDelayNanos: UInt64 = 0
    ) {
        self.init(
            script: ProviderScript([tokens]), model: model,
            throwsError: throwsError, chunkDelayNanos: chunkDelayNanos)
    }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let throwsError = throwsError
        let chunkDelayNanos = chunkDelayNanos
        // Consume the pass HERE, not inside the stream's task, so `calls` counts
        // "the backend asked for a pass" at a deterministic point. (Python's
        // async generator body runs on first iteration, which the backend does
        // immediately — same thing, minus the race.)
        let tokens = script.take(messages)
        return AsyncThrowingStream { continuation in
            Task {
                if throwsError {
                    continuation.finish(throwing: MockError.boom)
                    return
                }
                for t in tokens {
                    if chunkDelayNanos > 0 { try? await Task.sleep(nanoseconds: chunkDelayNanos) }
                    continuation.yield(.token(t))
                }
                continuation.finish()
            }
        }
    }
}

enum MockError: Error, CustomStringConvertible {
    case boom
    var description: String { "boom" }
}

// MARK: - bundles

/// Test bundle wrapping `AIBackend` with a configurable mock provider.
final class MockBundle: AgentBundle, @unchecked Sendable {
    let name = "mock_backend"
    private let core: AIBackend

    init(config: AIBackendConfig) {
        self.core = buildAIBackend(config)
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The shared cancel hook — every real backend forwards exactly like this.
    func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }
}

/// In-memory file_bridge stand-in: dict-backed STREAM verbs (`write_stream` /
/// `read_stream` on the binary channel — the string read/write verbs are gone),
/// so the real chat persistence path (the JSONL sidecar
/// `agents/<self>/chat_<client>.jsonl` → the AI's own `file_bridge_id` →
/// write_stream/read_stream) is exercised without touching disk.
final class MockFileBridge: AgentBundle, @unchecked Sendable {
    let name = "mock_file_bridge"
    private let lock = NSLock()
    private var files: [String: String] = [:]

    /// ONE bundle instance backs EVERY file_bridge agent in the kernel, so the
    /// dict is keyed by `<bridge agent>/<path>` — each leg has its own root in
    /// reality, and two legs must not share a `state.yaml`.
    private func get(_ agentId: AgentId, _ path: String) -> String? {
        lock.lock()
        defer { lock.unlock() }
        return files["\(agentId.value)/\(path)"]
    }
    private func put(_ agentId: AgentId, _ path: String, _ content: String) {
        lock.lock()
        defer { lock.unlock() }
        files["\(agentId.value)/\(path)"] = content
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("mock fs"),
                "kind": .string("file_bridge"), "verbs": .object([:]),
            ])
        default:
            return .object(["error": .string("unknown verb")])
        }
    }

    func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data) {
        let path = header["path"].asString ?? ""
        switch header["type"].asString ?? "" {
        case "write_stream":
            // Stream semantics, not replace: absent offset + no truncate =
            // APPEND at end (the JSONL sidecar's whole write path). Mirrors the
            // real bundle; a replace here silently ate every turn but the last.
            let incoming = String(data: blob, encoding: .utf8) ?? ""
            let existing =
                (header["truncate"].asBool == true) ? "" : (get(agentId, path) ?? "")
            let content = existing + incoming
            put(agentId, path, content)
            return (
                .object([
                    "written": .bool(true),
                    "size": .integer(Int64(content.utf8.count)),
                ]), Data()
            )
        case "read_stream":
            guard let c = get(agentId, path) else {
                return (.object(["error": .string("not found")]), Data())
            }
            let data = Data(c.utf8)
            return (
                .object([
                    "eof": .bool(true), "size": .integer(Int64(data.count)),
                    "next_offset": .integer(Int64(data.count)),
                ]), data
            )
        default:
            return (.object(["error": .string("unknown verb")]), Data())
        }
    }
}

/// Records every payload it is sent, verbatim. A routing test points `reply_to`
/// (or `send_to`) at this so the DELIVERED TURN can be inspected key by key —
/// the only way to assert what routing does NOT put on the wire.
final class PayloadRecorder: AgentBundle, @unchecked Sendable {
    let name = "mock_recorder"
    private let lock = NSLock()
    private var seen: [JSON] = []
    private var blobs: [(JSON, Data)] = []

    private func record(_ payload: JSON) {
        lock.lock()
        seen.append(payload)
        lock.unlock()
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("payload recorder"),
                "verbs": .object([:]),
            ])
        // LIFECYCLE, not traffic. `create_agent` dispatches `boot` to every fresh
        // agent, so recording it would make `payloads()` non-empty before a test
        // has done anything — and "nothing was dispatched" is precisely what
        // several of these tests assert.
        case "boot", "shutdown":
            return nil
        default:
            record(payload)
            return .object(["recorded": .bool(true)])
        }
    }

    /// The BINARY channel. A `send_binary` whose blob was lifted correctly lands
    /// here; one that was not lands on `handle` with the program still stuck in
    /// the JSON header, which is exactly the bug this records the difference for.
    func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data) {
        lock.withLock { blobs.append((header, blob)) }
        return (.object(["written": .integer(Int64(blob.count))]), Data())
    }

    /// Every `(header, blob)` that reached the BINARY channel, in arrival order.
    func binaries() -> [(JSON, Data)] {
        lock.lock()
        defer { lock.unlock() }
        return blobs
    }

    /// Every payload received (lifecycle + `reflect` excluded), in arrival order.
    func payloads() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return seen
    }

    /// Poll until at least `n` payloads have arrived.
    func waitForPayloads(_ n: Int, timeout: Double = 5.0) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if payloads().count >= n { return true }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        return payloads().count >= n
    }
}

/// A handler that takes its time. Routing AWAITS each dispatched call, so a slow
/// target is the only way to hold a `route_reply` task open long enough for `ps`
/// to be observed mid-route.
final class SlowBundle: AgentBundle, @unchecked Sendable {
    let name = "mock_slow"

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            try? await Task.sleep(nanoseconds: 500_000_000)
            return .object(["slow": .bool(true)])
        }
    }
}

/// A handler that RAISES on every real verb — the failure python measured on
/// 2026-08-02 (a `file_bridge` whose ingress rule could not be resolved threw out
/// of its handler and killed the chain silently). `boot`/`shutdown` answer
/// normally so the agent can be created at all.
final class RaisingBundle: AgentBundle, @unchecked Sendable {
    let name = "mock_raiser"

    enum Failure: Error, CustomStringConvertible {
        case unresolvableIngressRule(String)
        /// Rendered the way the kernel renders any caught bundle error
        /// (`"\(error)"`), with the type name in front so the model is told WHAT
        /// went wrong as well as where — python's `f"{type(exc).__name__}: {exc}"`.
        var description: String {
            switch self {
            case .unresolvableIngressRule(let rule):
                return "IngressRuleError: unknown ingress rule type '\(rule)'"
            }
        }
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default: throw Failure.unresolvableIngressRule("password:whatever")
        }
    }
}

/// Collects everything emitted on an agent's inbox (token / status / done).
final class EventSink: @unchecked Sendable {
    private let lock = NSLock()
    private var events: [JSON] = []

    func add(_ e: JSON) {
        lock.lock()
        events.append(e)
        lock.unlock()
    }

    func snapshot() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }

    func first(type: String) -> JSON? {
        snapshot().first { $0["type"].asString == type }
    }

    /// The terminal event of a turn: `status` with `phase:'done'`. There is no
    /// `done` event on any runtime (B2.15) — this is what a client waits on.
    func statusDone() -> JSON? {
        snapshot().first { $0["type"].asString == "status" && $0["phase"].asString == "done" }
    }
}

// MARK: - config + kernel boot

func baseConfig(
    provider: @escaping @Sendable () -> ProviderResult
) -> AIBackendConfig {
    AIBackendConfig(
        kind: "mock_backend",
        sentence: "Mock LLM agent.",
        defaultModel: "(no default — set the `model` meta)",
        defaultEndpoint: "http://mock.invalid",
        reflectExtra: { _ in ["extra": .string("yes")] },
        makeProvider: { _, _, _ in provider() }
    )
}

/// The routing config: one `ProviderScript` PER AGENT ID, so a chain and its peer
/// each get their own script and their own request log (python does this by
/// keying `ot._providers` on the agent id).
///
/// An agent with no script is a REFUSAL naming the id — never a silently empty
/// provider, which would look exactly like a model that chose to stop.
func scriptedConfig(_ scripts: [String: ProviderScript]) -> AIBackendConfig {
    var cfg = baseConfig { .refused(.object(["error": .string("unreachable")])) }
    cfg.makeProvider = { agent, _, _ in
        guard let script = scripts[agent.id.value] else {
            return .refused(
                .object([
                    "error": .string("test harness: no ProviderScript for \(agent.id.value)")
                ]))
        }
        return .provider(MockProvider(script: script))
    }
    return cfg
}

/// Boot an in-memory kernel whose registry has the mock backend, then
/// create an agent for it. Built directly (not via
/// `startKernelInMemory`) so we can inject the mock bundle.
func freshMockKernel(config: AIBackendConfig) async throws -> (Kernel, AgentId) {
    let (kernel, ids, _) = await bootMockKernel(config: config, backendIds: ["mock"])
    return (kernel, ids["mock"]!)
}

/// The routing boot: a backend agent per id in `backendIds`, a `probe` payload
/// recorder, a `raiser` throwing agent, and an explicit `context_window`.
///
/// `contextWindow` is NOT decoration. The default (4096) is smaller than the
/// system block once `SEND_TO_HOWTO` is in it plus a turn of history, so pass 2
/// of any chain would die on `context_insufficient` — which looks exactly like a
/// routing bug. Python documents the same trap at `test_send_to.py:53-66`.
func bootMockKernel(
    config: AIBackendConfig,
    backendIds: [String],
    contextWindow: Int64 = 32768,
    meta: OrderedCollections.OrderedDictionary<String, JSON> = [:]
) async -> (Kernel, [String: AgentId], PayloadRecorder) {
    let registry = BundleRegistry()
    registry.register("mock_backend.tools", MockBundle(config: config))
    registry.register("file_bridge.tools", MockFileBridge())
    let recorder = PayloadRecorder()
    registry.register("mock_recorder.tools", recorder)
    registry.register("mock_raiser.tools", RaisingBundle())
    registry.register("mock_slow.tools", SlowBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    // A REAL root path, so `findStore` has something to match the store's own
    // `root` against — that match is how the loader owns sidecar placement.
    let storeRoot = URL(fileURLWithPath: NSTemporaryDirectory())
        .appendingPathComponent("fantastic-mock-\(UUID().uuidString)")
        .appendingPathComponent(".fantastic")
    let root = Agent(
        id: AgentId("core"), handlerModule: nil, parentId: nil, rootPath: storeRoot)
    kernel.register(root)
    kernel.setRoot(root)
    // Two-sided deny-all: each callee must ACCEPT the backends that reach it. The
    // backends persist to `store`, route replies to probe/raiser/slowpoke, and call
    // each other — so every target's `callable_by` accepts all backend ids (ingress),
    // the mirror of the backends' `can_call` (egress).
    var accept: OrderedCollections.OrderedDictionary<String, JSON> = [:]
    for id in backendIds { accept[id] = .string("*") }
    let acceptJSON = JSON.object(accept)
    // The loader's own discoverable store (`findStore` matches a file_bridge
    // whose `root` resolves to the root agent's dir). Chat no longer rides this
    // leg — each backend gets its own below — but the kernel's record
    // persistence still wants a provider to exist.
    _ = await kernel.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("file_bridge.tools"),
            // `findStore` matches a file_bridge whose `root` resolves to the
            // root agent's own dir — that is how the loader owns sidecar
            // placement, and the stand-in must satisfy the same rule.
            "root": .string(storeRoot.path),
            "id": .string("store"),
            "callable_by": acceptJSON,
        ]))])[0].result
    _ = await kernel.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("mock_recorder.tools"),
            "id": .string("probe"),
            "callable_by": acceptJSON,
        ]))])[0].result
    _ = await kernel.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("mock_raiser.tools"),
            "id": .string("raiser"),
            "callable_by": acceptJSON,
        ]))])[0].result
    _ = await kernel.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("mock_slow.tools"),
            "id": .string("slowpoke"),
            "callable_by": acceptJSON,
        ]))])[0].result
    // Deny-all: a backend streams to `cli`, routes replies to its peers
    // (probe/raiser/slowpoke + the other backends), and its `file_bridge_id` leg
    // is granted per backend below. Grant exactly those lateral edges — wiring
    // pairs with granting.
    var grant: OrderedCollections.OrderedDictionary<String, JSON> = [
        "cli": .string("*"),
        "probe": .string("*"), "raiser": .string("*"), "slowpoke": .string("*"),
    ]
    for other in backendIds { grant[other] = .string("*") }
    var out: [String: AgentId] = [:]
    for id in backendIds {
        // ONE FILE LEG PER BACKEND. The chat sidecar lands at the leg's root
        // (`agents/<self>/chat_<client>.jsonl`) — a private leg keeps each
        // backend's transcripts under its own `agents/` tree.
        let legId = "store_\(id)"
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("file_bridge.tools"),
                "root": .string(storeRoot.appendingPathComponent(id).path),
                "id": .string(legId),
                "callable_by": acceptJSON,
            ]))])[0].result
        var backendGrant = grant
        backendGrant[legId] = .string("*")
        var create: OrderedCollections.OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("mock_backend.tools"),
            "id": .string(id),
            "file_bridge_id": .string(legId),
            "context_window": .integer(contextWindow),
            "can_call": .object(backendGrant),
            // Peer backends call each other — accept them (ingress).
            "callable_by": acceptJSON,
        ]
        // Per-test record fields (`nudge_limit`, the context_* flags) ride here so
        // a case can vary ONE thing without a second boot helper.
        for (k, v) in meta { create[k] = v }
        let r = await kernel.send([Item(AgentId("core"), .object(create))])[0].result
        out[id] = AgentId(r["id"].asString ?? id)
    }
    return (kernel, out, recorder)
}

// MARK: - task-table helpers (the `ps` / `cancel` surface, driven as a caller)

/// The live task table, read through the `ps` VERB like any caller.
func psRows(_ kernel: Kernel) async -> [JSON] {
    let r = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0].result
    return r["tasks"].asArray ?? []
}

/// Poll until `agent`'s `verb` task shows up in the table; return its id.
func waitForTask(kernel: Kernel, agent: AgentId, verb: String) async throws -> Int64 {
    for _ in 0..<300 {
        let row = await psRows(kernel).first {
            $0["agent"].asString == agent.value && $0["verb"].asString == verb
        }
        if let id = row?["id"].asInt { return id }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
    Issue.record("timed out waiting for \(agent)'s \(verb) task")
    throw CancellationError()
}

/// Poll until `cond` holds, or fail the test. Bounded so a broken
/// expectation fails instead of hanging the run.
func waitUntil(_ what: String, _ cond: () async -> Bool) async throws {
    for _ in 0..<300 {
        if await cond() { return }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
    Issue.record("timed out waiting for \(what)")
}

/// The agent's CONVERSATION as the kernel row store holds it — every row for
/// the agent minus the kernel's own bookkeeping kinds (`call`, `task`, `body`,
/// `view`, `settle`). AGENT ID IS CONVERSATION ID: there is no client axis.
func conversationRows(kernel: Kernel, id: AgentId) -> [JSON] {
    let bookkeeping: Set<String> = ["call", "task", "body", "view", "settle"]
    return kernel.rows.forAgent(id).filter { r in
        guard let k = r["kind"]?.asString else { return true }
        return !bookkeeping.contains(k)
    }.map { JSON.object($0) }
}

/// Poll the row store until the conversation reaches `count` rows (routing
/// runs in a spawned task, so a chain's turns land asynchronously). Bounded so
/// a hung stream fails the test instead of hanging the run.
func waitForHistory(kernel: Kernel, id: AgentId, count: Int) async throws {
    for _ in 0..<200 {
        if conversationRows(kernel: kernel, id: id).count >= count { return }
        try await Task.sleep(nanoseconds: 5_000_000)
    }
}

/// The explicit end-of-chain call, as the model writes it.
let doneEnvelope = "<call name=\"done\"></call>"
