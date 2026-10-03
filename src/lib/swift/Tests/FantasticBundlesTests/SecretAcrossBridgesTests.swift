// SECRET MARKS CROSS EVERY BRIDGE (owner-approved, 2026-10-02).
//
// A batch item's `secret` marks (`Secret.swift`) used to stop at the edge of the
// kernel that held them: a call frame carried no marks, so the RECEIVING kernel
// recorded the value verbatim (call/settle rows, ledger, state stream, memory
// store), and a credential store on the far side refused the call outright. Now
// the marks ride the call:
//
//   * a ws / bridge `call` frame carries `secret: [marks]` beside `payload` (each a
//     dotted string or an array of steps, as given), and
//     every inbound leg (web_ws, the io_bridge engine serving ws_bridge peers)
//     puts it on the dispatched item verbatim;
//   * web_rest reads it from `X-Fantastic-Secret`, a JSON array (both the JSON
//     leg and the host's octet leg);
//   * outbound, a `<guid>|<agent>` send hands its marks to the peer route, and
//     `forward` re-roots the marks on its own call, in each mark's spelling
//     (`payload.payload.<f>` → `payload.<f>`, `["payload","payload",<s>…]` →
//     `["payload",<s>…]`, a `result` mark as is).
//
// A RELAY leg is SEALED for secrets (owner, 2026-10-02): relayd records the frames
// it routes unmasked, so `relay_connector` refuses any call carrying marks,
// outbound (`forward`, text or binary) and inbound (a tunneled `call` frame with
// `secret`) — section (g).
//
// Twin of python `io_bridge/tests/test_secret_across_bridges.py`. Two in-process
// kernels per test, each with a memory store, so every record a kernel keeps is
// checked. Bridge pairs run the ENGINE on both ends (a paired in-process
// transport through the wire codec, or the relay transport over an in-process
// hub); the web legs are served over a real loopback socket.

import FantasticCredentialStore
import FantasticIoBridge
import FantasticJSON
import FantasticKernelState
import FantasticRamStore
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel
@testable import FantasticKernelBridge

private let KEY = "sk-cross-0123456789abcdef"
private let KEY2 = "sk-cross-dotted-key-77aa77aa"
private let ISSUED = "tok-cross-issued-fedcba98"
private let PLACEHOLDER = Secret.placeholder

// ─── the probe the far kernel serves ─────────────────────────────────────────

private final class Vault: AgentBundle, @unchecked Sendable {
    let name = "xsecret_probe"
    private let lock = NSLock()
    private var _got: [JSON] = []
    private var _blobs: [Data] = []
    var got: [JSON] { lock.withLock { _got } }
    var blobs: [Data] { lock.withLock { _blobs } }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        if ["boot", "shutdown", "reflect"].contains(verb) {
            return .object([
                "id": .string(agentId.value), "sentence": .string("cross-kernel secret probe"),
                "verbs": .object([:]),
            ])
        }
        lock.withLock { _got.append(payload) }
        switch verb {
        case "set_key":
            let key = payload["key"].asString ?? ""
            return .object(["ok": .bool(true), "len": .integer(Int64(key.count))])
        case "issue":
            return .object(["key": .string(ISSUED), "note": .string("issued")])
        case "issue_dotted":
            return ["a.b": .string(ISSUED), "a": ["b": "nested-plain"]]
        default:
            return .object(["error": .string("unknown verb \(pyRepr(.string(verb)))")])
        }
    }

    /// A body arrives at the payload's `bytes` — where python's dict holds it — on
    /// the binary channel (an octet REST call, a binary forward).
    func handleBinary(agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel) async throws
        -> (JSON?, Blob?)
    {
        // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
        let blob = bodyAt(body, "bytes") ?? Data()
        guard header["type"].asString == "keep" else {
            return (try await handle(agentId: agentId, payload: header, kernel: kernel), nil)
        }
        lock.withLock {
            _got.append(header)
            _blobs.append(blob)
        }
        return (.object(["kept": .integer(Int64(blob.count))]), nil)
    }
}

// ─── two kernels, each keeping every record ──────────────────────────────────

/// Every state event a kernel publishes.
private final class Tap: @unchecked Sendable {
    private let lock = NSLock()
    private var _all: [JSON] = []
    func add(_ e: JSON) { lock.withLock { _all.append(e) } }
    var all: [JSON] { lock.withLock { _all } }
}

private struct K {
    let k: Kernel
    let store: String
    let tap: Tap
    let ws: KernelBridgeBundle
    let relay: KernelBridgeBundle
    var guid: String { k.root?.metaValue(forKey: "guid")?.asString ?? "" }
}

private func op(
    _ k: Kernel, _ target: String, _ payload: JSON, secret: JSON? = nil
) async -> Outcome {
    var it = Item(AgentId(target), payload)
    it.secret = secret
    return await k.send([it])[0]
}

private func paths(_ p: String...) -> JSON { .array(p.map { .string($0) }) }

/// Python `_kernel`: a root with `guid`, a ram_store named its memory store,
/// started, and a tap on its state stream.
private func mkKernel(guid: String, store: String, vault: Vault) async -> K {
    // Only the bundles these tests reach — never the real keychain.
    let r = BundleRegistry()
    r.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    r.register(RamStoreBundle.handlerModule, RamStoreBundle())
    r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: MemKeychain()))
    r.register("ws_bridge.tools", KernelBridgeBundle(family: .ws))
    r.register("relay_connector.tools", KernelBridgeBundle(family: .relay))
    r.register("web.tools", WebBundle())
    r.register("web_ws.tools", WebWSBundle())
    r.register("web_rest.tools", WebRestBundle())
    r.register("xsecret_probe.tools", vault)
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: ROOT_HANDLER_MODULE, parentId: nil)
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": .string(guid)])
    let made = await op(
        k, "kernel",
        .object([
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": "ram_store.tools",
            "id": .string(store), "operator": true, "admins": ["kernel"],
        ]))
    #expect(made.status == .completed, "\(made.result.serialize())")
    k.update(root.id, ["memory_store_id": .string(store)])
    let started = await k.start(bootAll: false)
    #expect(started["started"].asBool == true, "\(started.serialize())")
    let tap = Tap()
    k.subscribe { ev, _ in tap.add(ev) }
    return K(
        k: k, store: store, tap: tap,
        ws: r.get("ws_bridge.tools") as! KernelBridgeBundle,
        relay: r.get("relay_connector.tools") as! KernelBridgeBundle)
}

/// Python `_records`: everything the kernel KEEPS of its calls — the rows in RAM,
/// the ledger projection, every state event, then (after a flush) the memory
/// store.
private struct Records {
    let rows: [JSON]
    let ledger: JSON
    let events: [JSON]
    let store: JSON

