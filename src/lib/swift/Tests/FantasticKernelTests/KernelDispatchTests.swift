// Kernel.send / emit / subscribe + system verb tests.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

import FantasticKernelState

@testable import FantasticKernel

/// Echo bundle — verb dispatch returns the payload echoed back
/// with `{type: "echoed", original: payload}`.
struct EchoBundle: AgentBundle {
    let name = "echo"

    func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        return .object([
            "type": .string("echoed"),
            "original": payload,
        ])
    }
}

/// Captures every state event for assertions.
final class EventCapture: @unchecked Sendable {
    private let lock = NSLock()
    private var events: [JSON] = []
    private var bodies: [Blob?] = []

    func handler() -> StateSubscriber {
        return { [weak self] ev, body in
            self?.lock.lock()
            self?.events.append(ev)
            self?.bodies.append(body)
            self?.lock.unlock()
        }
    }

    func snapshot() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }

    /// The raw body that rode beside each event — `nil` for a text payload.
    func bodySnapshot() -> [Blob?] {
        lock.lock()
        defer { lock.unlock() }
        return bodies
    }
}

/// A kernel whose ROOT carries the `kernel_state.tools` loader, as every real one
/// does. The root reflect FRAME (`persistence`, `guid`, `readmes`, `docs`,
/// `self_extension`) is that bundle's `reflectRootExtra` contribution, so a
/// bare-root fixture carries none of it.
func makeLoaderKernel() -> Kernel {
    let registry = BundleRegistry()
    registry.register("echo.tools", EchoBundle())
    registry.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId(ROOT_ID), handlerModule: ROOT_HANDLER_MODULE, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

func makeKernel(withEcho: Bool = true) -> Kernel {
    let registry = BundleRegistry()
    if withEcho {
        registry.register("echo.tools", EchoBundle())
    }
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

@Suite("Kernel.send dispatch")
struct KernelSendTests {
    @Test func sendToBareAgentBoot() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("kernel_state", ["type": "boot"])])[0].result
        // Bare agent's boot returns null per substrate convention.
        #expect(reply.isNull)
    }

    @Test func sendToBundleAgentEchoes() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "echo1",
            ])])[0].result
        let reply = await kernel.send([Item("echo1", ["type": "say", "text": "hi"])])[0].result
        #expect(reply["type"].asString == "echoed")
        #expect(reply["original"]["text"].asString == "hi")
    }

    @Test func sendToMissingAgentNamesTheLegalIds() async {
        // A bare `no agent 'x'` teaches the caller nothing it can act on.
        // Measured 2026-08-04: a model copied the literal placeholder `<id>` out
        // of a prompt example, got the same refusal 34 times in one run, and
        // never corrected. The call still fails exactly once, by the one path —
        // it just says what the legal ids are on the way out.
        let kernel = makeKernel()
        let out = await kernel.send([Item("ghost", ["type": "anything"])])[0]
        #expect(out.status == .failed)
        #expect(out.pid == nil, "a refusal never reached the task table")
        #expect(out.result["error"].asString?.contains("no agent") == true)
        #expect(out.result["reason"].asString == "unknown_agent")
        #expect(out.result["agents"].asArray?.contains(.string("kernel_state")) == true)
    }

    @Test func sendUnknownHandlerModuleReturnsError() async {
        let kernel = makeKernel(withEcho: false)
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "missing.tools",
                "id": "x",
            ])])[0].result
        let reply = await kernel.send([Item("x", ["type": "say"])])[0].result
        #expect(reply["error"].asString?.contains("missing.tools") == true)
    }

    /// C24: python's PREFIX pinned verbatim (`agent <id!r>: handler_module
    /// <hm!r> cannot be imported in this runtime: `); the tail is a D3
    /// residual (python's own import error text is not stable across runs —
    /// "No module named 'yaml_state'" one round, "...'yaml_state.tools'" the
    /// next — so it is not pinned here either).
    @Test func handlerModuleNotInstalledPrefixIsPythonExact() async {
        let kernel = makeKernel(withEcho: false)
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "yaml_state.tools",
                "id": "yaml",
            ])])[0].result
        let reply = await kernel.send([Item("yaml", ["type": "reflect"])])[0].result
        #expect(
            reply["error"].asString
                == "agent 'yaml': handler_module 'yaml_state.tools' cannot be imported in this "
                    + "runtime: ModuleNotFoundError: No module named 'yaml_state.tools'",
            "\(reply)")
        #expect(reply["reason"].asString == "unhandled")
        #expect(reply["verb"].asString == "reflect")
    }

    @Test func kernelAliasResolvesToRoot() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("kernel", ["type": "reflect"])])[0].result
        #expect(reply["id"].asString == "kernel_state")
    }
}

