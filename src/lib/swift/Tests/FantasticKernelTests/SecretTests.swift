// SECRET VALUES on a call (owner, 2026-10-01; `Secret.swift`): the caller marks
// paths on the batch item; the target gets the real payload and the caller the
// real reply, while every record and mirror of the call shows `<secret>`.
//
// Twin of python `tests/kernel/test_secret.py` (the first seven tests), plus the
// swift carrier: a binary item's blob and a binary reply's body ride BESIDE the
// JSON here, so `payload.bytes` / `result.bytes` must reach them where python's
// dict holds them.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel
@testable import FantasticKernelState
@testable import FantasticRamStore

private let KEY = "sk-live-0123456789abcdef"
private let ISSUED = "tok-issued-fedcba9876543210"
private let PLACEHOLDER = Secret.placeholder

/// Python `_probe`: records every payload (and blob) it is handed.
private final class SecretProbe: AgentBundle, @unchecked Sendable {
    let name = "secret_probe"
    private let lock = NSLock()
    private var _got: [JSON] = []
    private var _blobs: [Data] = []

    var got: [JSON] { lock.withLock { _got } }
    var blobs: [Data] { lock.withLock { _blobs } }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        if ["boot", "shutdown", "reflect"].contains(verb) {
            return .object([
                "id": .string(agentId.value), "sentence": .string("secret probe"),
                "verbs": .object([:]),
            ])
        }
        lock.withLock { _got.append(payload) }
        switch verb {
        case "set_key":
            let key = payload["key"].asString ?? ""
            return .object(["ok": .bool(true), "len": .integer(Int64(key.unicodeScalars.count))])
        case "issue":
            return .object(["key": .string(ISSUED), "note": .string("issued")])
        case "fail":
            throw PyError("RuntimeError", "rejected key \(payload["key"].asString ?? "")")
        default:
            return .object(["error": .string("unknown verb \(pyRepr(verb))")])
        }
    }

    /// The swift carrier: `keep` stores the blob, `give` answers with a body.
    func handleBinary(agentId: AgentId, header: JSON, blob: Data, kernel: Kernel) async throws
        -> (JSON?, Data?)
    {
        switch header["type"].asString ?? "" {
        case "keep":
            lock.withLock {
                _got.append(header)
                _blobs.append(blob)
            }
            return (.object(["kept": .integer(Int64(blob.count))]), nil)
        case "give":
            return (.object(["note": .string("given")]), Data(ISSUED.utf8))
        default:
            return (try await handle(agentId: agentId, payload: header, kernel: kernel), nil)
        }
    }
}

private func makeKernel(_ probe: SecretProbe) -> Kernel {
    let r = BundleRegistry()
    r.register("kernel_state.tools", KernelStateBundle())
    r.register("ram_store.tools", RamStoreBundle())
    r.register("secret_probe.tools", probe)
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: "kernel_state.tools", parentId: nil)
    k.register(root)
    k.setRoot(root)
    return k
}

private func mk(_ k: Kernel, _ aid: String = "vault") async -> AgentId {
    let out = await k.send([
        Item(
            "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("secret_probe.tools"), "id": .string(aid),
            ]))
    ])[0]
    return AgentId(out.result["id"].asString ?? "")
}

private func item(
    _ target: AgentId, _ payload: OrderedDictionary<String, JSON>, secret: JSON?,
    id: String? = nil, binary: Data? = nil
) -> Item {
    var it = Item(target, .object(payload), id: id, binary: binary)
    it.secret = secret
    return it
}

private func paths(_ p: String...) -> JSON { .array(p.map { .string($0) }) }

private func ledger(_ k: Kernel) async -> [JSON] {
    let r = await k.send([Item("kernel", .object(["type": .string("ledger")]))])[0].result
    return r["executions"].asArray ?? []
}

/// Python `_mentions`: the value anywhere in the rendered structure.
private func mentions(_ x: JSON, _ value: String) -> Bool { x.serialize().contains(value) }

