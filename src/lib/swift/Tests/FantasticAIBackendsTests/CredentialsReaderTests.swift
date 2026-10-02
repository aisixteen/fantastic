// A backend's key, read from a credential store at USE time (`Credentials.swift`).
// Twin of python `ai_core/tests/test_credentials.py`.
//
// Against a real `credential_store` (file backend under a temp dir): the reader
// asks AS the agent, pinned to the endpoint's host, the answer marked secret;
// nothing is cached, so a rotated key is used by the next request; no value lands
// in the ledger. Never the real keychain or home.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import FantasticRamStore
import Foundation
import OrderedCollections
import Testing

@testable import FantasticCredentialStore
@testable import FantasticResponsesBackend

private let EP = "https://api.example.com/v1/responses"
private let KEY = "sk-live-0123456789"
private let KEY2 = "sk-rotated-9876543210"

private func send(_ k: Kernel, _ target: String, _ payload: OrderedDictionary<String, JSON>, secret: JSON? = nil)
    async -> Outcome
{
    var it = Item(AgentId(target), .object(payload))
    it.secret = secret
    return await k.send([it])[0]
}

private func storeSet(_ k: Kernel, _ ref: String, _ host: String, _ value: String) async {
    let out = await send(
        k, "creds",
        ["type": "set", "ref": .string(ref), "host": .string(host), "fields": ["api_key": .string(value)]],
        secret: ["payload.fields"])
    #expect(out.status == .completed && out.result["stored"] == true, "\(out.asJSON.serialize())")
}

private func create(_ k: Kernel, _ fields: OrderedDictionary<String, JSON>) async -> JSON {
    var payload: OrderedDictionary<String, JSON> = ["type": "create_agent", "parent_id": "kernel_state"]
    for (key, v) in fields { payload[key] = v }
    return await k.send([Item("kernel", .object(payload))])[0].result
}

/// Python `_mk`: a store with the ref `main` (bound to `host`, read by
/// `readers`) and a responses agent `ai` whose record names it.
private func mk(readers: [String] = ["ai"], host: String = "api.example.com") async -> (Kernel, URL) {
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
        "handler_module": .string(RamStoreBundle.handlerModule), "id": "memory", "operator": true,
        "admins": ["kernel"], "callable_by": ["ai": "*"],
    ])
    _ = await create(k, [
        "handler_module": .string(CredentialStoreBundle.handlerModule), "id": "creds",
        "file": .string(dir.appendingPathComponent("creds.yaml").path), "callable_by": ["ai": "*"],
    ])
    await storeSet(k, "main", host, KEY)
    var refs = k.agent(AgentId("creds"))?.meta["refs"]?.asObject ?? [:]
    var entry = refs["main"]?.asObject ?? [:]
    entry["readers"] = .array(readers.map { .string($0) })
    refs["main"] = .object(entry)
    k.update(AgentId("creds"), ["refs": .object(refs)])
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
        await storeSet(k, "main", "api.example.com", KEY2)  // rotate
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

    @Test func anAgentTheRefDoesNotNameReadsNothing() async throws {
        let (k, dir) = await mk(readers: ["someone_else"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let read = keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)
        do {
            _ = try await read(EP)
            Issue.record("a stranger must read nothing")
        } catch {
            #expect(
                pyErrorText(error)
                    == "responses_backend: credentials creds/main: credential_store: no such "
                    + "ref 'main'")
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

    // ─── what one read COSTS in ids (parity, 2026-10-01) ───────────────────
    //
    // A read is ONE send: a call row and a settle row — 2 ids, the same as any
    // unmarked send (a `secret` mark mints nothing of its own) — and, once the
    // kernel's flusher writes them to its memory store, that flush's own 3 (the
    // flush row, the append call, its settle). Measured on python (read-only
    // script, a started kernel on a ram_store): one read = 2 ids, 5 with its
    // flush; 20 reads back to back = 43. Pinned here so a swift read can never
    // mint more than python's.

    @Test func aReadMintsTheCallAndTheSettleOnlyLikeAnyUnmarkedSend() async throws {
        let (k, dir) = await mk()
        defer { try? FileManager.default.removeItem(at: dir) }
        let read = keyReader(AgentId("ai"), k, backend: "responses_backend", required: true)
        var n0 = k.nextId
        #expect(try await read(EP) == KEY)
        #expect(k.nextId - n0 == 2, "a credential read is one call and one settle")
        n0 = k.nextId
        _ = await KernelTaskLocals.$currentSender.withValue(AgentId("ai")) {
            await k.send([Item("creds", ["type": "list"])])[0]
        }
        #expect(k.nextId - n0 == 2, "the same as an unmarked send")
    }

    @Test func aReadAndItsFlushCostPythonsFive() async throws {
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
        for _ in 0..<50 where k.nextId - n0 < 5 { try await Task.sleep(nanoseconds: 100_000_000) }
        await quiet(k)
        #expect(k.nextId - n0 == 5, "call + settle, then the flush row, its append call and settle")
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
        let out = await send(
            k, "creds",
            ["type": "set", "ref": "tok", "host": "api.example.com", "fields": ["token": "t"]],
            secret: ["payload.fields"])
        #expect(out.status == .completed)
        var refs = k.agent(AgentId("creds"))?.meta["refs"]?.asObject ?? [:]
        var entry = refs["tok"]?.asObject ?? [:]
        entry["readers"] = ["ai"]
        refs["tok"] = .object(entry)
        k.update(AgentId("creds"), ["refs": .object(refs)])
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
}
