// vllm_backend — the wire it speaks, the turns it must not lose, and the
// KV-cache session table it keeps per endpoint. Twins of python
// `vllm_backend/tests/test_vllm_backend.py`.
//
// Every behaviour here traces to a measured failure on a live vLLM (the dataroom
// deployment, Qwen3.8-27B-FP8, vLLM 0.27.1): a dropped thought channel, a turn
// spent thinking with no answer, a stream that died without saying so, a system
// message the template refused, and a cache that only pays off while the live
// set stays under the server's KV ceiling.
//
// The session tables are PROCESS-global and keyed by endpoint, and swift-testing
// runs tests in parallel — so every test names its OWN endpoint (a unique path
// under the stub's port; the stub ignores the path). Python clears its tables
// per test instead.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticVllmBackend

/// A fresh endpoint no other test shares.
private func endpoint(port: Int) -> String {
    "http://127.0.0.1:\(port)/\(UUID().uuidString.prefix(8))/v1"
}

private func sse(_ chunks: [JSON], done: Bool = true) -> StubHTTP.Reply {
    var lines = chunks.map { "data: \($0.serialize())" }
    if done { lines.append("data: [DONE]") }
    return .sse(lines.joined(separator: "\n\n") + "\n\n")
}

private func delta(finish: String? = nil, _ fields: OrderedDictionary<String, JSON> = [:]) -> JSON {
    .object([
        "choices": .array([
            .object([
                "delta": .object(fields),
                "finish_reason": finish.map { .string($0) } ?? .null,
            ])
        ])
    ])
}

private func provider(
    _ endpoint: String,
    maxTokens: Int = 8000,
    kvSlots: Int = 7,
    kvOverflow: String = "evict",
    sampling: OrderedDictionary<String, JSON> = SAMPLING_DEFAULTS,
    explicitBudget: Bool = false,
    agentKey: String = UUID().uuidString,
    kernelSerial: UInt64 = providerKernel.serial,
    key: @escaping KeyReader = fixedKey(nil)
) -> VllmProvider {
    VllmProvider(
        endpoint: endpoint, model: "qwen3.8-27b", key: key, maxTokens: maxTokens,
        kvSlots: kvSlots, kvOverflow: kvOverflow, kvTtl: KV_TTL,
        sampling: sampling, explicitBudget: explicitBudget,
        connectTimeout: CONNECT_TIMEOUT, readTimeout: READ_TIMEOUT,
        agentKey: agentKey, kernelSerial: kernelSerial,
        clock: { ProcessInfo.processInfo.systemUptime })
}

/// The kernel a bare provider's tables belong to — the provider tests drive the
/// wire, not a kernel; one kernel serial keys their session tables.
private let providerKernel = Kernel(storage: .inMemory, bundles: BundleRegistry())

private let oneUser: [JSON] = [.object(["role": "user", "content": "hi"])]

/// The streamed text, or the error text it failed with.
private func drain(_ p: VllmProvider, _ messages: [JSON] = oneUser, maxTokens: Int? = nil)
    async -> (String, String?)
{
    var text = ""
    do {
        for try await chunk in p.chat(messages: messages, maxTokens: maxTokens) {
            if case .token(let t) = chunk { text += t }
        }
        return (text, nil)
    } catch {
        return (text, "\(error)")
    }
}

/// The JSON body of the n-th request the stub received.
private func body(_ stub: StubHTTP, _ n: Int = 0) throws -> JSON {
    let raw = stub.requests[n]
    let start = raw.range(of: "\r\n\r\n")!.upperBound
    return try JSON.parse(String(raw[start...]))
}

@Suite("vLLM backend")
struct VllmBackendTests {

    // MARK: the stream

