// Parse / serialize round-trip tests + key-order preservation.

import OrderedCollections
import Testing

@testable import FantasticJSON

@Suite("JSON parse + serialize")
struct JSONSerializationTests {
    @Test func parsesScalars() throws {
        #expect(try JSON.parse("null") == .null)
        #expect(try JSON.parse("true") == .bool(true))
        #expect(try JSON.parse("false") == .bool(false))
        #expect(try JSON.parse("42") == .integer(42))
        #expect(try JSON.parse("-17") == .integer(-17))
        #expect(try JSON.parse("3.14") == .double(3.14))
        #expect(try JSON.parse("\"hello\"") == .string("hello"))
    }

    @Test func parsesArrays() throws {
        let j = try JSON.parse("[1, 2, 3]")
        #expect(j == .array([.integer(1), .integer(2), .integer(3)]))
    }

    @Test func parsesNestedObject() throws {
        let j = try JSON.parse(#"{"a":1,"b":{"c":2}}"#)
        #expect(j["a"].asInt == 1)
        #expect(j["b"]["c"].asInt == 2)
    }

    @Test func preservesObjectKeyOrder() throws {
        // The whole point — parse retains insertion order.
        let j = try JSON.parse(#"{"z":1,"a":2,"m":3}"#)
        guard case let .object(dict) = j else {
            Issue.record("expected object")
            return
        }
        #expect(Array(dict.keys) == ["z", "a", "m"])
    }

    @Test func serializeScalars() {
        #expect(JSON.null.serialize() == "null")
        #expect(JSON.bool(true).serialize() == "true")
        #expect(JSON.integer(42).serialize() == "42")
        #expect(JSON.string("x").serialize() == "\"x\"")
    }

    @Test func serializeObjectPreservesOrder() {
        let j: JSON = ["z": 1, "a": 2, "m": 3]
        // Insertion order z,a,m must be in the output.
        #expect(j.serialize() == #"{"z":1,"a":2,"m":3}"#)
    }

    @Test func serializeEscapesQuotesAndControlChars() {
        let j: JSON = "he said \"hi\"\nthen left"
        #expect(j.serialize() == "\"he said \\\"hi\\\"\\nthen left\"")
    }

    @Test func roundTripPreservesOrder() throws {
        let original = #"{"type":"register","name":"get_weather","agent_id":"weather"}"#
        let parsed = try JSON.parse(original)
        #expect(parsed.serialize() == original)
    }

    @Test func parseRejectsTrailingContent() {
        #expect(throws: JSON.SerializationError.self) {
            _ = try JSON.parse("42 extra")
        }
    }

    @Test func parseRejectsTruncatedString() {
        #expect(throws: JSON.SerializationError.self) {
            _ = try JSON.parse("\"unterminated")
        }
    }

    @Test func parsesUnicodeEscape() throws {
        let j = try JSON.parse(#""é""#)
        #expect(j.asString == "é")
    }

    @Test func parsesSurrogatePair() throws {
        // 😀 = U+1F600 = surrogate pair D83D DE00
        let j = try JSON.parse(#""😀""#)
        #expect(j.asString == "😀")
    }
}

@Suite("JSON Codable bridge")
struct JSONCodableBridgeTests {
    @Test func encodesViaJSONEncoder() throws {
        let j: JSON = ["x": 1, "y": 2]
        let data = try JSONEncoder().encode(j)
        let str = String(data: data, encoding: .utf8) ?? ""
        // JSONEncoder does NOT preserve order, so check membership only.
        #expect(str.contains("\"x\":1"))
        #expect(str.contains("\"y\":2"))
    }

    @Test func decodesViaJSONDecoder() throws {
        let str = #"{"a":42}"#
        let data = str.data(using: .utf8)!
        let j = try JSONDecoder().decode(JSON.self, from: data)
        #expect(j["a"].asInt == 42)
    }
}

import Foundation