@Suite("Kernel.emit")
struct KernelEmitTests {
    @Test func emitDeliversToInbox() async {
        let kernel = makeKernel()
        kernel.register(Agent(id: "listener", handlerModule: nil, parentId: nil))
        // THE READER REGISTERS ITS OWN INBOX (C29): `register` mints none.
        let stream = kernel.ensureInbox("listener")
        await kernel.emit("listener", ["type": "ping", "x": 1])

        var iterator = stream.makeAsyncIterator()
        let first = await iterator.next()
        #expect(first?.payload["type"].asString == "ping")
        #expect(first?.payload["x"].asInt == 1)
    }

    /// a16 (audit 2026-09-28): an emit onto ANOTHER agent's inbox passes the
    /// two-sided gate under verb `emit`; a denied emit is dropped. Emitting on
    /// your own inbox is how every bundle publishes, so it stays ungated.
    @Test func emitOnAnotherAgentNeedsBothHalves() async {
        let kernel = makeKernel()
        kernel.register(
            Agent(
                id: "em_open", handlerModule: nil, parentId: "kernel_state",
                meta: ["can_call": ["em_target": ["emit"]]]))
        kernel.register(Agent(id: "em_closed", handlerModule: nil, parentId: "kernel_state"))
        kernel.register(
            Agent(
                id: "em_target", handlerModule: nil, parentId: "kernel_state",
                meta: ["callable_by": ["em_open": ["emit"], "em_closed": ["emit"]]]))
        let inbox = kernel.ensureInbox("em_target")
        await KernelTaskLocals.$currentSender.withValue(AgentId("em_closed")) {
            await kernel.emit("em_target", ["type": "poke", "n": 1])
        }
        #expect(inbox.tryTake() == nil, "an emit with no egress grant must be dropped")
        await KernelTaskLocals.$currentSender.withValue(AgentId("em_open")) {
            await kernel.emit("em_target", ["type": "poke", "n": 2])
        }
        #expect(inbox.tryTake()?.payload == ["type": "poke", "n": 2])
        await KernelTaskLocals.$currentSender.withValue(AgentId("em_target")) {
            await kernel.emit("em_target", ["type": "self", "n": 3])
        }
        #expect(inbox.tryTake()?.payload["n"] == 3)
    }

    /// C29 — an agent gets no queue merely by existing.
    @Test func registerMintsNoInbox() async {
        let kernel = makeKernel()
        kernel.register(Agent(id: "quiet", handlerModule: nil, parentId: nil))
        #expect(kernel.hasInbox("quiet") == false)
        kernel.ensureInbox("quiet")
        #expect(kernel.hasInbox("quiet") == true)
    }

    /// C28 + C29 — a registered agent that never called `ensureInbox` cannot be
    /// a watcher, and the refusal says so in python's words.
    @Test func watchByABareRegisteredAgentIsRefused() async {
        let kernel = makeKernel()
        kernel.register(Agent(id: "ghost", handlerModule: nil, parentId: nil))
        let denial = kernel.watch(src: "kernel_state", watcher: "ghost")
        #expect(
            denial?["error"].asString
                == "watch: 'ghost' has no registered inbox, so nothing would read "
                    + "what is mirrored there")
        #expect(denial?["reason"].asString == "no_consumer")
        #expect(
            denial?["hint"].asString
                == "register the reader first — ctx.ensure_inbox('ghost') — then watch")
        #expect(denial?["watcher"].asString == "ghost")
        #expect(denial?["src"].asString == "kernel_state")
    }

    /// C78 — an emit to an id no agent holds does nothing at all: no inbox put,
    /// no state event. Python `emit` resolves the target first.
    @Test func emitToNonAgentIdIsANoop() async {
        let kernel = makeKernel()
        let stream = kernel.ensureInbox("_ws_beef")
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        await kernel.emit("_ws_beef", ["type": "thing"])
        #expect(cap.snapshot().isEmpty)
        var it = stream.makeAsyncIterator()
        // Finish the stream so the read below cannot hang on an empty inbox.
        kernel.removeInbox("_ws_beef")
        #expect(await it.next() == nil)
    }

    @Test func emitPublishesStateEvent() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        await kernel.emit("kernel_state", ["type": "thing"])
        let evs = cap.snapshot()
        #expect(
            evs.contains {
                $0["kind"].asString == "emit" && $0["agent_id"].asString == "kernel_state"
                    && $0["summary"].asString == #"{"type": "thing"}"#
            })
    }
}

