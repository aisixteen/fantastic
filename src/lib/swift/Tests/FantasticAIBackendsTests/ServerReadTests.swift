// What a vllm record leaves out is READ FROM THE SERVER — python
// `tests/test_server_read.py`, case for case. The bodies are the shapes measured on
// a live vLLM (the local 3090, 2026-10-07).

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticVllmBackend

private let MODEL = "qwen3.8-27b"

private func models(_ entries: [(String, Int)]) -> String {
    let data = entries.map { #"{"id": "\#($0.0)", "object": "model", "max_model_len": \#($0.1)}"# }
    return #"{"object": "list", "data": ["# + data.joined(separator: ", ") + "]}"
}

private func metrics(_ concurrency: [String]) -> String {
    var lines = ["# HELP vllm:cache_config_info Information of the LLMEngine CacheConfig"]
    for c in concurrency {
        lines.append(
            #"vllm:cache_config_info{block_size="432",engine="0",kv_cache_max_concurrency="\#(c)",kv_cache_size_tokens="74561",model_name="qwen3.8-27b"} 1.0"#
        )
    }
    lines.append(#"vllm:num_requests_running{engine="0",model_name="qwen3.8-27b"} 0.0"#)
    return lines.joined(separator: "\n") + "\n"
}

private let noKey: KeyReader = { _ in nil }
private let aKey: KeyReader = { _ in "sekret" }

private func rec(_ endpoint: String, _ extra: OrderedDictionary<String, JSON> = [:])
    -> OrderedDictionary<String, JSON>
{
    var r: OrderedDictionary<String, JSON> = [
        "endpoint": .string(endpoint), "kv_overflow": "evict", "memory_store_id": "memory",
    ]
    for (k, v) in extra { r[k] = v }
    return r
}

/// The request line's path of each request the stub saw.
private func paths(_ stub: StubHTTP) -> [String] {
    stub.requests.map { r in
        let line = r.split(separator: "\r\n").first.map(String.init) ?? r
        return line.split(separator: " ").dropFirst().first.map(String.init) ?? ""
    }
}

private func auth(_ stub: StubHTTP) -> [String?] {
    stub.requests.map { r in
        r.split(separator: "\r\n").first { $0.lowercased().hasPrefix("authorization:") }
            .map { String($0.dropFirst("authorization:".count)).trimmingCharacters(in: .whitespaces) }
    }
}

@Suite("vllm: what the record leaves out is read from the server")
struct ServerReadTests {

    @Test func whatTheRecordLeavesOutIsRead() async throws {
        let stub = StubHTTP([.init(body: models([(MODEL, 65536)])), .init(body: metrics(["1.1377245508982037"]))])
        let ep = "http://127.0.0.1:\(stub.port)/v1"
        let got = try await ServerRead.read(rec(ep), key: noKey)
        #expect(
            got == [
                "model": .string(MODEL), "max_tokens": .integer(65536),
                "context_window": .integer(65536), "kv_slots": .integer(1),
            ])
        #expect(paths(stub) == ["/v1/models", "/metrics"])
    }

    @Test func aFieldTheRecordSetsIsNeverRead() async throws {
        let stub = StubHTTP([.init(body: metrics(["1.1377245508982037"]))])
        let ep = "http://127.0.0.1:\(stub.port)/v1"
        let full = rec(ep, ["model": .string(MODEL), "max_tokens": 8000, "context_window": 262144, "kv_slots": 6])
        #expect(try await ServerRead.read(full, key: noKey).isEmpty)
        #expect(stub.requests.isEmpty)
        let partial = rec(ep, ["model": .string(MODEL), "max_tokens": 8000, "context_window": 262144])
        #expect(try await ServerRead.read(partial, key: noKey) == ["kv_slots": .integer(1)])
        #expect(paths(stub) == ["/metrics"])
    }

    @Test func theRecordsModelPicksItsEntryAmongSeveral() throws {
        let e = try ServerRead.servedModel(
            models([("other", 4096), (MODEL, 262144)]), model: MODEL, url: "u", field: "max_tokens")
        #expect(e["max_model_len"] == .integer(262144))
    }

    @Test func severalServedModelsAndNoModelRefuseNamingThem() {
        #expect(
            throws: ServerRead.Failure(
                field: "model",
                message: "model: http://h/v1/models serves a, b — not one model; set model on the record")
        ) {
            try ServerRead.servedModel(
                models([("a", 4096), ("b", 8192)]), model: nil, url: "http://h/v1/models", field: "model")
        }
    }

    @Test func aModelTheServerDoesNotServeIsRefused() {
        #expect(
            throws: ServerRead.Failure(
                field: "max_tokens",
                message: "max_tokens: u does not serve model 'nope' — it serves \(MODEL)")
        ) {
            try ServerRead.servedModel(models([(MODEL, 1)]), model: "nope", url: "u", field: "max_tokens")
        }
    }

    @Test func aDoorWithoutMetricsRefusesNamingKvSlots() async {
        let stub = StubHTTP([.init(status: 404, body: "not found")])
        let ep = "http://127.0.0.1:\(stub.port)/v1"
        let url = "http://127.0.0.1:\(stub.port)/metrics"
        await #expect(
            throws: ServerRead.Failure(
                field: "kv_slots",
                message: "kv_slots: GET \(url) answered HTTP 404 — the server (or a door in front of "
                    + "it) does not serve it; set kv_slots on the record")
        ) {
            try await ServerRead.read(
                rec(ep, ["model": .string(MODEL), "max_tokens": 8000, "context_window": 262144]), key: noKey)
        }
    }

    @Test func anUnreachableServerRefusesNamingTheField() async {
        await #expect(
            throws: ServerRead.Failure(
                field: "model",
                message: "model: GET http://127.0.0.1:1/v1/models failed — the server is not reachable "
                    + "there; set model on the record")
        ) {
            try await ServerRead.read(rec("http://127.0.0.1:1/v1"), key: noKey)
        }
    }

    @Test func kvSlotsIsTheFloorOfVllmsConcurrency() throws {
        for (c, want) in [("1.1377245508982037", 1), ("6.9", 6), ("3", 3)] {
            #expect(try ServerRead.kvSlots(metrics([c]), url: "u") == Int64(want), "\(c)")
        }
    }

    @Test func aMetricsPageThatNamesNoCountRefuses() {
        for (body, words) in [
            (metrics(["0.6"]), "holds 0.6 full-length conversations, under one"),
            (metrics(["2.0", "3.0"]), "carries 2 vllm:cache_config_info lines, not one"),
            (metrics([]), "carries 0 vllm:cache_config_info lines, not one"),
            (#"vllm:cache_config_info{block_size="432"} 1.0"# + "\n", "names no kv_cache_max_concurrency"),
        ] {
            do {
                _ = try ServerRead.kvSlots(body, url: "u")
                Issue.record("no refusal for \(body)")
            } catch {
                #expect(error.field == "kv_slots" && error.message.contains(words), "\(error.message)")
            }
        }
    }

    @Test func metricsUrlOnTheRecordWinsAndTheKeyStaysOnTheEndpointHost() async throws {
        let srv = StubHTTP([.init(body: models([(MODEL, 65536)])), .init(body: metrics(["2.0"]))])
        let msrv = StubHTTP([.init(body: metrics(["2.0"]))])
        let ep = "http://127.0.0.1:\(srv.port)/v1"
        // a metrics page on ANOTHER host (another port is another host)
        _ = try await ServerRead.read(
            rec(ep, ["metrics_url": .string("http://127.0.0.1:\(msrv.port)/metrics")]), key: aKey)
        #expect(auth(srv) == ["Bearer sekret"])
        #expect(auth(msrv) == [nil])
        // the same host gets the key
        _ = try await ServerRead.read(
            rec(ep, ["model": .string(MODEL), "max_tokens": 1, "context_window": 2]), key: aKey)
        #expect(paths(srv).last == "/metrics" && auth(srv).last == "Bearer sekret")
    }

    @Test func aKeyTheStoreRefusesRefusesNamingTheFirstField() async {
        let refused: KeyReader = { _ in throw PyError("CredentialError", "vllm_backend: the store refused the key") }
        await #expect(
            throws: ServerRead.Failure(field: "max_tokens", message: "vllm_backend: the store refused the key")
        ) {
            try await ServerRead.read(rec("http://127.0.0.1:1/v1", ["model": .string(MODEL)]), key: refused)
        }
    }

    @Test func metricsUrlIsTheEndpointsV1ReplacedByMetrics() {
        #expect(ServerRead.metricsURL(["endpoint": "http://h:10000/v1"]) == "http://h:10000/metrics")
        #expect(
            ServerRead.metricsURL(["endpoint": "https://d.net/llm/openai/v1/"])
                == "https://d.net/llm/openai/metrics")
        #expect(ServerRead.metricsURL(["endpoint": "http://h:10000/api"]) == nil)
        #expect(
            ServerRead.metricsURL(["endpoint": "http://h/api", "metrics_url": "http://m/x"]) == "http://m/x")
    }

    // ─── the write ──────────────────────────────────────────────

    @Test func aRecordWithOnlyEndpointAndKvOverflowIsAccepted() {
        #expect(VllmBundle().validateMeta(rec("http://vllm.test/v1"), record: [:]) == nil)
    }

    @Test func aWriteTheServerReadCannotHonourIsRefused() {
        let base = rec("http://vllm.test/v1")
        let cases: [(OrderedDictionary<String, JSON>, String)] = [
            (["context_window": "big"], "context_window must be a positive int"),
            (["context_window": 0], "context_window must be a positive int"),
            (["metrics_url": "vllm.test/metrics"], "metrics_url must be an http(s) URL"),
            (["metrics_url": 9100], "metrics_url must be an http(s) URL"),
            (
                ["endpoint": "http://h:10000/api"],
                "kv_slots is left to the server, but the endpoint 'http://h:10000/api' does not end in /v1"
            ),
        ]
        for (meta, fragment) in cases {
            let err = VllmBundle().validateMeta(meta, record: base) ?? ""
            #expect(err.contains(fragment), "\(meta): \(err)")
        }
    }

    // ─── through the kernel ─────────────────────────────────────

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
            "type": "create_agent", "parent_id": "kernel_state", "id": "v1",
            "handler_module": .string(HANDLER_MODULE),
        ]
        for (key, v) in meta { payload[key] = v }
        return await k.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
    }

    @Test func aSendReadsTheServerFirstAndReflectShowsTheValues() async throws {
        let stub = StubHTTP([.init(body: models([(MODEL, 65536)])), .init(body: metrics(["1.1377245508982037"]))])
        let k = kernel()
        let r = await create(k, rec("http://127.0.0.1:\(stub.port)/v1"))
        #expect(r["id"] == "v1", "\(r)")
        let before = await k.send([Item(AgentId("v1"), ["type": "reflect"])])[0].result
        #expect(before["max_tokens"] == .null, "\(before)")
        _ = await k.send([Item(AgentId("v1"), ["type": "send", "text": "hi", "client_id": "t"])])[0]
        let meta = k.agent(AgentId("v1"))?.meta ?? [:]
        #expect(meta["model"] == .string(MODEL) && meta["max_tokens"] == 65536, "\(meta)")
        #expect(meta["context_window"] == 65536 && meta["kv_slots"] == 1, "\(meta)")
        let after = await k.send([Item(AgentId("v1"), ["type": "reflect"])])[0].result
        #expect(after["max_tokens"] == 65536 && after["kv"]["slots"] == 1, "\(after)")
        #expect(Array(paths(stub).prefix(2)) == ["/v1/models", "/metrics"])
    }

    @Test func aSendTheServerCannotAnswerIsRefusedNamingTheField() async {
        let stub = StubHTTP([.init(status: 404, body: "not found")])
        let k = kernel()
        _ = await create(
            k, rec("http://127.0.0.1:\(stub.port)/v1", ["model": .string(MODEL), "max_tokens": 8000, "context_window": 8192]))
        let r = await k.send([Item(AgentId("v1"), ["type": "send", "text": "hi", "client_id": "t"])])[0].result
        #expect(r["reason"] == "bad_arg" && r["arg"] == "kv_slots", "\(r)")
        #expect(r["error"].asString?.contains("answered HTTP 404") == true, "\(r)")
        #expect(k.agent(AgentId("v1"))?.meta["kv_slots"] == nil)
        #expect(paths(stub) == ["/metrics"])
    }

    @Test func aVerbThatRunsNoTurnReadsNothing() async {
        let stub = StubHTTP([])
        let k = kernel()
        _ = await create(k, rec("http://127.0.0.1:\(stub.port)/v1"))
        for verb in ["reflect", "status", "context_status", "rows"] {
            _ = await k.send([Item(AgentId("v1"), .object(["type": .string(verb)]))])[0]
        }
        #expect(stub.requests.isEmpty)
        #expect(k.agent(AgentId("v1"))?.meta["kv_slots"] == nil)
    }

    @Test func anEndpointWithoutV1PassesWithMetricsUrlOrKvSlots() {
        let base = rec("http://h:10000/api")
        #expect(VllmBundle().validateMeta(["metrics_url": "http://h:10000/metrics"], record: base) == nil)
        #expect(VllmBundle().validateMeta(["kv_slots": 3], record: base) == nil)
    }
}
