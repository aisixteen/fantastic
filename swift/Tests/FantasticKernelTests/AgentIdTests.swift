// AgentId conformance tests.

import Testing

@testable import FantasticKernel

@Suite("AgentId")
struct AgentIdTests {
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