@Suite("Kernel.subscribe state events")
struct KernelSubscribeTests {
    @Test func sendPublishesStateEvent() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        _ = await kernel.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let evs = cap.snapshot()
        let sendEvents = evs.filter { $0["kind"].asString == "send" }
        #expect(!sendEvents.isEmpty)
        #expect(sendEvents.last?["payload"]["type"].asString == "list_agents")
        // Python pairs every `send` with a `drain` once the handler returns.
        // BY KIND, not by position: the settle row's own `row` event (2026-09-23)
        // lands after the drain, exactly as python's `drains[-1]` reads it.
        let drain = evs.last { $0["kind"].asString == "drain" }
        #expect(drain != nil)
        #expect(drain?["backlog"].asInt == 0)
    }

    @Test func unsubscribeStopsDelivery() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        let token = kernel.subscribe(cap.handler())
        _ = await kernel.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let before = cap.snapshot().count
        kernel.unsubscribe(token)
        _ = await kernel.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let after = cap.snapshot().count
        #expect(after == before)
    }

    @Test func sendAsAttributesSender() async {
        let kernel = makeKernel()
        // Seed a real `ui` agent as a CHILD of the root: every verb is gated now
        // (reflect included), so only a PERMITTED send fires the state event whose
        // sender attribution is under test. A child reflecting its parent is covered
        // by the structural baseline on both ends — no explicit grant needed.
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools", "id": "ui"])])[0].result
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        _ = await kernel.sendAs(
            sender: "ui", target: "kernel_state", payload: ["type": "reflect"])
        let evs = cap.snapshot()
        let sends = evs.filter { $0["kind"].asString == "send" }
        #expect(sends.last?["sender"].asString == "ui")
    }

    /// C77 — the whole traffic-event contract in one place: key order, `sender`
    /// JSON null for an operator send, `summary` through `py_json_dumps`, a
    /// `drain` after the handler, and `ts` stamped LAST.
    @Test func stateEventsArePythonShape() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        _ = await kernel.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let evs = cap.snapshot()
        let send = evs.first { $0["kind"].asString == "send" }
        #expect(
            send?.asObject?.keys.elements
                == ["agent_id", "kind", "backlog", "sender", "summary", "payload", "ts"])
        #expect(send?["agent_id"].asString == "kernel_state")
        #expect(send?["backlog"].asInt == 1)
        // An OPERATOR send carries JSON null — never the target's own id.
        #expect(send?["sender"] == JSON.null)
        #expect(send?["summary"].asString == #"{"type": "list_agents"}"#)
        let drain = evs.last { $0["kind"].asString == "drain" }
        #expect(drain?.asObject?.keys.elements == ["agent_id", "kind", "backlog", "ts"])
        #expect(drain?["kind"].asString == "drain")
        #expect(drain?["backlog"].asInt == 0)
    }

    /// C77 — the snapshot is every agent in REGISTRATION order, reporting the
    /// in-flight handler count, not a queue length.
    @Test func stateSnapshotIsRegistrationOrderWithInFlight() async {
        let kernel = makeKernel()
        for id in ["beta", "alpha"] {
            _ = await kernel.send([
                Item(
                    "kernel_state",
                    [
                        "type": "create_agent", "parent_id": "kernel_state",
                        "handler_module": "echo.tools", "id": .string(id),
                    ])
            ])[0].result
        }
        let snap = kernel.stateSnapshot()
        #expect(snap.map { $0["agent_id"].asString } == ["kernel_state", "beta", "alpha"])
        #expect(snap.allSatisfy { $0["backlog"].asInt == 0 })
        #expect(snap.first?.asObject?.keys.elements == ["agent_id", "name", "backlog"])
    }

    /// C77 — lifecycle events carry python's `{agent_id, kind, …}` shape, not the
    /// port's `created`/`id`/`handler_module`.
    @Test func lifecycleEventsArePythonShape() async {
        let kernel = makeKernel()
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools", "id": "kid"])])[0].result
        _ = await kernel.send([Item("kernel_state", ["type": "update_agent", "id": "kid", "display_name": "Kid"])])[0].result
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "kid"])])[0].result
        let evs = cap.snapshot()
        let added = evs.first { $0["kind"].asString == "added" }
        #expect(added?.asObject?.keys.elements == ["agent_id", "kind", "name", "parent_id", "ts"])
        #expect(added?["agent_id"].asString == "kid")
        #expect(added?["name"].asString == "kid")
        #expect(added?["parent_id"].asString == "kernel_state")
        let updated = evs.first { $0["kind"].asString == "updated" }
        #expect(updated?.asObject?.keys.elements == ["agent_id", "kind", "changed", "ts"])
        #expect(updated?["changed"].asArray?.first?.asString == "display_name")
        let removed = evs.first { $0["kind"].asString == "removed" }
        #expect(removed?.asObject?.keys.elements == ["agent_id", "kind", "name", "ts"])
        #expect(removed?["name"].asString == "Kid")
    }
}