    /// Every execution row the kernel kept, wherever it is now: the RAM rows
    /// plus the rows the started kernel's own flush loop already moved into the
    /// store (it runs on its own clock, so a row may be in either).
    var kept: [JSON] {
        var seen: Set<Int64> = []
        var out: [JSON] = []
        let stored = (store["rows"].asArray ?? []).map { $0["row"] }
        for r in rows + stored {
            if let id = r["id"].asInt, !seen.insert(Int64(id)).inserted { continue }
            out.append(r)
        }
        return out
    }
}

private func records(_ x: K) async -> Records {
    let rows = x.k.rows.all().map { JSON.object($0) }
    let ledger = await op(x.k, "kernel", ["type": "ledger"]).result
    await x.k.flushExecutions()
    let tables = await op(x.k, x.store, ["type": "dump"]).result["tables"]
    return Records(rows: rows, ledger: ledger, events: x.tap.all, store: tables)
}

private func keptOut(_ x: K, _ r: Records, _ values: String...) {
    let places: [(String, JSON)] = [
        ("rows", .array(r.rows)), ("ledger", r.ledger), ("events", .array(r.events)),
        ("store", r.store),
    ]
    for (place, data) in places {
        for v in values {
            #expect(!data.serialize().contains(v), "\(x.guid): \(v) in \(place)")
        }
    }
}

private func callRow(_ r: Records, _ agent: String, _ verb: String) -> JSON {
    let rows = r.kept.filter {
        $0["kind"].asString == "call" && $0["agent"].asString == agent
            && $0["verb"].asString == verb
    }
    #expect(rows.count == 1, "\(JSON.array(r.kept).serialize())")
    return rows.first ?? .null
}

private func settleOf(_ r: Records, _ callId: JSON) -> JSON {
    let rows = r.kept.filter { $0["kind"].asString == "settle" && $0["parent"] == callId }
    #expect(rows.count == 1, "\(JSON.array(r.kept).serialize())")
    return rows.first ?? .null
}

private func vaultOn(_ x: K, callers: [String]) async {
    var grants: OrderedDictionary<String, JSON> = [:]
    for c in callers { grants[c] = "*" }
    let made = await op(
        x.k, "kernel",
        .object([
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "xsecret_probe.tools", "id": "vault", "callable_by": .object(grants),
        ]))
    #expect(made.result["id"].asString == "vault", "\(made.result.serialize())")
}

private func create(_ x: K, _ fields: OrderedDictionary<String, JSON>) async -> String {
    var p: OrderedDictionary<String, JSON> = ["type": "create_agent"]
    for (k, v) in fields { p[k] = v }
    let out = await op(x.k, "kernel", .object(p))
    #expect(out.status == .completed, "\(out.result.serialize())")
    return out.result["id"].asString ?? ""
}

// ─── the transports a bridge pair rides ─────────────────────────────────────

/// One end of a PAIRED in-process transport — python `MemoryTransport.pair()`:
/// what one end sends, the other receives, through the real wire codec (a text
/// frame is serialized and re-parsed strictly; a binary one is encoded
/// `[4B len|header|body]` and decoded) — where `MemoryTransport.pair()` hands
/// each frame across as it is. Both ends are driven by the bridge ENGINE.
final class PairEnd: BridgeTransport, @unchecked Sendable {
    private let lock = NSLock()
    private var queue: [(JSON, Data, Bool)] = []
    private var waiter: CheckedContinuation<(JSON, Data, Bool)?, Never>?
    private var closed = false
    fileprivate var peer: PairEnd?

    static func pair() -> (PairEnd, PairEnd) {
        let a = PairEnd()
        let b = PairEnd()
        a.peer = b
        b.peer = a
        return (a, b)
    }

    private func isShut() -> Bool { lock.withLock { closed } }

    func isClosed() async -> Bool { isShut() }

    func send(_ frame: JSON) async throws {
        if isShut() { throw ConnectionClosed("pair transport closed") }
        peer?.deliver((try JSON.parseStrict(Codec.encodeText(frame)), Data(), false))
    }

    func sendBinary(header: JSON, body: Data) async throws {
        if isShut() { throw ConnectionClosed("pair transport closed") }
        let (h, b) = try Codec.decodeBinaryFrame(Codec.encodeBinaryFrame(header: header, body: body))
        peer?.deliver((h, b, true))
    }

    fileprivate func deliver(_ f: (JSON, Data, Bool)) {
        let w: CheckedContinuation<(JSON, Data, Bool)?, Never>? = lock.withLock {
            if closed { return nil }
            if let w = waiter {
                waiter = nil
                return w
            }
            queue.append(f)
            return nil
        }
        w?.resume(returning: f)
    }

    func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        let next: (JSON, Data, Bool)? = await withCheckedContinuation { c in
            lock.lock()
            if !queue.isEmpty {
                let head = queue.removeFirst()
                lock.unlock()
                c.resume(returning: head)
                return
            }
            if closed {
                lock.unlock()
                c.resume(returning: nil)
                return
            }
            waiter = c
            lock.unlock()
        }
        guard let next else { throw ConnectionClosed("pair transport closed") }
        return next
    }

    fileprivate func shut() {
        let w: CheckedContinuation<(JSON, Data, Bool)?, Never>? = lock.withLock {
            closed = true
            let w = waiter
            waiter = nil
            return w
        }
        w?.resume(returning: nil)
    }

    func close() async {
        shut()
        peer?.shut()
    }
}

/// One peer's socket against the in-process relay hub (python `_testkit_relay`).
private actor HubWire: RelayWire {
    let guid: String
    private let hub: RelayHub
    private var inbox: [NIOWebSocketClient.Message] = []
    private var waiters: [CheckedContinuation<NIOWebSocketClient.Message, Error>] = []
    private var closed = false

    init(guid: String, hub: RelayHub) {
        self.guid = guid
        self.hub = hub
    }

    func send(_ message: NIOWebSocketClient.Message) async throws {
        await hub.route(from: guid, message)
    }

    func receive() async throws -> NIOWebSocketClient.Message {
        if !inbox.isEmpty { return inbox.removeFirst() }
        if closed { throw NIOWebSocketClient.Closed(reason: "hub wire closed") }
        return try await withCheckedThrowingContinuation { waiters.append($0) }
    }

    func deliver(_ m: NIOWebSocketClient.Message) {
        if !waiters.isEmpty { waiters.removeFirst().resume(returning: m) } else { inbox.append(m) }
    }

    func close() async {
        closed = true
        let w = waiters
        waiters.removeAll()
        for c in w { c.resume(throwing: NIOWebSocketClient.Closed(reason: "hub wire closed")) }
    }
}