    /// python `test_a_cap_that_is_the_whole_window_sends_no_max_tokens`: vLLM counts
    /// prompt AND output against one window, so a turn capped at the window asks for
    /// the rest of it — no `max_tokens`; a side call keeps its own bound; a cap below
    /// the window is sent as set.
    @Test func aCapThatIsTheWholeWindowSendsNoMaxTokens() async throws {
        let stub = StubHTTP([
            sse([delta(finish: "stop")]), sse([delta(finish: "stop")]), sse([delta(finish: "stop")]),
        ])
        defer { stub.stop() }
        var p = provider(endpoint(port: stub.port), maxTokens: 262144)
        p.window = 262144
        _ = await drain(p)  // the turn
        _ = await drain(p, maxTokens: 2000)  // a fold summary
        var q = provider(endpoint(port: stub.port))
        q.window = 262144
        _ = await drain(q)  // a cap below the window
        #expect(try body(stub, 0).asObject?["max_tokens"] == nil)
        #expect(try body(stub, 1)["max_tokens"] == 2000)
        #expect(try body(stub, 2)["max_tokens"] == 8000)
    }

    @Test func theThoughtChannelIsWrappedOnceNotDropped() async {
        let stub = StubHTTP([
            sse([
                delta(["reasoning_content": "let me "]),
                delta(["reasoning": "think"]),  // the other spelling a version may use
                delta(["content": "answer"]),
                delta(finish: "stop"),
                .object(["choices": .array([]), "usage": .object(["prompt_tokens": 42])]),
            ])
        ])
        defer { stub.stop() }
        let p = provider(endpoint(port: stub.port))
        let (text, error) = await drain(p)
        #expect(error == nil)
        #expect(text == "\(THINK_OPEN)let me think\(THINK_CLOSE)answer")
        #expect(p.usage?.lastStopReason == "stop")
        #expect(p.usage?.lastPromptTokens == 42)
    }

    @Test func aCutMainTurnFailsNamingTheCut() async {
        // the record's bound hit on a MAIN turn: it did not finish — never an answer
        let stub = StubHTTP([sse([delta(["content": "half"]), delta(finish: "length")])])
        defer { stub.stop() }
        let p = provider(endpoint(port: stub.port))
        let (_, error) = await drain(p)
        #expect(error?.contains("cut at max_tokens=8000") == true, "\(error ?? "nil")")
        #expect(p.usage?.lastStopReason == "max_tokens")
    }

    @Test func aCutBoundedSideCallIsItsAnswer() async {
        // a fold summary passes its own bound: stopping there is what it asked for
        let stub = StubHTTP([sse([delta(["content": "half"]), delta(finish: "length")])])
        defer { stub.stop() }
        let p = provider(endpoint(port: stub.port))
        let (text, error) = await drain(p, maxTokens: 50)
        #expect(error == nil, "\(error ?? "nil")")
        #expect(text == "half")
        #expect(p.usage?.lastStopReason == "max_tokens")
    }

    @Test func aStreamThatEndsWithoutFinishReasonIsAnError() async {
        // vLLM died mid-answer: the text so far must never read as a completion
        let stub = StubHTTP([sse([delta(["content": "partial"])], done: false)])
        defer { stub.stop() }
        let (_, error) = await drain(provider(endpoint(port: stub.port)))
        #expect(error?.contains("without a finish_reason") == true, "\(error ?? "nil")")
    }

