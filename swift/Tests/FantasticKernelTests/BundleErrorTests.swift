// KernelError wire-message format tests — parity with Rust kernel's
// Display impl ensures cross-runtime error envelopes match.

import Testing

@testable import FantasticKernel

@Suite("KernelError wire format")
struct BundleErrorTests {
    @Test func noAgentMessage() {
        let err = KernelError.noAgent(AgentId("ghost"))
        #expect(err.wireMessage == "no agent ghost")
    }

    @Test func noHandlerModuleMessage() {
        let err = KernelError.noHandlerModule(AgentId("bare"), verb: "render")
        #expect(err.wireMessage.contains("bare"))
        #expect(err.wireMessage.contains("render"))
    }

    @Test func noBundleForHandlerModuleMessage() {
        let err = KernelError.noBundleForHandlerModule("future.tools")
        #expect(err.wireMessage == "no bundle for handler_module \"future.tools\"")
    }

    @Test func invalidPayloadMessage() {
        let err = KernelError.invalidPayload("expected `text` string")
        #expect(err.wireMessage.contains("expected `text` string"))
    }

    @Test func equatable() {
        #expect(KernelError.noAgent(AgentId("a")) == KernelError.noAgent(AgentId("a")))
        #expect(KernelError.noAgent(AgentId("a")) != KernelError.noAgent(AgentId("b")))
    }
}