/// The relay kernel's routing, minus the relay: a `send` to a guid is delivered
/// to that peer as `{type:"event", source, payload}`, in the same frame kind.
private actor RelayHub {
    private var wires: [String: HubWire] = [:]

    func register(_ guid: String) -> HubWire {
        let w = HubWire(guid: guid, hub: self)
        wires[guid] = w
        return w
    }

    func route(from sender: String, _ message: NIOWebSocketClient.Message) async {
        switch message {
        case .text(let s):
            guard let env = try? JSON.parse(s), env["type"].asString == "send",
                let target = env["target"].asString
            else { return }
            let event: JSON = .object([
                "type": "event", "source": .string(sender), "payload": env["payload"],
            ])
            await wires[target]?.deliver(.text(event.serialize()))
        case .binary(let bytes):
            guard let (env, body) = try? Codec.decodeBinaryFrame(Data(bytes)),
                env["type"].asString == "send", let target = env["target"].asString
            else { return }
            var event: JSON = .object([
                "type": "event", "source": .string(sender), "payload": env["payload"],
            ])
            if let p = env.asObject?["_binary_path"] { event["_binary_path"] = p }
            await wires[target]?.deliver(
                .binary([UInt8](Codec.encodeBinaryFrame(header: event, body: body))))
        }
    }
}

enum BridgeKind: String, CaseIterable, Sendable {
    case ws = "ws_bridge.tools"
    case relay = "relay_connector.tools"
}

/// Python `_bridge_pair`: A's leg dials, B's leg SERVES (pinned to A, open,
/// reaching `vault`). Same engine for both bundles; the relay tunnels the frames
/// through the hub. Returns the two leg ids.
@discardableResult
private func bridgePair(_ a: K, _ b: K, _ kind: BridgeKind) async -> (String, String) {
    let aLeg = await create(
        a,
        [
            "parent_id": "kernel_state", "handler_module": .string(kind.rawValue),
            "id": "a_leg", "transport": "memory",
        ])
    let bLeg = await create(
        b,
        [
            "parent_id": "kernel_state", "handler_module": .string(kind.rawValue),
            "id": "b_leg", "transport": "memory", "peer_guid": .string(a.guid),
            "ingress_rule": "allow_all_ingress", "can_call": ["vault": "*"],
        ])
    switch kind {
    case .ws:
        let (ta, tb) = PairEnd.pair()
        a.ws.injectTransport(agentId: AgentId(aLeg), transport: ta)
        b.ws.injectTransport(agentId: AgentId(bLeg), transport: tb)
    case .relay:
        let hub = RelayHub()
        let wa = await hub.register("RA")
        let wb = await hub.register("RB")
        a.relay.injectTransport(
            agentId: AgentId(aLeg),
            transport: await RelayTransport.attach(wire: wa, partnerGuid: "RB"))
        b.relay.injectTransport(
            agentId: AgentId(bLeg),
            transport: await RelayTransport.attach(wire: wb, partnerGuid: "RA"))
    }
    for (x, leg) in [(a, aLeg), (b, bLeg)] {
        let booted = await op(x.k, leg, ["type": "boot"]).result
        #expect(booted["booted"].asBool == true, "\(booted.serialize())")
    }
    return (aLeg, bLeg)
}

private func dropLeg(_ x: K, _ leg: String) async {
    _ = await op(x.k, "kernel", .object(["type": "delete_agent", "id": .string(leg)]))
}

private func forward(
    _ x: K, _ leg: String, _ inner: JSON, secret: JSON? = nil
) async -> Outcome {
    await op(
        x.k, leg, .object(["type": "forward", "target": "vault", "payload": inner]), secret: secret)
}

private func twoKernels() async -> (K, K, Vault) {
    let vault = Vault()
    let a = await mkKernel(guid: "A-g", store: "a_mem", vault: Vault())
    let b = await mkKernel(guid: "B-g", store: "b_mem", vault: vault)
    return (a, b, vault)
}

// ─── the web legs, over a real loopback socket ──────────────────────────────

/// A `web` host on a free port with ONE open leg mounted under it. Returns the
/// bound port.
private func serveWeb(
    _ x: K, leg: String, module: String, canCall: JSON? = nil
) async -> UInt16 {
    _ = await create(
        x,
        [
            "parent_id": "kernel_state", "handler_module": "web.tools", "id": "web",
            "port": .integer(Int64(freeTCPPort())),
        ])
    _ = await op(x.k, "web", ["type": "boot"])
    var fields: OrderedDictionary<String, JSON> = [
        "parent_id": "web", "handler_module": .string(module), "id": .string(leg),
        "ingress_rule": "allow_all_ingress",
    ]
    if let canCall { fields["can_call"] = canCall }
    _ = await create(x, fields)
    _ = await op(x.k, "web", .object(["type": "mount", "child_id": .string(leg)]))
    return x.k.httpPort()
}

private func stopWeb(_ x: K) async {
    _ = await op(x.k, "web", ["type": "stop"])
}

private func socket(_ port: UInt16) -> URLSessionWebSocketTask {
    let task = URLSession(configuration: .ephemeral)
        .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/web/ws")!)
    task.resume()
    return task
}

/// The next frame of type `ftype`, bounded (see `PeerCallsTests.next`).
private func next(
    _ task: URLSessionWebSocketTask, _ ftype: String, seconds: UInt64 = 5
) async throws -> JSON {
    let timer = Task {
        try await Task.sleep(nanoseconds: seconds * 1_000_000_000)
        task.cancel(with: .goingAway, reason: nil)
    }
    defer { timer.cancel() }
    while true {
        let frame: JSON
        switch try await task.receive() {
        case .string(let s): frame = try JSON.parse(s)
        case .data(let d): frame = try Codec.decodeBinaryFrame(d).0
        @unknown default: continue
        }
        if frame["type"].asString == ftype { return frame }
    }
}

/// A bridge transport that keeps every inbound `call` frame — the TEST's
/// capture of what crossed the wire, not anything a kernel keeps.
private final class Recording: BridgeTransport, @unchecked Sendable {
    let inner: any BridgeTransport
    private let lock = NSLock()
    private var _calls: [JSON] = []
    init(_ inner: any BridgeTransport) { self.inner = inner }
    private func keep(_ f: JSON) { lock.withLock { _calls.append(f) } }
    var calls: [JSON] { lock.withLock { _calls } }
    func isClosed() async -> Bool { await inner.isClosed() }
    func send(_ frame: JSON) async throws { try await inner.send(frame) }
    func sendBinary(header: JSON, body: Data) async throws {
        try await inner.sendBinary(header: header, body: body)
    }
    func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        let r = try await inner.recv()
        if r.frame["type"].asString == "call" { keep(r.frame) }
        return r
    }
    func close() async { await inner.close() }
}

