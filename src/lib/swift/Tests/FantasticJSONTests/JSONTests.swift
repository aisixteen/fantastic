// Constructor + accessor tests for the JSON enum.

import OrderedCollections
import Testing

@testable import FantasticJSON

@Suite("JSON constructors")
struct JSONConstructorTests {
    @Test func nullLiteral() {
        let j: JSON = nil
        #expect(j.isNull)
    }

    @Test func booleanLiteral() {
        #expect(JSON(true) == .bool(true))
        let j: JSON = false
        #expect(j == .bool(false))
    }

    @Test func integerLiteral() {
        let j: JSON = 42
        #expect(j == .integer(42))
        #expect(j.asInt == 42)
    }

    @Test func doubleLiteral() {
        let j: JSON = 3.14
        #expect(j == .double(3.14))
        #expect(j.asDouble == 3.14)
    }

    @Test func stringLiteral() {
        let j: JSON = "hello"
        #expect(j == .string("hello"))
        #expect(j.asString == "hello")
    }

    @Test func arrayLiteral() {
        let j: JSON = [1, "two", true]
        #expect(j == .array([.integer(1), .string("two"), .bool(true)]))
        #expect(j.asArray?.count == 3)
    }

    @Test func dictionaryLiteralPreservesOrder() {
        // Insertion order: a, b, c — must be preserved.
        let j: JSON = ["a": 1, "b": 2, "c": 3]
        guard case let .object(dict) = j else {
            Issue.record("expected object")
            return
        }
        #expect(Array(dict.keys) == ["a", "b", "c"])
    }
}

extension JSON {
    init(_ b: Bool) { self = .bool(b) }
}

@Suite("JSON accessors")
struct JSONAccessorTests {
    @Test func asIntFromInteger() {
        #expect(JSON.integer(7).asInt == 7)
    }

    @Test func asIntFromWholeDouble() {
        // Matches serde_json::Value::as_i64 — accepts whole doubles.
        #expect(JSON.double(7.0).asInt == 7)
    }

    @Test func asIntRejectsFractionalDouble() {
        #expect(JSON.double(7.5).asInt == nil)
    }

    @Test func asDoubleFromInteger() {
        #expect(JSON.integer(7).asDouble == 7.0)
    }

    @Test func subscriptObjectKeyMiss() {
        let j: JSON = ["a": 1]
        #expect(j["missing"].isNull)
    }

    @Test func subscriptObjectKeyHit() {
        let j: JSON = ["a": 1, "b": 2]
        #expect(j["b"].asInt == 2)
    }

    @Test func subscriptArrayIndex() {
        let j: JSON = ["x", "y", "z"]
        #expect(j[1].asString == "y")
        #expect(j[10].isNull)
        #expect(j[-1].isNull)
    }
}
