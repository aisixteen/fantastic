// A MODEL NEVER RECEIVES A SECRET (owner, 2026-10-02, option B).
//
// An AI agent reads its key from a credential store as ITSELF, so the same grant
// would let its MODEL `send` the store's `get` and read the value into its prompt
// and memory. `holds_secrets: true` on a record makes that agent unreachable from
// a model turn: `routeReply` — the one place a model-parsed call is dispatched —
// refuses every call whose target or `reply_to` is such an agent, loud, and sends
// nothing. The backend's own key read (`keyReader`) calls the kernel directly and
// is untouched.
//
// Twin of python `ai_core/tests/test_secret_holders.py`. Route-level cases run
// `routeReply` against a bare kernel whose agents are the python stub's two
// halves (python drives `_route_reply` against a stub kernel; swift's takes a
// real `Kernel`); the end-to-end cases run a real kernel, a real
// credential_store / file_bridge and a scripted model that reads its key through
// `keyReader` before every pass, as a real provider does.

import FantasticFile
import FantasticJSON
import FantasticKernel
import FantasticRamStore
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticCredentialStore

private let EP = "https://api.example.com/v1/responses"
private let KEY = "sk-live-holder-0123456789"
private let DONE = "<call name=\"done\"></call>"
private let DOT = "\u{b7}"

private func refused(_ agent: String) -> String {
    SECRET_HOLDER_TEXT.replacingOccurrences(of: "{agent}", with: agent)
}

/// Python `_call`: one `send` call as the model writes it.
private func call(
    _ target: String, _ verb: String = "get", replyTo: String = "self",
    args: OrderedDictionary<String, JSON> = [:]
) -> String {
    var payload: OrderedDictionary<String, JSON> = ["type": .string(verb)]
    for (k, v) in args { payload[k] = v }
    return renderCall(
        "send",
        [
            "target_id": .string(target), "payload": .object(payload),
            "reply_to": .string(replyTo),
        ])
}

// ─── the choke point, on a bare kernel ───────────────────────────────────────

/// Python's stub `send` for a non-`send` item: answers `{ok:<id>}` and records
/// what it answered (the stub's `batches`, read as what actually ran).
private final class Echo: AgentBundle, @unchecked Sendable {
    let name = "echo"
    private let lock = NSLock()
    private var seen: [String] = []
    var answered: [String] { lock.withLock { seen } }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            lock.withLock { seen.append(agentId.value) }
            return .object(["ok": .string(agentId.value)])
        }
    }
}

/// Python's stub `sent`: every delivered `(agent, payload)` — a turn handed back
/// to the model, or a `send` the model addressed to a peer.
private final class Deliveries: AgentBundle, @unchecked Sendable {
    let name = "delivery"
    private let lock = NSLock()
    private var seen: [(String, JSON)] = []

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            lock.withLock { seen.append((agentId.value, payload)) }
            return .object(["recorded": .bool(true)])
        }
    }

    /// Python `_settle`: wait for `count` deliveries (a delivery is a spawned
    /// kernel task the router does not await), then a beat for stragglers.
    func settled(_ count: Int) async -> [(String, JSON)] {
        let deadline = Date().addingTimeInterval(2)
        while Date() < deadline, lock.withLock({ seen.count }) < count {
            try? await Task.sleep(nanoseconds: 5_000_000)
        }
        try? await Task.sleep(nanoseconds: 20_000_000)
        let out = lock.withLock { seen }
        #expect(out.count >= count, "delivered \(out.count), wanted \(count)")
        return out
    }
}

/// This kernel's guid on the stub's root — python's `OWN_GUID`.
private let OWN_GUID = "own-guid"

/// `creds` holds secrets (its record says so); `notes` is a plain agent; `ai` and
/// `peer` receive deliveries. `selfId` is the routing agent. The root is
/// `kernel_state`, this kernel's guid `OWN_GUID`.
private func stubKernel(selfId: String = "ai") async -> (Kernel, Echo, Deliveries) {
    let r = BundleRegistry()
    let echo = Echo()
    let sent = Deliveries()
    r.register("echo.tools", echo)
    r.register("delivery.tools", sent)
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
        meta: ["guid": .string(OWN_GUID)])
    k.register(root)
    k.setRoot(root)
    for (id, module, extra) in [
        ("creds", "echo.tools", [FantasticAICore.HOLDS_SECRETS: JSON.bool(true)]),
        ("notes", "echo.tools", [:]), (selfId, "delivery.tools", [:]),
        ("peer", "delivery.tools", [:]),
    ] as [(String, String, OrderedDictionary<String, JSON>)] {
        var p: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state", "id": .string(id),
            "handler_module": .string(module),
        ]
        for (key, v) in extra { p[key] = v }
        let made = await k.send([Item("kernel", .object(p))])[0].result
        #expect(made["id"].asString == id, "\(made.serialize())")
    }
    return (k, echo, sent)
}

