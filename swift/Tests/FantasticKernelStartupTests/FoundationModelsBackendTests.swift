// Skeleton-level integration tests for the Apple Foundation Models
// backend bundle.
//
// We can't exercise live `LanguageModelSession.respond(to:)` from
// the test runner because Apple FM requires macOS 26 + Apple
// Intelligence enabled (not present in CI). So the tests pin only
// what's invariant: the verb surface answers, the bundle is in
// the default registry, and the unavailable-path responses are
// well-shaped.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("FoundationModelsBackend")
struct FoundationModelsBackendTests {

    @Test func bundleIsRegisteredInDefaultSet() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let r = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("foundation_models_backend.tools"),
                "id": .string("fm"),
            ]))
        #expect(
            r["id"].asString == "fm",
            "expected fm agent created, got \(r.serialize())")
    }

    @Test func reflectReturnsExpectedShape() async throws {
        let kernel = try await freshKernelWithFm()
        let r = await kernel.send(AgentId("fm"), .object(["type": .string("reflect")]))
        #expect(r["kind"].asString == "foundation_models_backend")
        #expect(r["provider"].asString == "apple_foundation_models")
        #expect(r["verbs"]["send"].asString != nil)
        #expect(r["verbs"]["history"].asString != nil)
        #expect(r["verbs"]["interrupt"].asString != nil)
        #expect(r["verbs"]["backend_state"].asString != nil)
    }

    @Test func backendStateReportsAvailability() async throws {
        let kernel = try await freshKernelWithFm()
        let r = await kernel.send(AgentId("fm"), .object(["type": .string("backend_state")]))
        // Skeleton-level: we only require the keys exist + booleans
        // are present. Their values depend on whether the test host
        // actually has Apple Intelligence enabled.
        #expect(r["provider"].asString == "apple_foundation_models")
        #expect(r["apple_intelligence_available"].asBool != nil)
        #expect(r["model_available"].asBool != nil)
        #expect(r["backend_registered"].asBool == true)
        #expect(r["in_flight"].asInt != nil)
        #expect(r["reason"].asString != nil)
    }

    @Test func sendOnUnavailableHostReturnsError() async throws {
        let kernel = try await freshKernelWithFm()
        // On the CI / dev host (macOS < 26 OR no Apple Intelligence),
        // send should refuse with `foundation_models_unavailable`.
        // On a fully-enabled macOS 26 host this test would instead
        // observe `queued: true` — both shapes are documented.
        let r = await kernel.send(
            AgentId("fm"),
            .object([
                "type": .string("send"),
                "text": .string("hello"),
            ]))
        if r["queued"].asBool == true {
            #expect(r["stream_id"].asString != nil)
            #expect(r["message_id"].asString != nil)
        } else {
            #expect(r["error"].asString == "foundation_models_unavailable")
            #expect(r["reason"].asString != nil)
        }
    }

    @Test func historyOnFreshAgentIsEmpty() async throws {
        let kernel = try await freshKernelWithFm()
        let r = await kernel.send(
            AgentId("fm"),
            .object([
                "type": .string("history"),
                "client_id": .string("test"),
            ]))
        #expect(r["client_id"].asString == "test")
        #expect((r["messages"].asArray ?? []).isEmpty)
    }
}

/// Boot a kernel + create an `fm` agent ready for verb dispatch.
private func freshKernelWithFm() async throws -> Kernel {
    let kernel = try await startKernelInMemory(portHint: 0)
    _ = await kernel.send(
        AgentId("core"),
        .object([
            "type": .string("create_agent"),
            "handler_module": .string("foundation_models_backend.tools"),
            "id": .string("fm"),
        ]))
    return kernel
}