/// A thread-safe sink for the state stream.
private final class Events: @unchecked Sendable {
    private let lock = NSLock()
    private var _all: [JSON] = []
    func add(_ e: JSON) { lock.withLock { _all.append(e) } }
    var all: [JSON] { lock.withLock { _all } }
}

@Suite("secret values on a call")
struct SecretTests {

    // ─── the item field ─────────────────────────────────────────────────────

    @Test func aBadSecretFieldIsRefusedBeforeAnythingRuns() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        // python's texts, verbatim (`kernel/_secret.check`)
        let cases: [(JSON, String)] = [
            (
                .string("payload.key"),
                "send: secret must be a non-empty list of paths, got 'payload.key'"
            ),
            (.array([]), "send: secret must be a non-empty list of paths, got []"),
            (
                paths("key"),
                "send: secret path 'key' must start with 'payload.' or be 'result[.…]'"
            ),
            (
                paths("payload"),
                "send: secret path 'payload' must start with 'payload.' or be 'result[.…]'"
            ),
            (
                paths("payload.type"),
                "send: secret cannot hide payload.type — the verb is how every row is read"
            ),
            (
                paths("payload.nope"),
                "send: secret path 'payload.nope' names nothing in the payload"
            ),
            (paths("payload..key"), "send: secret path 'payload..key' has an empty step"),
            (.array([.integer(5)]), "send: secret path must be a non-empty string, got 5"),
            (paths(""), "send: secret path must be a non-empty string, got ''"),
            (paths("result."), "send: secret path 'result.' has an empty step"),
            (
                .object(["a": .integer(1)]),
                "send: secret must be a non-empty list of paths, got {'a': 1}"
            ),
            (
                paths("payload.type.x"),
                "send: secret path 'payload.type.x' names nothing in the payload"
            ),
        ]
        for (secret, text) in cases {
            let out = await k.send([
                item(a, ["type": .string("set_key"), "key": .string(KEY)], secret: secret)
            ])[0]
            #expect(
                out.status == .failed && out.pid == nil,
                "\(secret.serialize()) \(out.asJSON.serialize())")
            #expect(out.result["reason"].asString == "bad_arg", "\(out.result.serialize())")
            #expect(out.result["arg"].asString == "secret", "\(out.result.serialize())")
            #expect(out.result["error"].asString == text, "\(out.result.serialize())")
        }
        #expect(probe.got.isEmpty, "a refused item must not reach the handler")
    }

    // ─── a secret payload field ─────────────────────────────────────────────

    @Test func theTargetGetsTheValueAndEveryRecordShowsThePlaceholder() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        let events = Events()
        k.subscribe { ev, _ in events.add(ev) }
        let out = await k.send([
            item(
                a, ["type": .string("set_key"), "ref": .string("openai"), "key": .string(KEY)],
                secret: paths("payload.key"))
        ])[0]
        // the handler and the caller see the real thing
        #expect(
            probe.got == [
                .object([
                    "type": .string("set_key"), "ref": .string("openai"), "key": .string(KEY),
                ])
            ])
        #expect(out.status == .completed)
        #expect(
            out.result == .object(["ok": .bool(true), "len": .integer(Int64(KEY.count))]),
            "\(out.result.serialize())")
        // the call row: placeholder, the other fields intact, the PATHS recorded
        let row = k.rows.getOne(out.pid!.value)
        #expect(
            row?["payload"]
                == .object([
                    "type": .string("set_key"), "ref": .string("openai"),
                    "key": .string(PLACEHOLDER),
                ]), "\(row.map { JSON.object($0).serialize() } ?? "nil")")
        #expect(row?["secret"] == paths("payload.key"))
        // the state stream's send event — payload AND summary
        let sends = events.all.filter {
            $0["kind"].asString == "send" && $0["agent_id"].asString == a.value
        }
        #expect(
            !sends.isEmpty && !sends.contains { mentions($0, KEY) },
            "\(sends.map { $0.serialize() })")
        #expect(sends.first?["payload"]["key"].asString == PLACEHOLDER)
        // the ledger read-back
        #expect(!mentions(.array(await ledger(k)), KEY))
    }

    @Test func aWatcherMirrorShowsThePlaceholder() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        let q = k.ensureInbox(AgentId("spy"))
        #expect(k.watch(src: a, watcher: AgentId("spy")) == nil)
        _ = await k.send([
            item(
                a, ["type": .string("set_key"), "key": .string(KEY)], secret: paths("payload.key"))
        ])
        var mirrored: [InboxItem] = []
        while let m = q.tryTake() { mirrored.append(m) }
        #expect(
            !mirrored.isEmpty && !mirrored.contains { mentions($0.payload, KEY) },
            "\(mirrored.map { $0.payload.serialize() })")
    }

    @Test func anErrorTextQuotingTheValueIsScrubbedInTheRecord() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        let out = await k.send([
            item(a, ["type": .string("fail"), "key": .string(KEY)], secret: paths("payload.key"))
        ])[0]
        #expect(out.status == .failed)
        #expect(
            (out.result["error"].asString ?? "").contains(KEY), "the caller holds the real outcome")
        let led = await ledger(k)
        #expect(!mentions(.array(led), KEY), "\(JSON.array(led).serialize())")
        let mine = led.first { $0["id"].asInt == out.pid?.value }
        #expect(
            (mine?["outcome"]["error"].asString ?? "").contains(PLACEHOLDER),
            "\(mine?.serialize() ?? "nil")")
        // the call row's own stamp carries the scrubbed text too
        let stamp = k.rows.getOne(out.pid!.value)?["outcome"]
        #expect(
            stamp?["error"].asString == "RuntimeError: rejected key \(PLACEHOLDER)",
            "\(stamp?.serialize() ?? "nil")")
    }

    // ─── a secret result ────────────────────────────────────────────────────

    @Test func aSecretResultReachesTheCallerAndIsKeptAsThePlaceholder() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        // named: the settle keeps the whole result
        let out = await k.send([
            item(a, ["type": .string("issue")], secret: paths("result.key"), id: "issue-1")
        ])[0]
        #expect(out.result == .object(["key": .string(ISSUED), "note": .string("issued")]))
        let led = await ledger(k)
        #expect(!mentions(.array(led), ISSUED), "\(JSON.array(led).serialize())")
        let mine = led.first { $0["name"].asString == "issue-1" }
        #expect(
            mine?["outcome"]["result"]
                == .object(["key": .string(PLACEHOLDER), "note": .string("issued")]),
            "\(mine?.serialize() ?? "nil")")
    }

    @Test func theWholeResultCanBeSecret() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        let out = await k.send([
            item(a, ["type": .string("issue")], secret: paths("result"), id: "issue-2")
        ])[0]
        #expect(out.result["key"].asString == ISSUED)
        let mine = await ledger(k).first { $0["name"].asString == "issue-2" }
        #expect(mine?["outcome"]["result"] == .string(PLACEHOLDER), "\(mine?.serialize() ?? "nil")")
    }

    // ─── the record cannot answer a replay ──────────────────────────────────

    @Test func theStoreNeverHoldsTheValueAndAReplayIsRefused() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let made = await k.send([
            Item(
                "kernel",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("ram_store.tools"), "id": .string("memory"),
                    "operator": .bool(true), "admins": .array([.string("kernel")]),
                ]))
        ])[0].result
        #expect(made["id"].asString == "memory", "\(made.serialize())")
        k.update(AgentId("kernel_state"), ["memory_store_id": .string("memory")])
        let started = await k.start(bootAll: false)
        #expect(started["started"].asBool == true, "\(started.serialize())")
        let a = await mk(k)

        let send: () async -> Outcome = {
            await k.send([
                item(
                    a, ["type": .string("set_key"), "key": .string(KEY)],
                    secret: paths("payload.key"), id: "key-1")
            ])[0]
        }
        let first = await send()
        #expect(first.status == .completed, "\(first.result.serialize())")
        // in RAM the execution still answers a re-send (the same process holds it)
        let again = await send()
        #expect(again.result == first.result && probe.got.count == 1)
        await k.flushExecutions()
        let tables = await k.send([Item("memory", .object(["type": .string("dump")]))])[0]
            .result["tables"]
        #expect(!mentions(tables, KEY), "the memory store holds the secret value")
        // once it lives only in the store, the record cannot answer — refused, loud
        for e in k.ledger.entries() { k.ledger.forget(e.id) }
        let replay = await send()
        #expect(replay.status == .failed, "\(replay.asJSON.serialize())")
        #expect(replay.result["reason"].asString == "bad_arg", "\(replay.result.serialize())")
        #expect(
            replay.result["error"].asString
                == "send: execution 'key-1' carried secrets — its record holds <secret> "
                + "placeholders, not values, so it cannot be answered from the record; send "
                + "again under a new id", "\(replay.result.serialize())")
        #expect(replay.result["arg"].asString == "id")
        #expect(probe.got.count == 1, "the refused replay must not run the handler")
    }

    // ─── the swift carrier: raw bytes beside the JSON ───────────────────────

    @Test func aSecretBlobReachesTheTargetAndIsRecordedAsThePlaceholder() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        let q = k.ensureInbox(AgentId("spy"))
        #expect(k.watch(src: a, watcher: AgentId("spy")) == nil)
        let out = await k.send([
            item(
                a, ["type": .string("keep"), "path": .string("k")], secret: paths("payload.bytes"),
                binary: Data(KEY.utf8))
        ])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(probe.blobs == [Data(KEY.utf8)], "the target gets the real blob")
        // the call row holds the placeholder where python's dict held the bytes —
        // not the `{len}` digest of a value that is not there
        let row = k.rows.getOne(out.pid!.value)
        #expect(
            row?["payload"]
                == .object([
                    "type": .string("keep"), "path": .string("k"), "bytes": .string(PLACEHOLDER),
                ]), "\(row.map { JSON.object($0).serialize() } ?? "nil")")
        // the mirror is the shown payload: text, no body
        var mirrored: [InboxItem] = []
        while let m = q.tryTake() { mirrored.append(m) }
        #expect(mirrored.count == 1 && mirrored.allSatisfy { $0.body == nil })
        #expect(mirrored.first?.payload["bytes"].asString == PLACEHOLDER)
    }

    @Test func aSecretReplyBodyReachesTheCallerAndIsRecordedAsThePlaceholder() async {
        let probe = SecretProbe()
        let k = makeKernel(probe)
        let a = await mk(k)
        let out = await k.send([
            item(
                a, ["type": .string("give")], secret: paths("result.bytes"), id: "give-1",
                binary: Data())
        ])[0]
        #expect(out.body == Data(ISSUED.utf8), "the caller gets the real body")
        let mine = await ledger(k).first { $0["name"].asString == "give-1" }
        #expect(
            mine?["outcome"]["result"]
                == .object(["note": .string("given"), "bytes": .string(PLACEHOLDER)]),
            "\(mine?.serialize() ?? "nil")")
    }

    /// A blob's secret VALUE is python's `str(bytes)` — the `b'…'` repr.
    @Test func aBlobValueIsSpelledAsPythonsBytesRepr() {
        #expect(pyBytesRepr(Data([0x27, 0x41, 0x0A, 0xFF, 0x5C])) == #"b"'A\n\xff\\""#)
        #expect(pyBytesRepr(Data("say \"hi\" it's".utf8)) == #"b'say "hi" it\'s'"#)
        #expect(pyBytesRepr(Data([0x00, 0x09, 0x0D, 0x7F, 0x7E, 0x20])) == #"b'\x00\t\r\x7f~ '"#)
        #expect(pyBytesRepr(Data()) == "b''")
    }
}