@Suite("Watcher fanout")
struct WatcherFanoutTests {
    /// C76 — the payload reaches the watcher BEFORE the handler runs, and it
    /// reaches the target's own registered inbox too.
    @Test func sendFansOutBeforeHandlerRuns() async {
        let registry = BundleRegistry()
        let seen = OrderBox()
        registry.register("slow.tools", OrderBundle(seen: seen))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "slow.tools", "id": "worker"])])[0].result

        let watcher = kernel.ensureInbox("spy")
        kernel.watch(src: "worker", watcher: "spy")
        // The target's OWN inbox is a reader too — python puts there first.
        let own = kernel.ensureInbox("worker")

        var wit = watcher.makeAsyncIterator()
        var oit = own.makeAsyncIterator()
        async let reply = kernel.send([Item("worker", ["type": "work"])])[0].result
        let mirrored = await wit.next()
        #expect(mirrored?.payload["type"].asString == "work")
        // Observed while the handler is still suspended: the fanout ran first.
        #expect(seen.handlerRan() == false)
        seen.release()
        _ = await reply
        let ownCopy = await oit.next()
        #expect(ownCopy?.payload["type"].asString == "work")
    }

    /// C86 — a binary send fans the FULL payload out: the header with `bytes`
    /// nulled, plus the raw body at that path. Without it a watcher could never
    /// receive the binary frame python sends.
    @Test func watchedBinarySendCarriesTheBody() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools", "id": "sink"])])[0].result
        let watcher = kernel.ensureInbox("spy")
        kernel.watch(src: "sink", watcher: "spy")
        let cap = EventCapture()
        _ = kernel.subscribe(cap.handler())

        var it = watcher.makeAsyncIterator()
        let blob = Data([1, 2, 3, 4, 5])
        _ = await kernel.send([
            Item("sink", ["type": "write_stream", "path": "a.bin"], binary: Blob("bytes", blob)),
        ])
        let got = await it.next()
        #expect(got?.payload["type"].asString == "write_stream")
        #expect(got?.payload["bytes"] == JSON.null)
        #expect(got?.body?.parts == ["bytes"])
        #expect(got?.body?.bytes == Data([1, 2, 3, 4, 5]))
        // The state event carries the same body, and its summary shrinks it.
        let idx = cap.snapshot().firstIndex { $0["kind"].asString == "send" }
        #expect(idx != nil)
        #expect(cap.bodySnapshot()[idx ?? 0]?.bytes == Data([1, 2, 3, 4, 5]))
        #expect(
            cap.snapshot()[idx ?? 0]["summary"].asString
                == #"{"type": "write_stream", "path": "a.bin", "bytes": "<bytes:5>"}"#)
    }
}