private func route(_ k: Kernel, _ text: String, selfId: String = "ai", messageId: Int64? = nil)
    async
{
    await routeReply(
        selfId: AgentId(selfId), kernel: k, text: text, clientId: "cli", sendTo: nil,
        systemPrompt: nil, incoming: "", sendTimeout: 9.0,
        modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW, messageId: messageId)
}

// ─── end to end: a real kernel, a real store, a scripted model ───────────────

/// A scripted mind that, like a real backend, reads its KEY before every pass
/// (through `keyReader`, as the agent) and records every prompt it was sent.
/// Python `_Model`. Held outside the provider, which is built per pass.
private final class KeyedScript: @unchecked Sendable {
    let required: Bool
    private let lock = NSLock()
    private var turns: [String]
    private var _calls = 0
    private var _keys: [String?] = []
    private var _seen: [[JSON]] = []

    init(required: Bool, _ turns: [String]) {
        self.required = required
        self.turns = turns
    }

    var calls: Int { lock.withLock { _calls } }
    var keys: [String?] { lock.withLock { _keys } }
    var seen: [[JSON]] { lock.withLock { _seen } }

    func began() { lock.withLock { _calls += 1 } }

    /// The key read and the prompt recorded, then this pass's turn.
    func pass(key: String?, messages: [JSON]) -> String {
        lock.withLock {
            _keys.append(key)
            _seen.append(messages)
            return turns.isEmpty ? DONE : turns.removeFirst()
        }
    }

    /// Python `_until(lambda: model.calls >= n)`, then a beat — bounded.
    func until(passes n: Int) async {
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline, keys.count < n {
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(keys.count >= n, "condition not reached: \(keys.count) passes")
        try? await Task.sleep(nanoseconds: 200_000_000)
    }
}

private final class KeyedModel: AIProvider, @unchecked Sendable {
    let model = "m"
    let read: KeyReader
    let script: KeyedScript

    init(read: @escaping KeyReader, script: KeyedScript) {
        self.read = read
        self.script = script
    }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        script.began()
        let (read, script) = (read, script)
        return AsyncThrowingStream { c in
            Task {
                do {
                    let key = try await read(EP)
                    c.yield(.token(script.pass(key: key, messages: messages)))
                    c.finish()
                } catch {
                    c.finish(throwing: error)
                }
            }
        }
    }
}

/// The shared ai_core machinery with the scripted model as its provider — python
/// patches `core._providers[aid]` on the real responses backend; swift builds a
/// provider per pass, so the backend is built with this one.
private final class KeyedBackend: AgentBundle, @unchecked Sendable {
    let name = "responses_backend"
    private let core: AIBackend
    var verbs: [String] { AIBackend.verbNames }

    init(_ scripts: [String: KeyedScript]) {
        core = buildAIBackend(
            AIBackendConfig(
                kind: "responses_backend", sentence: "scripted",
                makeProvider: { agent, _, kernel in
                    guard let s = scripts[agent.id.value] else {
                        throw PyError("RuntimeError", "no script for \(agent.id.value)")
                    }
                    return KeyedModel(
                        read: keyReader(
                            agent.id, kernel, backend: "responses_backend", required: s.required),
                        script: s)
                }))
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }
}

/// The keychain is never reached here (file backend).
private struct NoKeychain: KeychainStore {
    func get(service: String, account: String) throws -> String? { nil }
    func set(service: String, account: String, value: String) throws {}
    func delete(service: String, account: String) throws {}
}

private func op(_ k: Kernel, _ target: String, _ payload: JSON, secret: JSON? = nil) async
    -> Outcome
{
    var it = Item(AgentId(target), payload)
    it.secret = secret
    return await k.send([it])[0]
}

