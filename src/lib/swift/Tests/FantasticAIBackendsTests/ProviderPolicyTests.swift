// What each provider does at the wire, pinned against python's own policy.
//
// These are the B4 items: what a missing field SAYS and which URL the record's
// `endpoint` builds. The stub (`StubHTTP`) is a loopback listener — python
// reaches the same places with an httpx transport seam.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticOllamaBackend

/// Drain a provider stream, returning the tokens it yielded and the error it
/// ended with (if any) — a failing turn's TOKENS matter as much as its error.
private func drain(_ stream: AsyncThrowingStream<AIChunk, Error>) async -> (
    tokens: [String], error: Error?
) {
    var tokens: [String] = []
    do {
        for try await chunk in stream {
            if case .token(let t) = chunk { tokens.append(t) }
        }
        return (tokens, nil)
    } catch {
        return (tokens, error)
    }
}

private let oneUser: [JSON] = [.object(["role": .string("user"), "content": .string("hi")])]

@Suite("Provider policy")
struct ProviderPolicyTests {

    // ── B4.6: the ollama model and endpoint ──────────────────────────────

    /// An unset model FAILS LOUD when the provider is built, naming the field —
    /// python's `required_meta`.
    @Test func ollamaUnsetModelFailsLoudWithPythonText() async {
        let bundle = OllamaBackendBundle()
        let agent = Agent(
            id: AgentId("ol"), handlerModule: nil, parentId: nil,
            meta: ["endpoint": .string("http://127.0.0.1:9")])
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        do {
            _ = try await bundle.core.config.makeProvider(agent, "c", kernel)
            Issue.record("an unset model must refuse the provider")
        } catch {
            #expect(
                "\(error)"
                    == "ValueError: ollama_backend: `model` is required on the agent record — "
                    + "nothing is guessed (update_agent <id> model=…)")
        }
    }

    /// The record's `endpoint` is where ollama listens; `/api/chat` is appended.
    /// Swift read a `host` meta and ignored `endpoint` entirely.
    @Test func ollamaRecordEndpointBuildsApiChatUrl() async {
        let stub = StubHTTP([.init(body: #"{"message":{"content":"ok"},"done":true}"#)])
        defer { stub.stop() }
        let bundle = OllamaBackendBundle()
        let agent = Agent(
            id: AgentId("ol"), handlerModule: nil, parentId: nil,
            meta: ["model": .string("m"), "endpoint": .string("http://127.0.0.1:\(stub.port)")])
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        guard let provider = try? await bundle.core.config.makeProvider(agent, "c", kernel) else {
            Issue.record("unexpected refusal")
            return
        }
        _ = await drain(provider.chat(messages: oneUser))
        #expect(
            stub.requests.first?.hasPrefix("POST /api/chat HTTP") == true,
            Comment(rawValue: stub.requests.first ?? "no request"))
    }
}