/// A handler that parks until released — so a test can observe what the kernel
/// published BEFORE it ran.
final class OrderBox: @unchecked Sendable {
    private let lock = NSLock()
    private var ran = false
    private let sem = DispatchSemaphore(value: 0)
    func handlerRan() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return ran
    }
    func enter() {
        sem.wait()
        lock.lock()
        ran = true
        lock.unlock()
    }
    func release() { sem.signal() }
}

struct OrderBundle: AgentBundle {
    let name = "order"
    let seen: OrderBox
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        if payload["type"].asString == "work" { seen.enter() }
        return .object(["ok": .bool(true)])
    }
}

@Suite("System verbs")
struct SystemVerbTests {
    @Test func listAgents() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let agents = reply["agents"].asArray ?? []
        #expect(agents.count == 1)
        #expect(agents[0]["id"].asString == "kernel_state")
    }

    @Test func createAgent() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "alpha",
            ])])[0].result
        #expect(reply["id"].asString == "alpha")
        #expect(reply["handler_module"].asString == "echo.tools")

        let listed = await kernel.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let names = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(names.contains("alpha"))
    }

    /// The `agent_created` event lands on the PARENT's inbox at every depth
    /// (audit 2026-10-03, KV-4). It was emitted as the root, so the gate judged
    /// root→parent, and the root's `$children` baseline reaches depth 1 only: a
    /// parent two levels down never got it — an operator create included, an
    /// agent creating under itself too — and nothing reported the drop.
    @Test func agentCreatedReachesAParentAtAnyDepth() async {
        let kernel = makeKernel()
        func create(_ parent: String, _ id: String) async -> JSON {
            await kernel.send([
                Item("kernel", [
                    "type": "create_agent", "parent_id": .string(parent),
                    "handler_module": "echo.tools", "id": .string(id),
                ])
            ])[0].result
        }
        #expect(await create("kernel", "kv4_a")["id"].asString == "kv4_a")
        #expect(await create("kv4_a", "kv4_b")["id"].asString == "kv4_b")  // depth 2
        let inbox = kernel.ensureInbox("kv4_b")
        #expect(await create("kv4_b", "kv4_by_operator")["id"].asString == "kv4_by_operator")
        // baseline only: placement under itself
        let bySelf = await KernelTaskLocals.$currentSender.withValue(AgentId("kv4_b")) {
            await create("kv4_b", "kv4_by_self")
        }
        #expect(bySelf["id"].asString == "kv4_by_self", "\(bySelf)")
        var created: [String] = []
        while let item = inbox.tryTake() {
            if item.payload["type"].asString == "agent_created" {
                created.append(item.payload["id"].asString ?? "")
            }
        }
        #expect(created == ["kv4_by_operator", "kv4_by_self"])
    }

    @Test func createAgentRequiresHandlerModule() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "id": "x"])])[0].result
        #expect(reply["error"].asString?.contains("handler_module") == true)
    }

    /// C26: an empty STRING is exactly as missing as an absent key — python
    /// `if not isinstance(x, str) or not x`.
    @Test func emptyRequiredArgsAreMissing() async {
        let kernel = makeKernel()
        let hm = await kernel.send([
            Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "", "id": "x"])
        ])[0].result
        #expect(hm["reason"].asString == "missing_arg")
        #expect(hm["arg"].asString == "handler_module")
        let pid = await kernel.send([
            Item("kernel_state", ["type": "create_agent", "parent_id": "", "handler_module": "echo.tools", "id": "x"])
        ])[0].result
        #expect(pid["reason"].asString == "missing_arg")
        #expect(pid["arg"].asString == "parent_id")
    }

    /// C26: the `hint` key precedes `arg` in the refusal object — python's
    /// `refusal()` writes `error, reason, hint, ...extra` in that order, and
    /// `missing()` passes `arg` as part of `extra`.
    @Test func missingHintOrdersHintBeforeArg() async {
        let kernel = makeKernel()
        let reply = await kernel.send([
            Item("kernel_state", ["type": "create_agent", "handler_module": "echo.tools", "id": "x"])
        ])[0].result
        let keys = reply.asObject?.keys.map { $0 } ?? []
        let hintIdx = keys.firstIndex(of: "hint")
        let argIdx = keys.firstIndex(of: "arg")
        #expect(hintIdx != nil && argIdx != nil && hintIdx! < argIdx!, "\(keys)")
        #expect(
            reply["hint"].asString
                == "name the agent that will OWN the new one — your own id for a tool you keep, "
                    + "or the root ('kernel_state') for a top-level agent", "\(reply)")
    }

    /// C26: check order is handler_module → parent_id → resolve parent →
    /// reserved keys (sorted) → grant shape → validate_meta → guard id →
    /// exists. Each case below breaks TWO checks at once; the refusal must
    /// name the earlier one.
    @Test func createRefusalPrecedenceMatchesPython() async {
        let kernel = makeKernel()
        // unknown parent_id BEFORE a reserved key in the payload.
        let unknownParent = await kernel.send([
            Item("kernel_state", [
                "type": "create_agent", "parent_id": "ghost", "handler_module": "echo.tools",
                "id": "x", "ctx": "nope",
            ])
        ])[0].result
        #expect(unknownParent["reason"].asString == "unknown_agent", "\(unknownParent)")
        // a reserved key BEFORE a bad grant shape.
        let reservedBeforeGrant = await kernel.send([
            Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools",
                "id": "y", "ctx": "nope", "can_call": "*",
            ])
        ])[0].result
        #expect(reservedBeforeGrant["reason"].asString == "bad_arg", "\(reservedBeforeGrant)")
        #expect(reservedBeforeGrant["arg"].asString == "ctx")
        // a bad grant shape BEFORE the duplicate-id check (`dup` already exists).
        _ = await kernel.send([
            Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools", "id": "dup2"])
        ])[0].result
        let grantBeforeExists = await kernel.send([
            Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools",
                "id": "dup2", "can_call": "*",
            ])
        ])[0].result
        #expect(grantBeforeExists["reason"].asString == "invalid_meta", "\(grantBeforeExists)")
    }

    @Test func createAgentRefusesDuplicate() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "dup",
            ])])[0].result
        let second = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "dup",
            ])])[0].result
        // Python: `f"agent {id!r} exists"` + a hint to pick another id or
        // update the existing one (C26).
        #expect(second["error"].asString == "agent 'dup' exists", "\(second)")
        #expect(second["reason"].asString == "exists")
        #expect(second["hint"].asString == "pick another id, or update the existing agent")
        #expect(second["id"].asString == "dup")
    }

    @Test func deleteAgent() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "doomed",
            ])])[0].result
        let reply = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "doomed"])])[0].result
        #expect(reply["deleted"].asBool == true)
        let listed = await kernel.send([Item("kernel_state", ["type": "list_agents"])])[0].result
        let names = (listed["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(!names.contains("doomed"))
    }

    @Test func deleteAgentRefusedWhenLocked() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "locked",
                "delete_lock": true,
            ])])[0].result
        let reply = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "locked"])])[0].result
        // #27: the machine field, not the ad-hoc flag it replaced.
        #expect(reply["reason"].asString == "locked")
        #expect(reply["blocked_by"].asString == "locked")
    }

    @Test func deleteCascadesToChildren() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "parent",
            ])])[0].result
        _ = await kernel.send([Item("kernel", [
                "type": "create_agent",
                "parent_id": "parent",
                "handler_module": "echo.tools",
                "id": "child",
            ])])[0].result
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "parent"])])[0].result
        #expect(kernel.agent("parent") == nil)
        #expect(kernel.agent("child") == nil)
    }

    @Test func updateAgent() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "u1",
                "display_name": "Original",
            ])])[0].result
        let updated = await kernel.send([Item("kernel_state", [
                "type": "update_agent",
                "id": "u1",
                "display_name": "Renamed",
                "color": "#ff00ff",
            ])])[0].result
        // {updated, id, agent} — python's shape; the record rides under `agent`.
        #expect(updated["updated"].asBool == true)
        #expect(updated["id"].asString == "u1")
        #expect(updated["agent"]["display_name"].asString == "Renamed")
        #expect(updated["agent"]["color"].asString == "#ff00ff")
    }

    /// C27: `update_agent` checks `handler_module` + sorted reserved keys
    /// BEFORE resolving the target id — a reserved key on an id naming NO
    /// agent still answers `bad_arg`, not `unknown_agent`.
    @Test func updateAgentReservedKeyCheckedBeforeTargetResolution() async {
        let kernel = makeKernel()
        let reply = await kernel.send([
            Item("kernel_state", ["type": "update_agent", "id": "ghost", "ctx": "nope"])
        ])[0].result
        #expect(reply["reason"].asString == "bad_arg", "\(reply)")
        #expect(reply["arg"].asString == "ctx")
        let hmReply = await kernel.send([
            Item("kernel_state", ["type": "update_agent", "id": "ghost", "handler_module": "x.tools"])
        ])[0].result
        #expect(hmReply["reason"].asString == "bad_arg", "\(hmReply)")
        #expect(hmReply["arg"].asString == "handler_module")
    }

    /// C27: `update_agent id ""` is `missing_arg`, exactly like every other
    /// empty required string arg.
    @Test func updateAgentEmptyIdIsMissingArg() async {
        let kernel = makeKernel()
        let reply = await kernel.send([Item("kernel_state", ["type": "update_agent", "id": ""])])[0].result
        #expect(reply["reason"].asString == "missing_arg")
        #expect(reply["arg"].asString == "id")
    }

    /// C27: python's exact delete_lock text — `pyRepr` on both ids, `blocked_
    /// by` then `id` in the extras, PLUS the `hint`.
    @Test func deleteAgentLockedRefusalIsPythonText() async {
        let kernel = makeKernel()
        _ = await kernel.send([
            Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools",
                "id": "locked2", "delete_lock": true,
            ])
        ])[0].result
        let reply = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "locked2"])])[0]
            .result
        #expect(
            reply["error"].asString
                == "delete_agent: 'locked2' blocked by delete_lock on 'locked2'; clear it via update_agent before deleting",
            "\(reply)")
        #expect(reply["hint"].asString == "clear the lock with update_agent, then delete again")
        let keys = reply.asObject?.keys.map { $0 } ?? []
        #expect(
            (keys.firstIndex(of: "blocked_by") ?? -1) < (keys.firstIndex(of: "id") ?? -1), "\(keys)")
    }

    /// python `if self._meta.get("delete_lock")` — any TRUTHY value locks the
    /// cascade, not only JSON `true`; a falsy one does not.
    @Test func deleteLockIsPythonTruthiness() async {
        let kernel = makeKernel()
        func deleteLocked(_ id: String, _ lock: JSON) async -> JSON {
            _ = await kernel.send([
                Item(
                    "kernel_state",
                    [
                        "type": "create_agent", "parent_id": "kernel_state",
                        "handler_module": "echo.tools", "id": .string(id), "delete_lock": lock,
                    ])
            ])[0].result
            return await kernel.send([
                Item("kernel_state", ["type": "delete_agent", "id": .string(id)])
            ])[0].result
        }
        let truthy: [JSON] = [1, "yes", "false", ["x"]]
        for (i, lock) in truthy.enumerated() {
            let reply = await deleteLocked("lk\(i)", lock)
            #expect(reply["reason"].asString == "locked", "\(lock.serialize()): \(reply)")
            #expect(kernel.agent(AgentId("lk\(i)")) != nil, "\(lock.serialize()) did not lock")
        }
        let falsy: [JSON] = [0, "", .array([]), false]
        for (i, lock) in falsy.enumerated() {
            let reply = await deleteLocked("ul\(i)", lock)
            #expect(reply["deleted"].asBool == true, "\(lock.serialize()): \(reply)")
        }
    }

    @Test func getIsNotAKernelVerb() async {
        // It was, in rust and swift only — python has no `get` and answers the
        // root bundle's UNHANDLED refusal. A record is read with `list_agents`
        // or a `reflect`, both of which every runtime has.
        let kernel = makeKernel()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "echo.tools",
                "id": "g1",
            ])])[0].result
        let reply = await kernel.send([Item("kernel_state", ["type": "get", "id": "g1"])])[0].result
        #expect(reply["reason"].asString == "unhandled", "\(reply.serialize())")
        #expect(reply["verb"].asString == "get", "the refusal names the verb")
        #expect(reply["id"].asString == nil, "it must not answer with a record")
    }
}