/// Python `seeded_kernel` + `memory_meta`: a root whose running dir is `dir`, a
/// memory store that accepts `ais`, and the scripted backend.
private func seeded(_ dir: URL, _ scripts: [String: KeyedScript]) async -> Kernel {
    let r = BundleRegistry()
    r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: NoKeychain()))
    r.register(RamStoreBundle.handlerModule, RamStoreBundle())
    r.register("file_bridge.tools", FileBundle())
    r.register("responses_backend.tools", KeyedBackend(scripts))
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil, meta: ["guid": "g1"],
        rootPath: dir.appendingPathComponent(".fantastic"))
    k.register(root)
    k.setRoot(root)
    var accept: OrderedDictionary<String, JSON> = [:]
    for id in scripts.keys.sorted() { accept[id] = "*" }
    let mem = await op(
        k, "kernel",
        .object([
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": .string(RamStoreBundle.handlerModule), "id": "memory",
            "operator": true, "admins": ["kernel"], "callable_by": .object(accept),
        ]))
    #expect(mem.result["id"] == "memory", "\(mem.result.serialize())")
    return k
}

/// Python `_ai`: a backend agent on the kernel's memory store, granted `canCall`.
private func ai(
    _ k: Kernel, _ id: String, _ canCall: OrderedDictionary<String, JSON>,
    _ extra: OrderedDictionary<String, JSON> = [:]
) async {
    var grant = canCall
    grant["memory"] = "*"
    grant["cli"] = "*"
    var p: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": "responses_backend.tools", "id": .string(id),
        "endpoint": .string(EP), "model": "m", "context_window": 32768,
        "memory_store_id": "memory", "can_call": .object(grant),
    ]
    for (key, v) in extra { p[key] = v }
    let made = await op(k, "kernel", .object(p))
    #expect(made.result["id"].asString == id, "\(made.result.serialize())")
}

/// Python `_rows`: the call rows the kernel keeps for `agent`'s `verb`.
private func rows(_ k: Kernel, _ agent: String, _ verb: String) -> [JSON] {
    k.rows.all().map { JSON.object($0) }.filter {
        $0["kind"].asString == "call" && $0["agent"].asString == agent
            && $0["verb"].asString == verb
    }
}