/// python `_until`: poll `pred` for up to 3 s.
private func until(_ what: String, _ pred: () -> Bool) async throws {
    let end = Date().addingTimeInterval(3)
    while !pred() {
        if Date() >= end {
            Issue.record("\(what): not reached")
            return
        }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
}

/// Python `(f)`: nothing on the way printed or logged a value — every runtime
/// WARN line and every operator stdout line emitted while `body` ran.
private func printedNothing(_ values: [String], _ body: () async throws -> Void) async rethrows {
    var warned: [String] = []
    let out = try await RuntimeLog.captureOut {
        warned = try await RuntimeLog.capture { try await body() }
    }
    for v in values {
        #expect(!warned.joined().contains(v), "\(v) on the WARN channel")
        #expect(!out.joined().contains(v), "\(v) on stdout")
    }
}

/// The relay seal's refusal (python `_SEALED`).
private let SEALED: JSON = [
    "error": .string(SECRET_SEALED), "reason": "unauthorized", "hint": .string(SECRET_SEALED_HINT),
]

private let BAD: [(JSON, String)] = [
    (.string("payload.key"), "must be a non-empty list of paths"),
    (.array([]), "must be a non-empty list of paths"),
    (paths("payload.nope"), "names nothing in the payload"),
    (paths("payload.type"), "cannot hide payload.type"),
    (paths("payload"), "must start with 'payload.'"),
    // arrays of steps
    ([[]], "path [] has no steps"),
    ([["payload", true]], "step True is neither a string"),
    ([["nope", "key"]], "must start with 'payload' and a step under it"),
    ([["payload", "type"]], "cannot hide payload.type"),
    ([["payload", "nope"]], "path ['payload', 'nope'] names nothing in the payload"),
    (["payload.key", ["payload", "k.x"]], "names nothing in the payload"),
]

// ─── the tests ──────────────────────────────────────────────────────────────

@Suite("secret marks across bridges", .serialized)
struct SecretAcrossBridgesTests {

    // ─── the forward mapping ────────────────────────────────────────────────

    @Test func forwardReRootsTheMarksAtTheForwardedItem() {
        #expect(
            forwardedSecret([
                "payload.payload.key", "payload.payload.fields.api_key", "result", "result.key",
                "payload.target", "payload.timeout", "payload.payload",
            ]) == ["payload.key", "payload.fields.api_key", "result", "result.key", "payload"])
        #expect(forwardedSecret([]) == [])
        // an ARRAY of steps re-roots as an array: one `payload` step dropped
        #expect(
            forwardedSecret([
                ["payload", "payload", "a.b"],
                ["payload", "payload", "items", 0],
                ["result"],
                ["result", "a.b"],
                ["payload", "target"],
                ["payload", "timeout"],
                ["payload", "payload"],
                "payload.payload.key",
            ]) == [
                ["payload", "a.b"],
                ["payload", "items", 0],
                ["result"],
                ["result", "a.b"],
                ["payload"],
                "payload.key",
            ])
    }

    // ─── (a) a payload mark across a bridge ─────────────────────────────────

    // relay_connector is SEALED for secrets — see (g).
    @Test(.timeLimit(.minutes(1)), arguments: [BridgeKind.ws])
    func aMarkedPayloadCrossesABridgeAndBothKernelsMaskIt(_ kind: BridgeKind) async {
        let (a, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, kind)
        let out = await forward(
            a, aLeg,
            ["type": "set_key", "ref": "openai", "key": .string(KEY)],
            secret: paths("payload.payload.key"))
        // the far TARGET got the value; the CALLER the real reply
        #expect(vault.got == [["type": "set_key", "ref": "openai", "key": .string(KEY)]])
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == ["ok": true, "len": .integer(Int64(KEY.count))])
        // the RECEIVING kernel: its call row is masked and names the paths
        let rb = await records(b)
        let row = callRow(rb, "vault", "set_key")
        #expect(
            row["payload"] == ["type": "set_key", "ref": "openai", "key": .string(PLACEHOLDER)],
            "\(row.serialize())")
        #expect(row["secret"] == paths("payload.key"), "\(row.serialize())")
        #expect(rb.store.serialize().contains(PLACEHOLDER), "the store got the masked row")
        keptOut(b, rb, KEY)
        // the calling kernel masks its own record of the forward
        let ra = await records(a)
        let fwd = callRow(ra, aLeg, "forward")
        #expect(fwd["payload"]["payload"]["key"].asString == PLACEHOLDER, "\(fwd.serialize())")
        keptOut(a, ra, KEY)
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    @Test(.timeLimit(.minutes(1)))
    func aBridgeDialingWebWsCarriesTheMarks() async throws {
        // The production ws topology: A's ws_bridge dials B's web_ws leg.
        let (a, b, _) = await twoKernels()
        let port = await serveWeb(b, leg: "b_ws", module: "web_ws.tools", canCall: ["vault": "*"])
        await vaultOn(b, callers: ["b_ws"])
        let aLeg = await create(
            a,
            [
                "parent_id": "kernel_state", "handler_module": "ws_bridge.tools", "id": "a_leg",
                "transport": "ws", "host": "127.0.0.1", "port": .integer(Int64(port)),
                "peer_id": "web",
            ])
        await printedNothing([ISSUED]) {
            // `create_agent` already booted the leg — it dialed B's socket
            let booted = await op(a.k, aLeg, ["type": "boot"]).result
            #expect(booted["transport"].asString == "ws", "\(booted.serialize())")
            let out = await forward(a, aLeg, ["type": "issue"], secret: paths("result.key"))
            #expect(out.result == ["key": .string(ISSUED), "note": "issued"], "\(out.result.serialize())")
            // the frame web_ws received named the call — the settle keeps its
            // whole result — and carried the mark, so B records the placeholder
            let rb = await records(b)
            let row = callRow(rb, "vault", "issue")
            #expect(row["secret"] == paths("result.key"), "\(row.serialize())")
            #expect(row["named"] == .bool(true), "\(row.serialize())")
            #expect(
                settleOf(rb, row["id"])["outcome"]["result"]
                    == ["key": .string(PLACEHOLDER), "note": "issued"])
            keptOut(b, rb, ISSUED)
            keptOut(a, await records(a), ISSUED)
        }
        await dropLeg(a, aLeg)
        await stopWeb(b)
    }

    // ─── (b) a result mark masks the RECEIVING settle row ───────────────────

    // relay_connector is SEALED for secrets — see (g).
    @Test(.timeLimit(.minutes(1)), arguments: [BridgeKind.ws])
    func aResultMarkMasksTheReceivingSettleRow(_ kind: BridgeKind) async {
        let (a, b, _) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, kind)
        let whole = await forward(a, aLeg, ["type": "issue"], secret: paths("result"))
        #expect(whole.result == ["key": .string(ISSUED), "note": "issued"])
        let rb = await records(b)
        let row = callRow(rb, "vault", "issue")
        #expect(row["secret"] == paths("result"), "\(row.serialize())")
        // the frame id NAMES the far execution, so its settle keeps the whole
        // result — the one place the value would have landed
        #expect(settleOf(rb, row["id"])["outcome"]["result"] == .string(PLACEHOLDER))
        keptOut(b, rb, ISSUED)
        keptOut(a, await records(a), ISSUED)
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    /// A binary `forward`'s chunk sits INSIDE the forwarded payload, at
    /// `payload.payload.bytes` — where python's dict holds it — so the caller marks
    /// it there, exactly as in python, and the mark re-roots like any other field:
    /// the frame carries it as the far item's `payload.bytes`, its body. Both
    /// kernels record `<secret>` in the body's slot. relay_connector is SEALED for
    /// secrets — see (g).
    @Test(.timeLimit(.minutes(1)), arguments: [BridgeKind.ws])
    func aMarkedBlobCrossesABinaryForward(_ kind: BridgeKind) async {
        let (a, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, kind)
        var it = Item(
            AgentId(aLeg),
            ["type": "forward", "target": "vault", "payload": ["type": "keep", "path": "k"]],
            binary: Blob(parts: ["payload", "bytes"], Data(KEY.utf8)))
        it.secret = paths("payload.payload.bytes")
        let out = await a.k.send([it])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == ["kept": .integer(Int64(KEY.count))], "\(out.result.serialize())")
        #expect(vault.blobs == [Data(KEY.utf8)], "the far target gets the real chunk")
        #expect(
            vault.got.map { $0.asObject.map { Array($0.keys) } ?? [] } == [["type", "path"]],
            "the far handler's header holds nothing at the body's slot: \(vault.got)")
        let rb = await records(b)
        let row = callRow(rb, "vault", "keep")
        #expect(
            row["payload"] == ["type": "keep", "path": "k", "bytes": .string(PLACEHOLDER)],
            "\(row.serialize())")
        #expect(row["secret"] == paths("payload.bytes"), "\(row.serialize())")
        keptOut(b, rb, KEY)
        let ra = await records(a)
        let fwd = callRow(ra, aLeg, "forward")
        #expect(
            fwd["payload"]["payload"]
                == ["type": "keep", "path": "k", "bytes": .string(PLACEHOLDER)],
            "\(fwd.serialize())")
        #expect(fwd["secret"] == paths("payload.payload.bytes"), "\(fwd.serialize())")
        keptOut(a, ra, KEY)
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    /// The control: what the marks prevent is real.
    @Test(.timeLimit(.minutes(1)))
    func withoutAMarkTheReceivingSettleKeepsTheValue() async {
        let (a, b, _) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, .ws)
        _ = await forward(a, aLeg, ["type": "issue"])
        let rb = await records(b)
        let row = callRow(rb, "vault", "issue")
        #expect(row["secret"].isNull, "\(row.serialize())")
        #expect(settleOf(rb, row["id"])["outcome"]["result"]["key"].asString == ISSUED)
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    // ─── (b2) an ARRAY mark (a literal dotted key) across a bridge ──────────

    /// `["payload", "payload", "a.b"]` on the forward rides as `["payload",
    /// "a.b"]`: the far kernel masks the KEY `"a.b"` (not `a` → `b`), and records
    /// the marks as the frame carried them — arrays and strings side by side.
    @Test(.timeLimit(.minutes(1)))
    func anArrayMarkMasksALiteralDottedKeyAcrossAWsBridge() async {
        let (a, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, .ws)
        let sent: JSON = [
            "type": "issue_dotted", "key": .string(KEY), "a.b": .string(KEY2),
            "a": ["b": "nested-plain"],
        ]
        let out = await forward(
            a, aLeg, sent,
            secret: ["payload.payload.key", ["payload", "payload", "a.b"], ["result", "a.b"]])
        #expect(vault.got == [sent], "\(JSON.array(vault.got).serialize())")
        #expect(
            out.result == ["a.b": .string(ISSUED), "a": ["b": "nested-plain"]],
            "\(out.result.serialize())")
        let rb = await records(b)
        let row = callRow(rb, "vault", "issue_dotted")
        #expect(
            row["secret"] == ["payload.key", ["payload", "a.b"], ["result", "a.b"]],
            "\(row.serialize())")
        #expect(
            row["payload"]
                == [
                    "type": "issue_dotted", "key": .string(PLACEHOLDER), "a.b": .string(PLACEHOLDER),
                    "a": ["b": "nested-plain"],
                ], "\(row.serialize())")
        // the frame id names the far execution: its settle keeps the whole result
        #expect(
            settleOf(rb, row["id"])["outcome"]["result"]
                == ["a.b": .string(PLACEHOLDER), "a": ["b": "nested-plain"]])
        keptOut(b, rb, KEY, KEY2, ISSUED)
        let ra = await records(a)
        let fwd = callRow(ra, aLeg, "forward")
        #expect(fwd["payload"]["payload"]["a.b"].asString == PLACEHOLDER, "\(fwd.serialize())")
        #expect(fwd["payload"]["payload"]["a"] == ["b": "nested-plain"], "\(fwd.serialize())")
        keptOut(a, ra, KEY, KEY2, ISSUED)
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    // ─── (c) a malformed mark is refused before dispatch ────────────────────

    @Test(.timeLimit(.minutes(1)))
    func aMalformedMarkOnABridgeFrameIsRefusedBeforeDispatch() async throws {
        let (_, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let bLeg = await create(
            b,
            [
                "parent_id": "kernel_state", "handler_module": "ws_bridge.tools", "id": "b_leg",
                "transport": "memory", "peer_guid": "A-g", "ingress_rule": "allow_all_ingress",
                "can_call": ["vault": "*"],
            ])
        let (peer, mine) = PairEnd.pair()
        b.ws.injectTransport(agentId: AgentId(bLeg), transport: mine)
        #expect(await op(b.k, bLeg, ["type": "boot"]).result["booted"].asBool == true)
        for (n, (secret, why)) in BAD.enumerated() {
            try await peer.send(
                .object([
                    "type": "call", "id": .string("bad-\(n)"), "target": "vault",
                    "payload": ["type": "set_key", "key": .string(KEY)], "caller": "A-g|kernel",
                    "secret": secret,
                ]))
            let data = try await peer.recv().frame["data"]
            #expect(data["reason"].asString == "bad_arg", "\(secret.serialize()) \(data.serialize())")
            #expect(data["arg"].asString == "secret", "\(data.serialize())")
            #expect(data["error"].asString?.contains(why) == true, "\(data.serialize())")
            #expect(data["error"].asString?.contains(KEY) == false, "\(data.serialize())")
        }
        #expect(vault.got.isEmpty, "a refused item must not reach the handler")
        let rb = await records(b)
        // (its `create_agent` boot is the only row the vault has)
        #expect(
            !rb.kept.contains { $0["agent"].asString == "vault" && $0["verb"].asString != "boot" },
            "\(JSON.array(rb.kept).serialize())")
        keptOut(b, rb, KEY)
        await dropLeg(b, bLeg)
    }

    @Test(.timeLimit(.minutes(1)))
    func aMalformedMarkOnAWebWsFrameIsRefusedBeforeDispatch() async throws {
        let (_, b, vault) = await twoKernels()
        let port = await serveWeb(b, leg: "b_ws", module: "web_ws.tools", canCall: ["vault": "*"])
        await vaultOn(b, callers: ["b_ws"])
        let ws = socket(port)
        for (n, (secret, why)) in BAD.enumerated() {
            let frame: JSON = .object([
                "type": "call", "id": .string("bad-\(n)"), "target": "vault",
                "payload": ["type": "set_key", "key": .string(KEY)], "secret": secret,
            ])
            try await ws.send(.string(frame.serialize()))
            let data = try await next(ws, "reply")["data"]
            #expect(data["reason"].asString == "bad_arg", "\(secret.serialize()) \(data.serialize())")
            #expect(data["arg"].asString == "secret", "\(data.serialize())")
            #expect(data["error"].asString?.contains(why) == true, "\(data.serialize())")
        }
        #expect(vault.got.isEmpty)
        ws.cancel(with: .normalClosure, reason: nil)
        await stopWeb(b)
    }

    @Test(
        .timeLimit(.minutes(1)),
        arguments: [
            ("payload.payload", "must start with 'payload.'"),
            ("payload.payload.type", "cannot hide payload.type"),
            (["payload", "payload"], "must start with 'payload' and a step under it"),
            (["payload", "payload", "type"], "cannot hide payload.type"),
        ] as [(JSON, String)])
    func aForwardMarkThatMapsToNothingValidIsTheFarKernelsRefusal(_ c: (JSON, String)) async {
        let (mark, why) = c
        let (a, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, .ws)
        let out = await forward(
            a, aLeg, ["type": "set_key", "key": .string(KEY)], secret: .array([mark]))
        let r = out.result
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        #expect(r["arg"].asString == "secret", "\(r.serialize())")
        #expect(r["error"].asString?.contains(why) == true, "\(r.serialize())")
        #expect(vault.got.isEmpty)
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    // ─── (d) web_rest: the X-Fantastic-Secret header ────────────────────────

    @Test(.timeLimit(.minutes(1)))
    func webRestCarriesTheMarksInAHeader() async throws {
        let (_, b, vault) = await twoKernels()
        let port = await serveWeb(b, leg: "b_rest", module: "web_rest.tools", canCall: ["vault": "*"])
        await vaultOn(b, callers: ["b_rest"])
        let session = URLSession(configuration: .ephemeral)
        func post(
            _ body: String, secret: String?, execId: String? = nil
        ) async throws -> (Int, JSON) {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/b_rest/vault")!)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            if let secret { req.setValue(secret, forHTTPHeaderField: "X-Fantastic-Secret") }
            if let execId { req.setValue(execId, forHTTPHeaderField: "X-Fantastic-Execution-Id") }
            req.httpBody = Data(body.utf8)
            let (d, resp) = try await session.data(for: req)
            return ((resp as? HTTPURLResponse)?.statusCode ?? 0, (try? JSON.parse(d)) ?? .null)
        }
        try await printedNothing([KEY, KEY2, ISSUED]) {
            let setBody = "{\"type\":\"set_key\",\"key\":\"\(KEY)\"}"
            let (s1, r1) = try await post(setBody, secret: "[\"payload.key\"]")
            #expect(s1 == 200 && r1 == ["ok": true, "len": .integer(Int64(KEY.count))], "\(r1)")
            let (_, r2) = try await post(
                "{\"type\":\"issue\"}", secret: "[\"result.key\"]", execId: "issue-rest-1")
            #expect(r2 == ["key": .string(ISSUED), "note": "issued"], "\(r2)")
            // an element may be an ARRAY of steps: the key "a.b", taken literally
            let (_, r5) = try await post(
                "{\"type\":\"issue_dotted\",\"a.b\":\"\(KEY2)\",\"a\":{\"b\":\"nested-plain\"}}",
                secret: #"[["payload","a.b"],["result","a.b"]]"#, execId: "issue-rest-2")
            #expect(r5 == ["a.b": .string(ISSUED), "a": ["b": "nested-plain"]], "\(r5)")
            // a malformed array is the kernel's own refusal, in its words
            let (_, r6) = try await post(setBody, secret: #"[["payload", "a.b"]]"#)
            #expect(
                r6
                    == [
                        "error": "send: secret path ['payload', 'a.b'] names nothing in the payload",
                        "reason": "bad_arg", "arg": "secret",
                    ], "\(r6)")
            // a header that is not JSON is the leg's own 400 — nothing dispatched
            let (s3, r3) = try await post(setBody, secret: "payload.key")
            #expect(s3 == 400, "\(r3)")
            #expect(r3 == ["error": "web_rest: X-Fantastic-Secret is not valid JSON"], "\(r3)")
            // JSON of the wrong shape is the kernel's own refusal
            let (_, r4) = try await post(setBody, secret: "\"payload.key\"")
            #expect(r4["reason"].asString == "bad_arg" && r4["arg"].asString == "secret", "\(r4)")
            #expect(
                vault.got
                    == [
                        ["type": "set_key", "key": .string(KEY)], ["type": "issue"],
                        ["type": "issue_dotted", "a.b": .string(KEY2), "a": ["b": "nested-plain"]],
                    ],
                "\(vault.got)")
            let rb = await records(b)
            #expect(callRow(rb, "vault", "set_key")["payload"]["key"].asString == PLACEHOLDER)
            let issue = callRow(rb, "vault", "issue")
            #expect(
                settleOf(rb, issue["id"])["outcome"]["result"]["key"].asString == PLACEHOLDER)
            let dotted = callRow(rb, "vault", "issue_dotted")
            #expect(
                dotted["secret"] == [["payload", "a.b"], ["result", "a.b"]], "\(dotted.serialize())")
            #expect(
                dotted["payload"]
                    == [
                        "type": "issue_dotted", "a.b": .string(PLACEHOLDER),
                        "a": ["b": "nested-plain"],
                    ], "\(dotted.serialize())")
            #expect(
                settleOf(rb, dotted["id"])["outcome"]["result"]
                    == ["a.b": .string(PLACEHOLDER), "a": ["b": "nested-plain"]])
            keptOut(b, rb, KEY, KEY2, ISSUED)
        }
        await stopWeb(b)
    }

    /// Swift's octet REST leg is the host's (a JSON reply cannot carry raw
    /// bytes); python's one `_rest_call` reads the header for both legs, so the
    /// host's octet leg reads it too — the blob is `payload.bytes`.
    @Test(.timeLimit(.minutes(1)))
    func theOctetRestLegCarriesTheMarksToo() async throws {
        let (_, b, vault) = await twoKernels()
        let port = await serveWeb(b, leg: "b_rest", module: "web_rest.tools", canCall: ["vault": "*"])
        await vaultOn(b, callers: ["b_rest"])
        let session = URLSession(configuration: .ephemeral)
        func post(secret: String) async throws -> (Int, JSON) {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/b_rest/vault")!)
            req.httpMethod = "POST"
            req.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
            req.setValue("{\"type\":\"keep\"}", forHTTPHeaderField: "X-Fantastic-Payload")
            req.setValue(secret, forHTTPHeaderField: "X-Fantastic-Secret")
            req.httpBody = Data(KEY.utf8)
            let (d, resp) = try await session.data(for: req)
            return ((resp as? HTTPURLResponse)?.statusCode ?? 0, (try? JSON.parse(d)) ?? .null)
        }
        let (s1, r1) = try await post(secret: "[\"payload.bytes\"]")
        #expect(s1 == 200 && r1 == ["kept": .integer(Int64(KEY.count))], "\(r1)")
        #expect(vault.blobs == [Data(KEY.utf8)], "the target gets the real body")
        let (s2, r2) = try await post(secret: "payload.bytes")
        #expect(s2 == 400 && r2 == ["error": "web_rest: X-Fantastic-Secret is not valid JSON"])
        #expect(vault.blobs.count == 1, "a 400 dispatches nothing")
        let rb = await records(b)
        #expect(callRow(rb, "vault", "keep")["payload"]["bytes"].asString == PLACEHOLDER)
        keptOut(b, rb, KEY)
        await stopWeb(b)
    }

    // ─── (e) <guid>|creds from another kernel ───────────────────────────────

    /// H serves a web_ws leg; P is a kernel connected to it (its ws_bridge leg
    /// dials the socket) and says `hello` — from then on H reaches P's agents as
    /// `P-g|<agent>`. P serves a credential_store that H may create refs in.
    @Test(.timeLimit(.minutes(1)))
    func aGuidQualifiedCredentialSetAndGetCrossWithTheirMarks() async throws {
        let h = await mkKernel(guid: "H-g", store: "h_mem", vault: Vault())
        let p = await mkKernel(guid: "P-g", store: "p_mem", vault: Vault())
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("xsecret-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let credsFile = dir.appendingPathComponent("creds.yaml")
        let port = await serveWeb(h, leg: "h_ws", module: "web_ws.tools")
        let pLeg = await create(
            p,
            [
                "parent_id": "kernel_state", "handler_module": "ws_bridge.tools", "id": "p_leg",
                "transport": "memory", "peer_guid": "H-g", "ingress_rule": "allow_all_ingress",
                "can_call": ["p_creds": "*"],
            ])
        _ = await create(
            p,
            [
                "parent_id": "kernel_state", "handler_module": "credential_store.tools",
                "id": "p_creds", "file": .string(credsFile.path),
                "refs": [
                    "openai/main": [
                        "host": "api.openai.com", "readers": ["H-g|kernel"], "writers": ["H-g|kernel"],
                    ]
                ],
                "callable_by": ["H-g|kernel": "*"],
            ])
        let client = try await NIOWebSocketClient.connect(
            url: URL(string: "ws://127.0.0.1:\(port)/web/ws")!)
        let sock = Recording(WSTransport(client: client, sealed: false))
        try await sock.send(["type": "hello", "guid": "P-g"])
        p.ws.injectTransport(agentId: AgentId(pLeg), transport: sock)
        #expect(await op(p.k, pLeg, ["type": "boot"]).result["booted"].asBool == true)
        try await until("P-g connected") { !h.k.peerRoutes("P-g").isEmpty }

        await printedNothing([KEY]) {
            let put = await op(
                h.k, "P-g|p_creds",
                [
                    "type": "set", "ref": "openai/main",
                    "fields": ["api_key": .string(KEY)],
                ], secret: paths("payload.fields"))
            #expect(put.status == .completed, "\(put.result.serialize())")
            #expect(
                put.result
                    == [
                        "ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"],
                        "stored": true,
                    ], "\(put.result.serialize())")
            let got = await op(
                h.k, "P-g|p_creds", ["type": "get", "ref": "openai/main"], secret: paths("result"))
            #expect(got.result["fields"] == ["api_key": .string(KEY)], "\(got.result.serialize())")
            // the frames H sent carried the item's marks verbatim
            #expect(
                sock.calls.map { $0["secret"] } == [paths("payload.fields"), paths("result")],
                "\(sock.calls.map { $0.serialize() })")
            // the value lives in P's backend, and in NO record on either side
            #expect(((try? String(contentsOf: credsFile, encoding: .utf8)) ?? "").contains(KEY))
            let rp = await records(p)
            #expect(
                callRow(rp, "p_creds", "set")["payload"]["fields"].asString == PLACEHOLDER)
            let getRow = callRow(rp, "p_creds", "get")
            #expect(settleOf(rp, getRow["id"])["outcome"]["result"] == .string(PLACEHOLDER))
            keptOut(p, rp, KEY)
            let rh = await records(h)
            #expect(
                callRow(rh, "P-g|p_creds", "set")["payload"]["fields"].asString == PLACEHOLDER)
            keptOut(h, rh, KEY)
            // an UNMARKED set from H is refused by P's store, as for a local caller
            let bare = await op(
                h.k, "P-g|p_creds",
                [
                    "type": "set", "ref": "openai/main",
                    "fields": ["api_key": "sk-unmarked-x"],
                ])
            #expect(bare.result["arg"].asString == "secret", "\(bare.result.serialize())")
        }
        await dropLeg(p, pLeg)
        await stopWeb(h)
    }

    // ─── (f) no leg, transport or bridge keeps or prints the unmasked frame ─

    // relay_connector is SEALED for secrets — see (g).
    @Test(.timeLimit(.minutes(1)), arguments: [BridgeKind.ws])
    func noLegOrBridgePrintsLogsOrMirrorsTheValue(_ kind: BridgeKind) async throws {
        let (a, b, _) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, kind)
        // watchers on every agent the call passes: both legs and the far target
        let spies: [(K, String)] = [(a, aLeg), (b, bLeg), (b, "vault")]
        var inboxes: [Inbox] = []
        for (x, aid) in spies {
            inboxes.append(x.k.ensureInbox(AgentId("spy_\(aid)")))
            #expect(x.k.watch(src: AgentId(aid), watcher: AgentId("spy_\(aid)")) == nil)
        }
        await printedNothing([KEY]) {
            let out = await forward(
                a, aLeg, ["type": "set_key", "key": .string(KEY)],
                secret: paths("payload.payload.key", "result"))
            #expect(out.result == ["ok": true, "len": .integer(Int64(KEY.count))])
            for (i, (_, aid)) in spies.enumerated() {
                var mirrored: [JSON] = []
                while let m = inboxes[i].tryTake() { mirrored.append(m.payload) }
                #expect(
                    !JSON.array(mirrored).serialize().contains(KEY),
                    "\(aid): \(JSON.array(mirrored).serialize())")
            }
            // the bridge state holds no frame once the call settled
            for (bundle, leg) in [(kind == .ws ? a.ws : a.relay, aLeg), (kind == .ws ? b.ws : b.relay, bLeg)] {
                let st = bundle.existingState(AgentId(leg))
                #expect(st?.pendingCount == 0)
                #expect(!(st?.extra ?? [:]).values.contains { $0.serialize().contains(KEY) })
            }
            keptOut(a, await records(a), KEY)
            keptOut(b, await records(b), KEY)
        }
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    // ─── (g) the relay seal ─────────────────────────────────────────────────

    /// Outbound: ANY mark on the forward call — one that would cross, a result
    /// mark, even one on forward's own field — is refused; nothing reaches the
    /// far kernel, and the caller's own records stay masked.
    @Test(
        .timeLimit(.minutes(1)),
        arguments: [
            ["payload.payload.key"], ["result"], ["payload.timeout"],
            [["payload", "payload", "key"]],
        ] as [JSON])
    func theRelayRefusesAMarkedForwardBeforeAFrameLeaves(_ secret: JSON) async {
        let (a, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, .relay)
        let out = await op(
            a.k, aLeg,
            [
                "type": "forward", "target": "vault",
                "payload": ["type": "set_key", "key": .string(KEY)], "timeout": 5,
            ], secret: secret)
        #expect(out.status == .errored, "\(out.asJSON.serialize())")
        #expect(out.result == SEALED, "\(out.result.serialize())")
        #expect(vault.got.isEmpty, "nothing crossed the relay")
        let rb = await records(b)
        #expect(
            !rb.kept.contains { $0["agent"].asString == "vault" && $0["verb"].asString != "boot" },
            "\(JSON.array(rb.kept).serialize())")
        if secret != ["result"] { keptOut(a, await records(a), KEY) }
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    /// A binary `forward` (its chunk marked `payload.payload.bytes`) is the same
    /// outbound path, sealed the same way.
    @Test(.timeLimit(.minutes(1)))
    func theRelayRefusesAMarkedBinaryForward() async {
        let (a, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let (aLeg, bLeg) = await bridgePair(a, b, .relay)
        var it = Item(
            AgentId(aLeg),
            ["type": "forward", "target": "vault", "payload": ["type": "keep", "path": "k"]],
            binary: Blob(parts: ["payload", "bytes"], Data(KEY.utf8)))
        it.secret = paths("payload.payload.bytes")
        let out = await a.k.send([it])[0]
        #expect(out.status == .errored, "\(out.asJSON.serialize())")
        #expect(out.result == SEALED, "\(out.result.serialize())")
        #expect(vault.got.isEmpty && vault.blobs.isEmpty, "nothing crossed the relay")
        keptOut(a, await records(a), KEY)
        await dropLeg(a, aLeg)
        await dropLeg(b, bLeg)
    }

    /// Inbound: a tunneled `call` frame that names `secret` is answered with the
    /// seal's refusal; the far target never runs and the serving kernel records no
    /// call to it.
    @Test(.timeLimit(.minutes(1)))
    func theRelayRefusesAnInboundMarkedFrameAndDispatchesNothing() async throws {
        let (_, b, vault) = await twoKernels()
        await vaultOn(b, callers: ["A-g|*"])
        let bLeg = await create(
            b,
            [
                "parent_id": "kernel_state", "handler_module": "relay_connector.tools",
                "id": "b_leg", "transport": "memory", "peer_guid": "A-g",
                "ingress_rule": "allow_all_ingress", "can_call": ["vault": "*"],
            ])
        let hub = RelayHub()
        let wa = await hub.register("RA")
        let wb = await hub.register("RB")
        let ta = await RelayTransport.attach(wire: wa, partnerGuid: "RB")
        b.relay.injectTransport(
            agentId: AgentId(bLeg), transport: await RelayTransport.attach(wire: wb, partnerGuid: "RA"))
        let booted = await op(b.k, bLeg, ["type": "boot"]).result
        #expect(booted["booted"].asBool == true, "\(booted.serialize())")
        try await ta.send([
            "type": "call", "id": "sealed-1", "target": "vault",
            "payload": ["type": "set_key", "key": .string(KEY)], "caller": "A-g|kernel",
            "secret": ["payload.key"],
        ])
        let sealed = try await ta.recv().frame
        #expect(
            sealed == ["type": "reply", "id": "sealed-1", "data": SEALED], "\(sealed.serialize())")
        // the same leg still serves an UNMARKED call — the seal is about marks only
        try await ta.send([
            "type": "call", "id": "plain-1", "target": "vault", "payload": ["type": "issue"],
            "caller": "A-g|kernel",
        ])
        let plain = try await ta.recv().frame
        #expect(plain["data"] == ["key": .string(ISSUED), "note": "issued"], "\(plain.serialize())")
        #expect(vault.got == [["type": "issue"]], "\(JSON.array(vault.got).serialize())")
        let rb = await records(b)
        #expect(
            rb.kept.filter { $0["agent"].asString == "vault" && $0["verb"].asString != "boot" }
                .map { $0["verb"] } == ["issue"],
            "\(JSON.array(rb.kept).serialize())")
        keptOut(b, rb, KEY)
        await ta.close()
        await dropLeg(b, bLeg)
    }
}
