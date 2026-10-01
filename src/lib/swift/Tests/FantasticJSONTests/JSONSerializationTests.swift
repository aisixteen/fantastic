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

    @Test func parsePrefixTakesTheValueAndReportsWhereItStopped() throws {
        // The counterpart of python's `raw_decode`, and its caller is the tool-call
        // parser: a model emits a complete object and then trailing junk (gpt-oss
        // closes an object early and writes one brace too many). `parse` refuses the
        // whole input; `parsePrefix` yields the value that IS there plus the offset
        // just past it, so the tail can be read rather than thrown away.
        let text = #"{"a":1},"reply_to":"self"}"#
        let (value, consumed) = try JSON.parsePrefix(Data(text.utf8))
        #expect(value.serialize() == #"{"a":1}"#)
        #expect(consumed == 7)
        let tail = String(decoding: Data(text.utf8).dropFirst(consumed), as: UTF8.self)
        #expect(tail == #","reply_to":"self"}"#)
    }

    @Test func parsePrefixSkipsLeadingButNotTrailingWhitespace() throws {
        // The offset must point at the first byte the VALUE did not claim, so the
        // caller sees the tail exactly as the model wrote it.
        let (value, consumed) = try JSON.parsePrefix(Data("  42  x".utf8))
        #expect(value.asInt == 42)
        #expect(consumed == 4)
    }

    @Test func parsePrefixStillThrowsWhenThePrefixItselfIsBroken() {
        #expect(throws: JSON.SerializationError.self) {
            _ = try JSON.parsePrefix(Data(#"{"a":"unclosed"#.utf8))
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

/// python `json.loads` (strict=True) refuses a raw control character in a
/// string and any number off the RFC grammar; `parseStrict` — the wire decode,
/// python's `io_bridge.loads` (owner D13) — also refuses what another runtime
/// would hold differently. Twin of `io_bridge/tests/test_codec.py`.
@Suite("JSON strictness (json.loads + io_bridge.loads)")
struct JSONStrictnessTests {
    @Test func rawControlCharactersAndOffGrammarNumbersAreNeverJSON() {
        for bad in [
            "\"a\u{01}b\"", "\"tab\there\"", "\"nl\nhere\"", "01", "-01", "[00]", "1.", "-",
            ".5", "1.e5", "1e", "1e+", "{\"x\": 012}",
        ] {
            #expect(throws: (any Error).self, "\(bad)") { try JSON.parse(bad) }
            #expect(throws: (any Error).self, "\(bad)") { try JSON.parseStrict(bad) }
        }
        // …and the grammar's own edges still read.
        #expect(try! JSON.parse("0") == .integer(0))
        #expect(try! JSON.parse("-0.5e-3") == .double(-0.0005))
        #expect(try! JSON.parse("10E2") == .double(1000))
        #expect(try! JSON.parse("\"\u{7f}\"") == .string("\u{7f}"))
    }

    @Test func onlyValuesEveryRuntimeCanHoldCrossTheWire() throws {
        for bad in [
            "{\"x\": 9223372036854775808}", "{\"x\": -9223372036854775809}",
            "{\"x\": 1e400}", "{\"x\": -1e400}", "{\"s\": \"\\udcff\"}", "{\"\\ud800\": 1}",
            "[[\"deep\", \"\\udfff\"]]", "{\"x\": NaN}", "{\"x\": Infinity}",
            "{\"x\": -Infinity}", "NaN",
        ] {
            #expect(throws: (any Error).self, "\(bad)") { try JSON.parseStrict(bad) }
        }
        // the int64 edges and a PAIRED surrogate (one real character) are fine
        #expect(
            try JSON.parseStrict("[9223372036854775807, -9223372036854775808]")
                == .array([.integer(Int64.max), .integer(Int64.min)]))
        #expect(try JSON.parseStrict("{\"s\": \"\\ud83d\\ude00\"}") == ["s": "\u{1F600}"])
        // Off the wire an over-int64 integer is still held, as a double.
        #expect(try JSON.parse("9223372036854775808") == .double(9223372036854775808))
    }
}