    @Test func anHTTPErrorNamesTheEndpointAndTheBody() async {
        let stub = StubHTTP([.init(status: 400, body: #"{"error":"bad effort"}"#)])
        defer { stub.stop() }
        let ep = endpoint(port: stub.port)
        let (_, error) = await drain(provider(ep))
        #expect(
            error
                == #"RuntimeError: vllm_backend: \#(ep) answered HTTP 400: {"error":"bad effort"}"#)
    }

    // MARK: the key (an HTTPS door in front of the GPU)

    /// Header only when set: no `credentials`, NO `Authorization` at all (a
    /// keyless local vLLM); with them, a Bearer header, read before EACH request
    /// — a rotated key is used by the next one; the reader is told the endpoint.
    /// Python `test_the_key_rides_as_a_bearer_header_only_when_set`.
    @Test func theKeyRidesAsABearerHeaderOnlyWhenSet() async {
        let ok = sse([delta(["content": "ok"]), delta(finish: "stop")])
        let bare = StubHTTP([ok])
        defer { bare.stop() }
        #expect(await drain(provider(endpoint(port: bare.port))).0 == "ok")
        let keyed = StubHTTP([ok, ok])
        defer { keyed.stop() }
        let ep = endpoint(port: keyed.port)
        let asked = KeyLog(["k-123", "k-rotated"])
        let p = provider(ep, key: { endpoint in asked.next(endpoint) })
        #expect(await drain(p).0 == "ok")
        #expect(await drain(p).0 == "ok")
        let auth = { (raw: String) in
            raw.split(separator: "\r\n").first { $0.lowercased().hasPrefix("authorization:") }
                .map(String.init)
        }
        #expect(auth(bare.requests[0]) == nil, "\(bare.requests[0])")
        #expect(auth(keyed.requests[0]) == "Authorization: Bearer k-123", "\(keyed.requests[0])")
        #expect(auth(keyed.requests[1]) == "Authorization: Bearer k-rotated", "\(keyed.requests[1])")
        #expect(asked.endpoints == [ep, ep])
    }

    @Test func aRefusedKeySaysToNameCredentials() async {
        let stub = StubHTTP([.init(status: 401, body: "unauthorized")])
        defer { stub.stop() }
        let ep = endpoint(port: stub.port)
        let (_, error) = await drain(provider(ep))
        #expect(
            error
                == "RuntimeError: vllm_backend: \(ep) answered HTTP 401 — the door wants a key: "
                + "name it in `credentials` ({store, ref}): unauthorized", "\(error ?? "nil")")
    }

    /// No `key_var`: the key is an optional `credentials` field (a store ref).
    @Test func theSchemaTakesOptionalCredentials() {
        #expect(VllmBundle.RECORD_SCHEMA.asObject?["key_var"] == nil)
        let f = (VllmBundle.RECORD_SCHEMA["fields"].asArray ?? []).first {
            $0["name"] == "credentials"
        }
        #expect(f?["type"] == "credentials" && f?["required"] == .bool(false))
    }

    @Test func aRefusedConnectionSaysVLLMIsNotListeningYet() async {
        // a real refused port: bind a stub, take its port, close it
        let stub = StubHTTP([])
        let port = stub.port
        stub.stop()
        let (_, error) = await drain(provider(endpoint(port: port)))
        #expect(error?.contains("not listening yet") == true, "\(error ?? "nil")")
    }

    /// The two failures a test cannot produce on demand (a stopped instance
    /// dropping SYNs; a stream torn mid-body), pinned on the classifier.
    @Test func eachTransportFailureSaysWhatItMeans() {
        func text(_ e: Error, _ phase: VllmPhase) -> String {
            "\(vllmTransportError(e, endpoint: "http://h/v1", connectTimeout: 5, readTimeout: 900, phase: phase))"
        }
        #expect(
            text(HTTPClientError.connectTimeout, .connect)
                == "RuntimeError: vllm_backend: no answer from http://h/v1 within connect_timeout=5s — "
                + "the instance is STOPPED (its SYNs are dropped), or the tunnel/port is wrong")
        #expect(
            text(HTTPClientError.readTimeout, .stream)
                == "RuntimeError: vllm_backend: http://h/v1 sent nothing for read_timeout=900s")
        #expect(text(HTTPClientError.remoteConnectionClosed, .stream).contains("died or restarted"))
        // python maps ONLY httpx's connect/read/protocol errors: a request the
        // client refuses before connecting (no scheme, a bad URL) propagates
        // with its own text — it is not a refused connection.
        for e in [HTTPClientError.emptyScheme, .invalidURL, .unsupportedScheme("ftp")] {
            let out = vllmTransportError(
                e, endpoint: "h/v1", connectTimeout: 5, readTimeout: 900, phase: .connect)
            #expect((out as? HTTPClientError) == e, "\(out)")
        }
        // a peer that hangs up before any head is python's RemoteProtocolError
        #expect(
            text(HTTPClientError.remoteConnectionClosed, .connect).contains("died or restarted"))
        #expect(
            vllmTransportError(
                CancellationError(), endpoint: "e", connectTimeout: 5, readTimeout: 900,
                phase: .stream)
                is CancellationError)
    }

    // MARK: the request

    @Test func theBodyCarriesTheDefaultsAndOnlyWhatTheRecordSet() async throws {
        let stub = StubHTTP([sse([delta(finish: "stop")])])
        defer { stub.stop() }
        var sampling = SAMPLING_DEFAULTS
        sampling["seed"] = 7
        _ = await drain(provider(endpoint(port: stub.port), sampling: sampling))
        let b = try body(stub)
        #expect(b["model"] == "qwen3.8-27b" && b["max_tokens"] == 8000)
        #expect(b["stream"] == .bool(true))
        #expect(b["stream_options"] == .object(["include_usage": .bool(true)]))
        #expect(b["temperature"] == .double(0.2) && b["top_p"] == .double(0.8) && b["top_k"] == 20)
        #expect(b["reasoning_effort"] == "medium" && b["seed"] == 7)
        // never defaulted: a penalty is sent only when the record sets one
        #expect(b.asObject?["presence_penalty"] == nil && b.asObject?["frequency_penalty"] == nil)
    }

    @Test func everySystemMessageBecomesOneAtIndex0() async throws {
        let msgs: [JSON] = [
            .object(["role": "system", "content": "A"]),
            .object(["role": "user", "content": "q"]),
            .object(["role": "system", "content": "B"]),
        ]
        #expect(
            oneSystemMessage(msgs) == [
                .object(["role": "system", "content": "A\n\nB"]),
                .object(["role": "user", "content": "q"]),
            ])
        let stub = StubHTTP([sse([delta(finish: "stop")])])
        defer { stub.stop() }
        _ = await drain(provider(endpoint(port: stub.port)), msgs)
        let roles = try body(stub)["messages"].asArray?.map { $0["role"] }
        #expect(roles == ["system", "user"])
    }

    @Test func theDefaultBudgetHalvesOnASmallerCallAndNoneBelowTheFloor() async throws {
        let stub = StubHTTP([
            sse([delta(finish: "stop")]), sse([delta(finish: "stop")]), sse([delta(finish: "stop")]),
        ])
        defer { stub.stop() }
        let key = UUID().uuidString
        let p = provider(endpoint(port: stub.port), agentKey: key)
        _ = await drain(p)  // the turn: max_tokens 8000 → min(16000, 4000)
        _ = await drain(p, maxTokens: 2000)  // a fold summary → min(16000, 1000)
        _ = await drain(p, maxTokens: 40)  // half is 20, under the 256 floor → none
        #expect(try body(stub, 0)["thinking_token_budget"] == 4000)
        #expect(try body(stub, 1)["thinking_token_budget"] == 1000)
        #expect(try body(stub, 2).asObject?["thinking_token_budget"] == nil)
        #expect(VllmLastBudget.get(providerKernel.serial, key) == nil)
    }

    @Test func anExplicitBudgetRulesTheTurnAndHalvesOnASideCall() async throws {
        // the proxy runs' 8000: a main turn sends it as set; a fold summary
        // bounded at 2000 gets 1000 instead of being refused; a tiny call gets none
        let stub = StubHTTP([
            sse([delta(finish: "stop")]), sse([delta(finish: "stop")]), sse([delta(finish: "stop")]),
        ])
        defer { stub.stop() }
        var sampling = SAMPLING_DEFAULTS
        sampling["thinking_token_budget"] = 8000
        let p = provider(
            endpoint(port: stub.port), maxTokens: 65536, sampling: sampling, explicitBudget: true)
        _ = await drain(p)
        _ = await drain(p, maxTokens: 2000)
        _ = await drain(p, maxTokens: 300)
        #expect(try body(stub, 0)["thinking_token_budget"] == 8000)
        #expect(try body(stub, 1)["thinking_token_budget"] == 1000)
        #expect(try body(stub, 2).asObject?["thinking_token_budget"] == nil)
    }

    // MARK: the KV-cache session table

    @Test func evictRotatesTheLeastRecentlyUsedConversationOut() throws {
        let t = VllmSessionTable()
        for (i, k) in ["a", "b", "c"].enumerated() {
            #expect(try t.admit(k, slots: 3, overflow: "evict", ttl: 1800, now: Double(i)) == nil)
        }
        // a is used again: b is now the idlest
        _ = try t.admit("a", slots: 3, overflow: "evict", ttl: 1800, now: 10)
        #expect(try t.admit("d", slots: 3, overflow: "evict", ttl: 1800, now: 11) == "b")
        #expect(t.view()["conversations"] == ["c", "a", "d"])
        #expect(t.view()["evicted"] == 1)
    }

    @Test func rejectRefusesTheNewcomerAndNamesWhenToRetry() throws {
        let t = VllmSessionTable()
        _ = try t.admit("a", slots: 1, overflow: "reject", ttl: 1800, now: 0)
        do {
            _ = try t.admit("b", slots: 1, overflow: "reject", ttl: 1800, now: 100)
            Issue.record("a full reject table admitted a newcomer")
        } catch {
            #expect(error.type == "KvSlotsFull")
            #expect(error.message.contains("retry in ~1700s"), "\(error.message)")
        }
        #expect(t.view()["conversations"] == ["a"])
        #expect(t.view()["rejected"] == 1)
    }

    @Test func anIdleConversationFreesItsSlotAfterTheTtl() throws {
        let t = VllmSessionTable()
        _ = try t.admit("a", slots: 1, overflow: "reject", ttl: 60, now: 0)
        #expect(try t.admit("b", slots: 1, overflow: "reject", ttl: 60, now: 61) == nil)
        #expect(t.view()["conversations"] == ["b"])
    }

    @Test func theTableIsSharedPerEndpointAcrossAgents() async {
        // two agents on ONE endpoint share the count; a second endpoint does not
        let stub = StubHTTP(Array(repeating: sse([delta(finish: "stop")]), count: 3))
        defer { stub.stop() }
        let shared = endpoint(port: stub.port)
        let p1 = provider(shared, kvSlots: 1, kvOverflow: "reject")
        let p2 = provider(shared, kvSlots: 1, kvOverflow: "reject")
        let other = provider(endpoint(port: stub.port), kvSlots: 1, kvOverflow: "reject")
        let first: [JSON] = [.object(["role": "user", "content": "first"])]
        let second: [JSON] = [.object(["role": "user", "content": "second"])]
        #expect(await drain(p1, first).1 == nil)
        #expect(await drain(p2, second).1?.hasPrefix("KvSlotsFull: ") == true)
        #expect(await drain(other, second).1 == nil)
        // the SAME conversation on the full endpoint is already live: admitted
        #expect(await drain(p2, first).1 == nil)
    }

    @Test func aConversationIsItsPrefix() {
        let base: [JSON] = [
            .object(["role": "system", "content": "S"]), .object(["role": "user", "content": "U"]),
        ]
        let grown =
            base + [
                .object(["role": "assistant", "content": "A"]),
                .object(["role": "user", "content": "U2"]),
            ]
        #expect(conversationKey(base) == conversationKey(grown))  // append-only: same
        let rewritten = [.object(["role": "system", "content": "S2"])] + base.dropFirst()
        #expect(conversationKey(Array(rewritten)) != conversationKey(base))
    }

    // MARK: the write

    @Test(
        arguments: [
            (["reasoning_effort": "high"], [:], "legal values: low, medium, xhigh"),
            (["kv_overflow": "drop"], [:], "legal values: evict"),
            (["kv_slots": 0], [:], "kv_slots must be a positive int"),
            (["max_tokens": .bool(true)], [:], "max_tokens must be a positive int"),
            (["kv_ttl": -1], [:], "kv_ttl must be a positive number"),
            (["thinking_token_budget": 8000], ["max_tokens": 8000], "not below max_tokens=8000"),
            (["max_tokens": 1000], ["thinking_token_budget": 2000], "not below max_tokens=1000"),
        ] as [(OrderedDictionary<String, JSON>, OrderedDictionary<String, JSON>, String)])
    func aWriteTheBackendCannotHonourIsRefused(
        meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>,
        fragment: String
    ) {
        let why = VllmBundle().validateMeta(meta, record: record) ?? ""
        #expect(why.contains(fragment), "\(why)")
    }

    @Test func aRecordMissingRequiredFieldsIsRefusedAtTheWriteNamingAll() {
        // what a model spawning a sub-AI wrote (E37): the fields it could see
        let meta: OrderedDictionary<String, JSON> = [
            "endpoint": .string("http://vllm.test/v1"), "model": "qwen3.8-27b",
            "max_tokens": 65536,
        ]
        let err = VllmBundle().validateMeta(meta, record: [:]) ?? ""
        #expect(
            err.contains("missing: kv_slots, kv_overflow, memory_store_id, context_window"),
            "\(err)")
        // each missing field is named WITH its hint, from the schema
        #expect(err.contains("kv_overflow = all slots live: evict"), "\(err)")
        // an update to a complete record is judged on the MERGED state
        var full = meta
        full["kv_slots"] = 7
        full["kv_overflow"] = "evict"
        full["memory_store_id"] = "memory"
        full["context_window"] = 262144
        #expect(VllmBundle().validateMeta(["seed": 3], record: full) == nil)
    }

    @Test func aConsistentWritePasses() {
        let meta: OrderedDictionary<String, JSON> = [
            "endpoint": "http://vllm.test/v1", "model": "qwen3.8-27b", "max_tokens": 8000,
            "kv_slots": 6, "kv_overflow": "evict", "thinking_token_budget": 4000,
            "reasoning_effort": "xhigh", "memory_store_id": "memory", "context_window": 262144,
        ]
        #expect(VllmBundle().validateMeta(meta, record: [:]) == nil)
    }

    @Test func theRequiredFieldsAreNamedInPythonsOrder() {
        func first(_ meta: OrderedDictionary<String, JSON>) -> String {
            let a = Agent(id: AgentId("v"), handlerModule: nil, parentId: nil, meta: meta)
            do {
                _ = try VllmBundle.makeProvider(a, kernel: providerKernel, key: fixedKey(nil))
                return ""
            } catch {
                return "\(error)"
            }
        }
        #expect(first([:]).contains("`kv_overflow` is required"))
        #expect(first(["kv_overflow": "evict"]).contains("`endpoint` is required"))
        #expect(
            first(["kv_overflow": "evict", "endpoint": "http://h/v1", "model": "m"])
                == "ValueError: vllm_backend: `max_tokens` is required on the agent record, a "
                + "positive int — nothing is guessed (update_agent <id> max_tokens=…)")
    }

    @Test func readmeIsPythonVerbatim() throws {
        let py = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent(
                "python/bundled_agents/ai/vllm/vllm_backend/src/vllm_backend/readme.md")
        #expect(VllmBundle().readme == (try String(contentsOf: py, encoding: .utf8)))
    }

    // MARK: through the kernel

    private func kernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register(HANDLER_MODULE, VllmBundle())
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        return k
    }

    private func create(_ k: Kernel, _ meta: OrderedDictionary<String, JSON>) async -> JSON {
        var payload: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": .string(HANDLER_MODULE),
        ]
        for (key, v) in meta { payload[key] = v }
        return await k.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
    }

    @Test func theKernelRefusesABadRecordAtTheWrite() async {
        let k = kernel()
        let r = await create(k, ["id": "bad", "max_tokens": 4000, "thinking_token_budget": 4000])
        #expect(r["reason"] == "invalid_meta", "\(r)")
        #expect(r["error"].asString?.contains("not below max_tokens=4000") == true, "\(r)")
    }

    @Test func reflectShowsTheEffectivePostureAndTheSharedTable() async {
        let k = kernel()
        let r = await create(
            k,
            [
                "id": "v1",
                "endpoint": .string("http://vllm.test/\(UUID().uuidString.prefix(8))/v1"),
                "model": "qwen3.8-27b", "max_tokens": 8000, "kv_slots": 7, "kv_overflow": "evict",
                "memory_store_id": "memory", "seed": 3, "context_window": 262144,
            ])
        #expect(r["id"] == "v1", "\(r)")
        let out = await k.send([Item(AgentId("v1"), ["type": "reflect"])])[0].result
        #expect(out["sampling"]["temperature"] == .double(0.2) && out["sampling"]["seed"] == 3)
        #expect(out["thinking_token_budget_source"] == "default")
        #expect(out["timeouts"] == .object(["connect": .double(5.0), "read": .double(900.0)]))
        #expect(
            out["kv"]
                == .object([
                    "slots": 7, "overflow": "evict", "ttl": .double(1800.0), "live": 0,
                    "conversations": .array([]), "evicted": 0, "rejected": 0,
                ]), "\(out["kv"])")
    }
}

