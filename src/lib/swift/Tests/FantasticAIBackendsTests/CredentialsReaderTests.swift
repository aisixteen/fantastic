// A backend's key, read from a credential store at USE time (`Credentials.swift`).
// Twin of python `ai_core/tests/test_credentials.py`.
//
// Against a real `credential_store` (file backend under a temp dir): the reader
// asks AS the agent, pinned to the endpoint's host; nothing is cached, so a
// rotated key is used by the next request; no value lands in the ledger (an
// unnamed call's settle keeps a digest). Never the real keychain or home.

import FantasticAICore
import FantasticJSON
@testable import FantasticKernel
import FantasticRamStore
import Foundation
import OrderedCollections
import Testing

@testable import FantasticCredentialStore
@testable import FantasticResponsesBackend

private let EP = "https://api.example.com/v1/responses"
private let KEY = "sk-live-0123456789"
private let KEY2 = "sk-rotated-9876543210"

private func send(_ k: Kernel, _ target: String, _ payload: OrderedDictionary<String, JSON>)
    async -> Outcome
{
    await k.send([Item(AgentId(target), .object(payload))])[0]
}

private func storeSet(_ k: Kernel, _ ref: String, _ value: String) async {
    let out = await send(
        k, "creds",
        ["type": "set", "ref": .string(ref), "fields": ["api_key": .string(value)]])
    #expect(out.status == .completed && out.result["stored"] == true, "\(out.asJSON.serialize())")
}

private func create(_ k: Kernel, _ fields: OrderedDictionary<String, JSON>) async -> JSON {
    var payload: OrderedDictionary<String, JSON> = ["type": "create_agent", "parent_id": "kernel_state"]
    for (key, v) in fields { payload[key] = v }
    return await k.send([Item("kernel", .object(payload))])[0].result
}

/// Python `_mk`: a store with the ref `main` (bound to `host`, read by
/// whoever `accept` names in its `callable_by`) and a responses agent `ai`
/// whose record names it.
private func mk(accept: [String] = ["ai"], host: String = "api.example.com") async -> (Kernel, URL) {
    let r = BundleRegistry()
    r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: NoKeychain()))
    r.register(HANDLER_MODULE, ResponsesBundle())
    r.register(RamStoreBundle.handlerModule, RamStoreBundle())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": "g1"])
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("cred-ai-\(UUID().uuidString.prefix(8))")
    _ = await create(k, [
        "handler_module": .string(RamStoreBundle.handlerModule), "id": "memory",
        "callable_by": ["ai": "*"],
    ])
    _ = await create(k, [
        "handler_module": .string(CredentialStoreBundle.handlerModule), "id": "creds",
        "file": .string(dir.appendingPathComponent("creds.yaml").path),
        // declared
        "refs": ["main": ["host": .string(host)]],
        "callable_by": .object(OrderedDictionary(uniqueKeysWithValues: accept.map { ($0, JSON.string("*")) })),
    ])
    await storeSet(k, "main", KEY)
    let rec = await create(k, [
        "handler_module": .string(HANDLER_MODULE), "id": "ai", "endpoint": .string(EP),
        "model": "m", "memory_store_id": "memory",
        "credentials": ["store": "creds", "ref": "main"],
        "can_call": ["creds": "*", "memory": "*"],
    ])
    #expect(rec["id"] == "ai", "\(rec.serialize())")
    return (k, dir)
}

/// The keychain is never reached here (file backend).
private struct NoKeychain: KeychainStore {
    func get(service: String, account: String) throws -> String? { nil }
    func set(service: String, account: String, value: String) throws {}
    func delete(service: String, account: String) throws {}
}

@Suite("credentials — the key reader")
struct CredentialsReaderTests {

