// IO-3 — JSON nesting depth is bounded where python's wire loads refuses.
//
// The scanner used to recurse once per `[`/`{` with no bound: one 100k-deep
// frame overflowed the parsing thread's stack (an NIO event-loop thread, 512 KiB)
// and killed the whole kernel BEFORE any gate. Python never crashes there:
// CPython's json scanner charges every container it enters against
// `sys.getrecursionlimit()` (1000), and `io_bridge.loads` refuses with
// RecursionError "maximum recursion depth exceeded while decoding a JSON array
// from a unicode string" (`decode_frame` → "bad text frame: …"). Measured
// 2026-10-03, CPython 3.11, `loads` called with no caller frames above it: 994
// nested containers parse, the 995th is refused — the deepest python ever takes.

import Foundation
import Testing

@testable import FantasticJSON

@Suite("JSON nesting depth (io_bridge.loads bound)")
struct JSONDepthTests {
    static let arrayRefused = JSON.SerializationError.parseFailure(
        "maximum recursion depth exceeded while decoding a JSON array from a unicode string")
    static let objectRefused = JSON.SerializationError.parseFailure(
        "maximum recursion depth exceeded while decoding a JSON object from a unicode string")

    static func arrays(_ n: Int, closed: Bool = true) -> String {
        String(repeating: "[", count: n) + (closed ? String(repeating: "]", count: n) : "")
    }

    static func objects(_ n: Int) -> String {
        String(repeating: #"{"a":"#, count: n) + "null" + String(repeating: "}", count: n)
    }

    /// Every entry point, one refusal: `parse` (python `json.loads`), `parseStrict`
    /// (`io_bridge.loads`) and `parsePrefix` (`raw_decode`).
    static func expectRefused(_ text: String, _ refusal: JSON.SerializationError) {
        #expect(throws: refusal) { try JSON.parseStrict(text) }
        #expect(throws: refusal) { try JSON.parse(text) }
        #expect(throws: refusal) { try JSON.parsePrefix(Data(text.utf8)) }
    }

    /// THE FINDING: 100k nested arrays — open, and closed — are a parse
    /// failure, not a stack overflow.
    @Test func hundredThousandNestedArraysAreRefusedNotACrash() {
        Self.expectRefused(Self.arrays(100_000, closed: false), Self.arrayRefused)
        Self.expectRefused(Self.arrays(100_000), Self.arrayRefused)
    }

    @Test func hundredThousandNestedObjectsAreRefusedNotACrash() {
        Self.expectRefused(Self.objects(100_000), Self.objectRefused)
    }

    /// The bound is python's: 994 containers parse, the 995th is refused, and the
    /// refusal names the container that crossed it.
    @Test func depthBoundIsPythons() throws {
        let deepArray = try JSON.parseStrict(Self.arrays(994))
        var depth = 0
        var node = deepArray
        while case .array(let items) = node {
            depth += 1
            node = items.first ?? .null
        }
        #expect(depth == 994)
        #expect(try JSON.parse(Self.objects(994))["a"]["a"]["a"].asObject != nil)
        #expect(try JSON.parsePrefix(Data((Self.arrays(994) + " x").utf8)).consumed == 994 * 2)
        Self.expectRefused(Self.arrays(995), Self.arrayRefused)
        Self.expectRefused(Self.objects(995), Self.objectRefused)
        // Mixed: the 995th container is an object (odd positions are `{`).
        let mixed = String(repeating: #"{"a":["#, count: 497) + "{}" + String(repeating: "]}", count: 497)
        Self.expectRefused(mixed, Self.objectRefused)
        // An empty container counts: python enters it before it looks inside.
        Self.expectRefused(Self.arrays(994, closed: false) + "{}" + String(repeating: "]", count: 994),
            Self.objectRefused)
    }

    /// Depth is NESTING, not the container count: 100k sibling containers at
    /// depth 2 parse.
    @Test func siblingsAreNotDepth() throws {
        let wide = "[" + Array(repeating: "[]", count: 100_000).joined(separator: ",") + "]"
        #expect(try JSON.parseStrict(wide).asArray?.count == 100_000)
        let keys = (0..<10_000).map { #""k\#($0)":{}"# }.joined(separator: ",")
        #expect(try JSON.parseStrict("{" + keys + "}").asObject?.count == 10_000)
    }

    /// The other refusals keep their texts and order: the walk is the same walk.
    @Test func malformedContainersKeepTheirRefusals() {
        let cases: [(String, String)] = [
            ("[1", "unterminated array at offset 2"),
            ("[1 2]", "expected ',' or ']' at offset 3"),
            (#"{"a":1"#, "unterminated object at offset 6"),
            (#"{"a":1 "b":2}"#, "expected ',' or '}' at offset 7"),
            (#"{"a" 1}"#, "expected ':' after key at offset 5"),
            ("{1:2}", "expected string key at offset 1"),
            (#"{"a":1,}"#, "expected string key at offset 7"),
            ("[1,]", "unexpected byte 0x5d at offset 3"),
            ("[", "unexpected end of input at offset 1"),
            (#"{"a":"#, "unexpected end of input at offset 5"),
            ("[[]]x", "trailing content at offset 4"),
        ]
        for (text, why) in cases {
            #expect(throws: JSON.SerializationError.parseFailure(why), "\(text)") {
                try JSON.parseStrict(text)
            }
        }
    }
}
