// AgentId conformance tests.

import Testing

@testable import FantasticKernel

@Suite("AgentId")
struct AgentIdTests {
    /// PYTHON'S MINT, BRANCH FOR BRANCH (`_agent.py::create`):
    /// `handler_module.split(".")[-2] if "." in handler_module else
    /// handler_module`, then `f"{bundle}_{secrets.token_hex(3)}"`. A DOTLESS
    /// module is not an error — it is the prefix itself; a trailing dot still
    /// takes the second-to-last component, which is the empty-tail case python
    /// produces too.
    @Test func mintIdTakesPythonsBundleComponent() {
        for (module, prefix) in [
            ("terminal_backend.tools", "terminal_backend"),
            ("a.b.c", "b"),
            ("noop", "noop"),
            ("a.", "a"),
        ] {
            let id = mintAgentId(module)
            #expect(id.hasPrefix(prefix + "_"), "\(module) → \(id)")
            let hex = id.dropFirst(prefix.count + 1)
            #expect(hex.count == 6)
            #expect(hex.allSatisfy { $0.isHexDigit && !$0.isUppercase })
        }
    }

    @Test func stringLiteralConstructor() {
        let a: AgentId = "weather"
        #expect(a.value == "weather")
        #expect(a.asString == "weather")
    }

    @Test func descriptionMatchesValue() {
        let a = AgentId("chat")
        #expect(a.description == "chat")
        #expect("\(a)" == "chat")
    }

    @Test func equatable() {
        #expect(AgentId("a") == AgentId("a"))
        #expect(AgentId("a") != AgentId("b"))
    }

    @Test func hashable() {
        let set: Set<AgentId> = ["a", "b", "a"]
        #expect(set.count == 2)
    }

    @Test func codableSingleValueString() throws {
        let a = AgentId("weather")
        let data = try JSONEncoder().encode(a)
        let str = String(data: data, encoding: .utf8)
        #expect(str == "\"weather\"")
        let decoded = try JSONDecoder().decode(AgentId.self, from: data)
        #expect(decoded == a)
    }
}

import Foundation