/// Audit ARCH2-5: the KV session table and the last-budget entry are keyed by
/// kernel. Two kernels in one process with an agent of the same id on the same
/// endpoint keep apart, and a kernel's teardown drops its rows.
@Suite("vLLM side tables — keyed by kernel")
struct VllmSideTablePinTests {
    @Test func twoKernelsOnOneEndpointShareNoTableAndTeardownDropsTheirRows() async throws {
        VllmSideTables.installTeardownHook()
        func bare() -> Kernel {
            let k = Kernel(storage: .inMemory, bundles: BundleRegistry())
            let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
            k.register(root)
            k.setRoot(root)
            return k
        }
        let a = bare()
        let b = bare()
        let endpoint = "http://vllm.test/\(UUID().uuidString.prefix(8))/v1"
        _ = try VllmTables.table(a.serial, for: endpoint).admit(
            "conv", slots: 1, overflow: "reject", ttl: 1800, now: 0)
        #expect(VllmTables.table(a.serial, for: endpoint).view()["live"] == 1)
        #expect(VllmTables.table(b.serial, for: endpoint).view()["live"] == 0)
        // kernel b's own conversation is admitted: a's live slot is not b's
        #expect(
            try VllmTables.table(b.serial, for: endpoint).admit(
                "other", slots: 1, overflow: "reject", ttl: 1800, now: 0) == nil)
        VllmLastBudget.set(a.serial, "v1", 4000)
        #expect(VllmLastBudget.get(a.serial, "v1") == 4000)
        #expect(VllmLastBudget.get(b.serial, "v1") == nil)
        await a.shutdown()
        #expect(VllmLastBudget.get(a.serial, "v1") == nil)
        #expect(VllmTables.table(a.serial, for: endpoint).view()["live"] == 0)
        #expect(VllmTables.table(b.serial, for: endpoint).view()["live"] == 1)
        await b.shutdown()
    }
}

/// A key reader's answers, in turn, and the endpoints it was asked for.
private final class KeyLog: @unchecked Sendable {
    private let lock = NSLock()
    private var answers: [String]
    private var asked: [String] = []
    init(_ answers: [String]) { self.answers = answers }
    func next(_ endpoint: String) -> String? {
        lock.withLock {
            asked.append(endpoint)
            return answers.isEmpty ? nil : answers.removeFirst()
        }
    }
    var endpoints: [String] { lock.withLock { asked } }
}
