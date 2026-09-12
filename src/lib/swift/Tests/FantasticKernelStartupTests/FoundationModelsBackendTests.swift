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
        let r = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("foundation_models_backend.tools"),
                "id": .string("fm"),
            ]))])[0].result
        #expect(
            r["id"].asString == "fm",
            "expected fm agent created, got \(r.serialize())")
    }

    @Test func reflectReturnsExpectedShape() async throws {
        let kernel = try await freshKernelWithFm()
        let r = await kernel.send([Item(AgentId("fm"), .object(["type": .string("reflect")]))])[0].result
        #expect(r["kind"].asString == "foundation_models_backend")
        #expect(r["provider"].asString == "apple_foundation_models")
        #expect(r["verbs"]["send"].asString != nil)
        #expect(r["verbs"]["history"].asString != nil)
        // The verb map is the shared ai_core one — python has no FM backend and
        // no `backend_state` verb anywhere.
        #expect(r["verbs"]["backend_state"].isNull)
        // No bundle-private `interrupt`: an in-flight `send` IS a kernel task,
        // so the backend names the substrate's `ps` + `cancel` — as a field.
        #expect(r["verbs"]["interrupt"].asString == nil)
        #expect(r["cancel"].asString?.contains("task_id") == true)
    }

    @Test func sendOnUnavailableHostReturnsError() async throws {
        let kernel = try await freshKernelWithFm()
        // On the CI / dev host (macOS < 26 OR no Apple Intelligence),
        // send should refuse with `foundation_models_unavailable`.
        // On a fully-enabled macOS 26 host it instead returns the completed
        // text — `send` runs the pass INLINE and answers `{response, final}`,
        // py/rust parity. Both shapes are documented.
        let r = await kernel.send([Item(AgentId("fm"), .object([
                "type": .string("send"),
                "text": .string("hello"),
            ]))])[0].result
        if r["error"].asString == "foundation_models_unavailable" {
            #expect(r["reason"].asString != nil)
        } else {
            #expect(r["response"].asString != nil)
            #expect(r["final"].asString == r["response"].asString)
            #expect(r["client_id"].asString != nil)
        }
    }

    @Test func historyOnFreshAgentIsEmpty() async throws {
        let kernel = try await freshKernelWithFm()
        let r = await kernel.send([Item(AgentId("fm"), .object([
                "type": .string("history"),
                "client_id": .string("test"),
            ]))])[0].result
        #expect((r["rows"].asArray ?? []).isEmpty && r["total"].asInt == 0, "\(r.serialize())")
    }
}

/// Boot a kernel + create an `fm` agent ready for verb dispatch.
private func freshKernelWithFm() async throws -> Kernel {
    let kernel = try await startKernelInMemory(portHint: 0)
    _ = await kernel.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("foundation_models_backend.tools"),
            "id": .string("fm"),
        ]))])[0].result
    return kernel
}