    @Test func theProviderReadsTheKeyPerRequestAndRecordsNone() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        let agent = try #require(k.agent(AgentId("ai")))
        let built = try await ResponsesBundle().core.config.makeProvider(agent, "c", k)
        let prov = try #require(built as? ResponsesProvider)
        #expect(try await prov.key(EP) == KEY)
        await storeSet(k, "main", KEY2)  // rotate
        #expect(try await prov.key(EP) == KEY2, "a rotated key is used by the next request")
        let ledger = await k.send([Item("kernel", ["type": "ledger"])])[0].result.serialize()
        #expect(!ledger.contains(KEY) && !ledger.contains(KEY2))
    }

    @Test func aKeyBoundToAnotherHostIsRefused() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        let read = keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)
        do {
            _ = try await read("https://evil.example.net/v1/responses")
            Issue.record("a key bound to another host must not be read")
        } catch {
            #expect(pyErrorText(error).contains("bound to host 'api.example.com'"), "\(error)")
            #expect((error as? PyError)?.type == "CredentialError")
        }
    }

    @Test func anAgentTheStoreDoesNotAcceptReadsNothing() async throws {
        let (k, dir) = await mk(accept: ["someone_else"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let read = keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)
        do {
            // the agent's own turn, which the test stands in for (python `as_agent`)
            _ = try await KernelTaskLocals.$sender.withValue(AgentId("ai")) { try await read(EP) }
            Issue.record("a stranger must read nothing")
        } catch {
            #expect(
                pyErrorText(error).contains("creds does not accept 'get' from ai"),
                "\(pyErrorText(error))")
        }
    }

    @Test func noCredentialsIsNoKeyOrALoudRefusal() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        k.register(Agent(id: AgentId("bare"), handlerModule: nil, parentId: AgentId("kernel_state")))
        #expect(try await keyReader(AgentId("bare"), k, backend: "vllm_backend", required: false)(EP) == nil)
        do {
            _ = try await keyReader(AgentId("bare"), k, backend: "responses_backend", required: true)(EP)
            Issue.record("a required key with no credentials must refuse")
        } catch {
            #expect(
                pyErrorText(error)
                    == "responses_backend: no `credentials` on bare — name the key's credential "
                    + "store and ref (update_agent bare credentials={store: <id>, ref: <ref>})")
        }
    }

    // ─── what one read COSTS in ids (parity) ───────────────────
    //
    // A read is ONE send: a call row and a settle row — 2 ids, the same as any
    // other send — and, once the
    // kernel's flusher writes them to its memory store, that flush's own ONE (the
    // flush row: the flush is the kernel's own call into the store, not a send,
    // so it mints no call or settle row of its own). Pinned here so a swift read
    // can never mint more than python's.

    @Test func aReadMintsTheCallAndTheSettleOnlyLikeAnyOtherSend() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        let read = keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)
        var n0 = k.nextId
        #expect(try await read(EP) == KEY)
        #expect(k.nextId - n0 == 2, "a credential read is one call and one settle")
        n0 = k.nextId
        _ = await KernelTaskLocals.$sender.withValue(AgentId("ai")) {
            await k.send([Item("creds", ["type": "list"])])[0]
        }
        #expect(k.nextId - n0 == 2, "the same as any other send")
    }

    @Test func aReadAndItsFlushCostThree() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        k.update(AgentId("kernel_state"), ["memory_store_id": "memory"])
        let started = await k.start(bootAll: false)
        #expect(started["started"] == true, "\(started.serialize())")
        let read = keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)
        await quiet(k)  // the start's own rows are flushed first
        let n0 = k.nextId
        #expect(try await read(EP) == KEY)
        #expect(k.nextId - n0 == 2)
        // the debounced flush (0.15 s) — waited for, bounded, never assumed
        for _ in 0..<50 where k.nextId - n0 < 3 { try await Task.sleep(nanoseconds: 100_000_000) }
        await quiet(k)
        #expect(k.nextId - n0 == 3, "call + settle, then the flush row")
    }

    /// Until the id counter has not moved for half a second (bounded at 10 s).
    private func quiet(_ k: Kernel) async {
        var last = k.nextId
        for _ in 0..<20 {
            try? await Task.sleep(nanoseconds: 500_000_000)
            if k.nextId == last { return }
            last = k.nextId
        }
    }

    /// The shape is checked again at the read (a record written before the
    /// field was validated), and a ref without `api_key` is named.
    @Test func aMalformedCredentialsOrAMissingFieldIsNamed() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        k.update(AgentId("ai"), ["credentials": ["store": "creds"]])
        do {
            _ = try await keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)(EP)
            Issue.record("a malformed credentials must refuse")
        } catch {
            #expect(
                pyErrorText(error)
                    == "responses_backend: credentials must be {store: <credential_store id>, ref: <ref>}")
        }
        // declared first — a set never creates a ref
        var refs = k.agent(AgentId("creds"))?.meta["refs"]?.asObject ?? [:]
        refs["tok"] = ["host": "api.example.com"]
        k.update(AgentId("creds"), ["refs": .object(refs)])
        let out = await send(
            k, "creds",
            ["type": "set", "ref": "tok", "fields": ["token": "t"]])
        #expect(out.status == .completed, "\(out.asJSON.serialize())")
        k.update(AgentId("ai"), ["credentials": ["store": "creds", "ref": "tok"]])
        do {
            _ = try await keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)(EP)
            Issue.record("a ref without api_key must refuse")
        } catch {
            #expect(
                pyErrorText(error)
                    == "responses_backend: credential 'tok' in 'creds' has no 'api_key' field")
        }
    }

    /// A key that is not a legal HTTP header value — a control
    /// character, a non-ASCII one, whitespace at an edge — is refused by the
    /// reader, naming the ref and never the value (python's client quotes a
    /// refused header in its error). The store keeps whatever it was given; a key
    /// with inner spaces rides.
    @Test func aKeyThatIsNotALegalHeaderValueIsRefusedByItsRef() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        let read = keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)
        let bad = [
            KEY + "\n", KEY + "\r\nX-Probe: y", KEY + " ", " " + KEY, KEY + "\t",
            "sk-l\u{0}ive", "sk-l\u{7f}ive", KEY + "é",
        ]
        for b in bad {
            await storeSet(k, "main", b)
            do {
                _ = try await read(EP)
                Issue.record("an illegal header value must not be read: \(b.debugDescription)")
            } catch {
                #expect((error as? PyError)?.type == "CredentialError")
                #expect(
                    pyErrorText(error)
                        == "responses_backend: credential 'main' in 'creds': 'api_key' is not a "
                        + "legal HTTP header value (a control or non-ASCII character, or "
                        + "whitespace at an edge) — store it again without",
                    "\(b.debugDescription)")
                #expect(!pyErrorText(error).contains("0123456789"))
            }
        }
        await storeSet(k, "main", "sk a\tb")
        #expect(try await read(EP) == "sk a\tb")
    }
}
