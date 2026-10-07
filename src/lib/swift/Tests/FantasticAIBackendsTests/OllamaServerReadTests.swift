// A `model` an ollama record leaves out is READ FROM THE SERVER — python
// `tests/test_ollama_server_read.py`, case for case.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticOllamaBackend

private func tags(_ names: [String]) -> String {
    #"{"models": ["# + names.map { #"{"name": "\#($0)", "model": "\#($0)", "size": 1}"# }
        .joined(separator: ", ") + "]}"
}

private let noKey: KeyReader = { _ in nil }

private func auth(_ stub: StubHTTP) -> [String?] {
    stub.requests.map { r in
        r.split(separator: "\r\n").first { $0.lowercased().hasPrefix("authorization:") }
            .map { String($0.dropFirst("authorization:".count)).trimmingCharacters(in: .whitespaces) }
    }
}

@Suite("ollama: a left-out model is read from the server")
struct OllamaServerReadTests {

    @Test func theOneServedModelIsRead() async throws {
        let stub = StubHTTP([.init(body: tags(["qwen3:8b"]))])
        #expect(try await OllamaServerRead.readModel("http://127.0.0.1:\(stub.port)", key: noKey) == "qwen3:8b")
        #expect(stub.requests.first?.hasPrefix("GET /api/tags ") == true)
    }

    @Test func aServerWithoutOneModelRefusesNamingWhatItServes() {
        for (body, words) in [
            (tags(["a:1", "b:2"]), "serves a:1, b:2 — not one model; set model on the record"),
            (tags([]), "serves nothing — not one model; set model on the record"),
            ("<html>", "is not a model list"),
        ] {
            do {
                _ = try OllamaServerRead.servedModel(body, url: "u")
                Issue.record("no refusal for \(body)")
            } catch {
                #expect(error.message.contains(words), "\(error.message)")
            }
        }
    }

    @Test func aServerThatDoesNotAnswerRefusesNamingTheUrl() async {
        let stub = StubHTTP([.init(status: 404, body: "nope")])
        await #expect(
            throws: OllamaServerRead.Failure(
                message: "model: GET http://127.0.0.1:\(stub.port)/api/tags answered HTTP 404 — the "
                    + "server (or a door in front of it) does not serve it; set model on the record")
        ) { try await OllamaServerRead.readModel("http://127.0.0.1:\(stub.port)", key: noKey) }
        await #expect(
            throws: OllamaServerRead.Failure(
                message: "model: GET http://127.0.0.1:1/api/tags failed — the server is not reachable "
                    + "there; set model on the record")
        ) { try await OllamaServerRead.readModel("http://127.0.0.1:1", key: noKey) }
    }

    @Test func theKeyRidesTheModelListWhenTheRecordNamesOne() async throws {
        let stub = StubHTTP([.init(body: tags(["m"]))])
        _ = try await OllamaServerRead.readModel("http://127.0.0.1:\(stub.port)", key: { _ in "sekret" })
        #expect(auth(stub) == ["Bearer sekret"])
    }

    @Test func aRecordWithOnlyEndpointAndAStoreIsAccepted() {
        #expect(
            OllamaBackendBundle().validateMeta(
                ["endpoint": "http://o.test", "memory_store_id": "memory"], record: [:]) == nil)
        let fields = OllamaBackendBundle.RECORD_SCHEMA["fields"].asArray ?? []
        #expect(fields.first { $0["name"] == "model" }?["required"] == .bool(false))
        #expect(fields.first { $0["name"] == "credentials" }?["required"] == .bool(false))
    }

    // ─── through the kernel ─────────────────────────────────────

    private func kernel(_ endpoint: String) async -> Kernel {
        let registry = BundleRegistry()
        registry.register("ollama_backend.tools", OllamaBackendBundle())
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        let r = await k.send([Item(AgentId("kernel_state"), .object([
            "type": "create_agent", "parent_id": "kernel_state", "id": "o1",
            "handler_module": "ollama_backend.tools", "endpoint": .string(endpoint),
            "memory_store_id": "memory",
        ]))])[0].result
        #expect(r["id"] == "o1", "\(r)")
        return k
    }

    @Test func aSendReadsTheModelFirst() async {
        let stub = StubHTTP([.init(body: tags(["qwen3:8b"]))])
        let k = await kernel("http://127.0.0.1:\(stub.port)")
        _ = await k.send([Item(AgentId("o1"), ["type": "send", "text": "hi", "client_id": "t"])])[0]
        #expect(k.agent(AgentId("o1"))?.meta["model"] == .string("qwen3:8b"))
    }

    @Test func aSendTheServerCannotAnswerIsRefused() async {
        let stub = StubHTTP([.init(status: 404, body: "nope")])
        let k = await kernel("http://127.0.0.1:\(stub.port)")
        let r = await k.send([Item(AgentId("o1"), ["type": "send", "text": "hi", "client_id": "t"])])[0].result
        #expect(r["reason"] == "bad_arg" && r["arg"] == "model", "\(r)")
        #expect(k.agent(AgentId("o1"))?.meta["model"] == nil)
    }

    @Test func aVerbThatRunsNoTurnReadsNothing() async {
        let stub = StubHTTP([])
        let k = await kernel("http://127.0.0.1:\(stub.port)")
        for verb in ["reflect", "status", "context_status", "rows"] {
            _ = await k.send([Item(AgentId("o1"), .object(["type": .string(verb)]))])[0]
        }
        #expect(stub.requests.isEmpty)
    }
}