private func tmpDir() -> URL {
    let d = FileManager.default.temporaryDirectory
        .appendingPathComponent("holders-\(UUID().uuidString.prefix(8))")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

@Suite("secret holders — a model never reaches one")
struct SecretHoldersTests {

    // ─── the choke point ────────────────────────────────────────────────────

    @Test func aSingleCallToAHolderIsRefusedAndNothingIsDispatched() async throws {
        let (k, echo, sent) = await stubKernel()
        await route(k, call("creds", args: ["ref": "main"]))
        let landed = await sent.settled(1)
        #expect(echo.answered.isEmpty, "nothing may reach the holder")
        let (target, payload) = try #require(landed.first)
        #expect(target == "ai")
        let text = payload["text"].asString ?? ""
        #expect(text.hasPrefix("[reply to - \(DOT) creds get"), Comment(rawValue: text))
        #expect(text.contains(refused("creds")), Comment(rawValue: text))
        #expect(text.contains("unauthorized"), Comment(rawValue: text))
    }

    /// The plain call is `ping`, not python's `reflect`: on a real kernel a
    /// reflect is composed by the kernel itself, and the stub only echoes.
    @Test func inABatchOnlyTheHolderIsRefused() async throws {
        let (k, echo, sent) = await stubKernel()
        await route(k, call("creds", args: ["ref": "main"]) + call("notes", "ping"))
        let landed = await sent.settled(1)
        #expect(echo.answered == ["notes"])
        let blocks = (landed.first?.1["text"].asString ?? "").components(separatedBy: "\n\n")
        #expect(blocks.count == 2, "\(blocks)")
        #expect(
            blocks[0].hasPrefix("[reply to - \(DOT) creds get \(DOT) 1 of 2"),
            Comment(rawValue: blocks[0]))
        #expect(blocks[0].contains(refused("creds")), Comment(rawValue: blocks[0]))
        #expect(
            blocks[1].range(
                of: "^\\[reply to [0-9]+ \(DOT) notes ping \(DOT) 2 of 2", options: .regularExpression)
                != nil, Comment(rawValue: blocks[1]))
    }

    @Test func aToFanOutDropsOnlyTheHolder() async throws {
        let (k, _, sent) = await stubKernel()
        let text = renderCall(
            "send",
            [
                "to": .array([.string("peer"), .string("creds")]), "reply_to": "self",
                "payload": .object(["type": "send", "text": "hello"]),
            ])
        await route(k, text, messageId: 7)
        let landed = await sent.settled(2)
        let dispatched = landed.filter { $0.1["text"].asString == "hello" }.map(\.0)
        #expect(dispatched == ["peer"], "\(landed.map(\.0))")
        let own = landed.filter { $0.0 == "ai" }.map { $0.1["text"].asString ?? "" }
        #expect(own.count == 1 && own[0].contains(refused("creds")), "\(own)")
    }

    /// A delivery to a holder is a dispatch on the model's behalf too.
    @Test func replyToAHolderIsRefusedToSelf() async throws {
        let (k, echo, sent) = await stubKernel()
        await route(k, call("notes", "ping", replyTo: "creds"))
        let landed = await sent.settled(1)
        #expect(echo.answered.isEmpty, "the call is refused whole, not half-run")
        #expect(landed.map(\.0) == ["ai"])
        #expect((landed.first?.1["text"].asString ?? "").contains(refused("creds")))
    }

    /// The refused call is not in flight, so `await` is the stall it is.
    ///
    /// ITS OWN AGENT ID: `inFlightTurns` is keyed by the bare id across every
    /// kernel in the process, so a sibling case's live batch for `ai` would count
    /// as this agent's in-flight work and turn the refusal into an honoured wait.
    @Test func awaitBesideARefusedCallHasNothingInFlight() async throws {
        let (k, echo, sent) = await stubKernel(selfId: "sh_aw")
        await route(k, call("creds", args: ["ref": "main"]) + "<call name=\"await\"></call>", selfId: "sh_aw")
        let landed = await sent.settled(2)
        #expect(echo.answered.isEmpty)
        let texts = landed.filter { $0.0 == "sh_aw" }.map { $0.1["text"].asString ?? "" }
        #expect(texts.contains { $0.contains(refused("creds")) }, "\(texts)")
        #expect(texts.contains { $0.hasPrefix("[await refused]") }, "\(texts)")
    }

    /// `<guid>|creds` lives on another kernel: its record is not here, so this
    /// layer cannot see it — the call goes, and that kernel guards its own
    /// holders. Python's stub records the dispatched batch; here no peer `g1` is
    /// connected, so the KERNEL answers the send `unknown_agent` — which is what
    /// shows routing handed it to `kernel.send` instead of refusing it.
    @Test func aPeerQualifiedTargetIsNotJudgedHere() async throws {
        let (k, _, sent) = await stubKernel()
        await route(k, call("g1|creds", args: ["ref": "main"]))
        let landed = await sent.settled(1)
        let text = landed.first?.1["text"].asString ?? ""
        #expect(text.contains(" \(DOT) g1|creds get"), Comment(rawValue: text))
        #expect(!text.contains("holds secrets"), Comment(rawValue: text))
        #expect(text.contains("unknown_agent"), "the kernel answered it: \(text)")
    }

    /// `<this kernel's guid>|creds` is not a foreign address: the kernel resolves
    /// it to the local `creds` and would deliver the call. The address is resolved
    /// the same way BEFORE the holder check, so it is refused like the bare id — as
    /// a target and as a `reply_to`.
    @Test func thisKernelsOwnGuidNamesTheLocalHolder() async throws {
        let (k, echo, sent) = await stubKernel()
        let own = "\(OWN_GUID)|creds"
        await route(k, call(own, args: ["ref": "main"]) + call("notes", "ping", replyTo: own))
        let landed = await sent.settled(1)
        #expect(echo.answered.isEmpty, "nothing may reach the holder")
        #expect(landed.map(\.0) == ["ai"], "\(landed.map(\.0))")
        let blocks = (landed.first?.1["text"].asString ?? "").components(separatedBy: "\n\n")
        #expect(blocks.count == 2, "\(blocks)")
        #expect(blocks.allSatisfy { $0.contains(refused(own)) }, "\(blocks)")
    }

    // ─── end to end ─────────────────────────────────────────────────────────

    /// Python `_store_and_ai`: a credential store `creds` holding `main` (the key,
    /// readable by `ai`), and the backend agent `ai` granted `canCall` (default
    /// `{creds: *}`), its key read from there.
    private func storeAndAI(
        _ k: Kernel, _ dir: URL, _ canCall: OrderedDictionary<String, JSON> = ["creds": "*"]
    ) async {
        let made = await op(
            k, "kernel",
            .object([
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": .string(CredentialStoreBundle.handlerModule), "id": "creds",
                "file": .string(dir.appendingPathComponent("creds.yaml").path),
                "callable_by": ["ai": "*"],
            ]))
        #expect(made.result["id"] == "creds", "\(made.result.serialize())")
        #expect(
            k.agent(AgentId("creds"))?.metaValue(forKey: FantasticAICore.HOLDS_SECRETS) == .bool(true),
            "the store stamps itself at boot")
        let put = await op(
            k, "creds",
            [
                "type": "set", "ref": "main", "host": "api.example.com",
                "fields": ["api_key": .string(KEY)],
            ], secret: ["payload.fields"])
        #expect(put.result["stored"] == true, "\(put.result.serialize())")
        var refs = k.agent(AgentId("creds"))?.meta["refs"]?.asObject ?? [:]
        var entry = refs["main"]?.asObject ?? [:]
        entry["readers"] = ["ai"]
        refs["main"] = .object(entry)
        k.update(AgentId("creds"), ["refs": .object(refs)])
        await ai(k, "ai", canCall, ["credentials": ["store": "creds", "ref": "main"]])
    }

    @Test(.timeLimit(.minutes(1)))
    func aModelCallToACredentialStoreIsRefusedAndTheKeyStaysOut() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let model = KeyedScript(required: true, [call("creds", args: ["ref": "main"]), DONE])
        let k = await seeded(dir, ["ai": model])
        await storeAndAI(k, dir)

        _ = await op(k, "ai", ["type": "send", "text": "fetch the key"])
        await model.until(passes: 2)
        #expect(model.calls == 2)
        // the PROVIDER read its key on both passes — the backend path is untouched
        #expect(model.keys == [KEY, KEY])
        // the MODEL's call was refused, loud, in its next prompt
        let next = model.seen.dropFirst().first?.last?["content"].asString ?? ""
        #expect(next.contains(refused("creds")) && next.contains("unauthorized"), Comment(rawValue: next))
        // the store answered the backend's two marked reads and nothing else
        let gets = rows(k, "creds", "get")
        #expect(
            gets.count == 2 && gets.allSatisfy { $0["secret"] == ["result"] },
            "\(JSON.array(gets).serialize())")
        // the key is in no prompt, no conversation row, no ledger
        #expect(!JSON.array(model.seen.map { JSON.array($0) }).serialize().contains(KEY))
        let dump = await op(k, "memory", ["type": "dump"]).result.serialize()
        #expect(dump.contains(refused("creds")), "the refusal is a conversation row")
        #expect(!dump.contains(KEY))
        let ledger = await op(k, "kernel", ["type": "ledger"]).result.serialize()
        #expect(!ledger.contains(KEY))
        #expect(!JSON.array(k.rows.all().map { JSON.object($0) }).serialize().contains(KEY))
    }

    /// The bypass this pins: `<this kernel's guid>|creds` has no record under that
    /// literal id, yet the kernel delivers it to the local `creds` (and the agent's
    /// `can_call` on `creds` grants it). The holder check resolves it the same way,
    /// so the model's call is refused and never dispatched.
    @Test(.timeLimit(.minutes(1)))
    func theOwnGuidAddressOfAHolderIsRefusedFromAModel() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let own = "g1|creds"
        let model = KeyedScript(required: true, [call(own, args: ["ref": "main"]), DONE])
        let k = await seeded(dir, ["ai": model])
        #expect(k.root?.metaValue(forKey: "guid") == "g1")
        await storeAndAI(k, dir)
        let reached = await op(k, own, ["type": "list"])
        #expect(reached.status == .completed, "\(reached.asJSON.serialize())")
        #expect(
            reached.result.asObject?["refs"] != nil,
            "the kernel resolves the address to creds: \(reached.result.serialize())")
        let before = rows(k, "creds", "get").count

        _ = await op(k, "ai", ["type": "send", "text": "fetch the key"])
        await model.until(passes: 2)
        #expect(model.calls == 2)
        let next = model.seen.dropFirst().first?.last?["content"].asString ?? ""
        #expect(next.contains(refused(own)) && next.contains("unauthorized"), Comment(rawValue: next))
        // the store saw the backend's two marked reads and no call of the model's
        let gets = Array(rows(k, "creds", "get").dropFirst(before))
        #expect(
            gets.count == 2 && gets.allSatisfy { $0["secret"] == ["result"] },
            "\(JSON.array(gets).serialize())")
        #expect(!JSON.array(model.seen.map { JSON.array($0) }).serialize().contains(KEY))
        #expect(!JSON.array(k.rows.all().map { JSON.object($0) }).serialize().contains(KEY))
    }

    @Test(.timeLimit(.minutes(1)))
    func anOperatorMarkedFileBridgeRefusesTheModel() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        try "TOP-SECRET-7f3a\n".write(
            to: dir.appendingPathComponent("vault.txt"), atomically: true, encoding: .utf8)
        let model = KeyedScript(required: false, [call("vault_fs", "read", args: ["path": "vault.txt"]), DONE])
        let k = await seeded(dir, ["fb_ai": model])
        let made = await op(
            k, "kernel",
            .object([
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "file_bridge.tools", "id": "vault_fs", "allow": ["."],
                "ingress_rule": "allow_all_ingress", "callable_by": ["fb_ai": "*"],
            ]))
        #expect(made.result["id"] == "vault_fs", "\(made.result.serialize())")
        let marked = await op(
            k, "kernel", ["type": "update_agent", "id": "vault_fs", "holds_secrets": true])
        #expect(marked.result["updated"] == true, "\(marked.result.serialize())")
        let shown = await op(k, "vault_fs", ["type": "reflect"]).result
        #expect(shown["holds_secrets"] == true, "a holder says so on its reflect")
        await ai(k, "fb_ai", ["vault_fs": "*"])

        _ = await op(k, "fb_ai", ["type": "send", "text": "read the vault"])
        await model.until(passes: 2)
        let next = model.seen.dropFirst().first?.last?["content"].asString ?? ""
        #expect(next.contains(refused("vault_fs")), Comment(rawValue: next))
        #expect(rows(k, "vault_fs", "read").isEmpty, "the read never dispatched")
        #expect(!JSON.array(model.seen.map { JSON.array($0) }).serialize().contains("TOP-SECRET-7f3a"))
    }

    /// The bypass this pins: a model whose agent holds `update_agent` writes
    /// `holds_secrets: false` on the store, then calls it. The KERNEL's gate
    /// refuses the unmark (only the operator lowers the mark), so the store stays
    /// a holder and ai_core still refuses the call — the key never reaches the
    /// model.
    @Test(.timeLimit(.minutes(1)))
    func aModelCannotUnmarkAHolderThenCallIt() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let unmark = call(
            "kernel", "update_agent", args: ["id": "creds", "holds_secrets": false])
        let model = KeyedScript(
            required: true, [unmark, call("creds", args: ["ref": "main"]), DONE])
        let k = await seeded(dir, ["ai": model])
        await storeAndAI(k, dir, ["creds": "*", "kernel_state": ["update_agent"]])

        _ = await op(k, "ai", ["type": "send", "text": "unmark the store, then read it"])
        await model.until(passes: 3)
        let seen = model.seen
        let refusedUnmark = seen.dropFirst().first?.last?["content"].asString ?? ""
        #expect(
            refusedUnmark.contains(
                "ai may not clear holds_secrets on creds: it holds secrets, and only "
                    + "the operator lowers that mark"), Comment(rawValue: refusedUnmark))
        #expect(
            refusedUnmark.contains("secret-holder") && refusedUnmark.contains("unauthorized"),
            Comment(rawValue: refusedUnmark))
        #expect(
            k.agent(AgentId("creds"))?.metaValue(forKey: FantasticAICore.HOLDS_SECRETS)
                == .bool(true), "the mark stayed")
        let next = seen.dropFirst(2).first?.last?["content"].asString ?? ""
        #expect(next.contains(refused("creds")), Comment(rawValue: next))
        let gets = rows(k, "creds", "get")
        #expect(gets.allSatisfy { $0["secret"] == ["result"] }, "\(JSON.array(gets).serialize())")
        #expect(!JSON.array(seen.map { JSON.array($0) }).serialize().contains(KEY))
        #expect(!JSON.array(k.rows.all().map { JSON.object($0) }).serialize().contains(KEY))
    }
}
