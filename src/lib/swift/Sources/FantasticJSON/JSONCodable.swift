// JSON ↔ Codable + JSON ↔ raw bytes/string.
//
// Two paths:
//   1. `Codable` conformance — for embedding JSON values inside typed
//      Codable structs (e.g. `AgentRecord.meta: [String: JSON]`).
//   2. Direct serialization — `JSON.parse(_:)` / `JSON.serialize()`
//      for kernel↔wire round-trips. Bypasses JSONEncoder/Decoder
//      because Foundation reorders object keys on encode + can't
//      keep insertion order on decode without a custom container
//      walk.

import Foundation
import OrderedCollections

// ── Codable conformance ───────────────────────────────────────────

extension JSON: Codable {
    public init(from decoder: Decoder) throws {
        // Try in order: null, bool, integer, double, string, array, object.
        if let container = try? decoder.singleValueContainer() {
            if container.decodeNil() {
                self = .null
                return
            }
            if let v = try? container.decode(Bool.self) {
                self = .bool(v)
                return
            }
            if let v = try? container.decode(Int64.self) {
                self = .integer(v)
                return
            }
            if let v = try? container.decode(Double.self) {
                self = .double(v)
                return
            }
            if let v = try? container.decode(String.self) {
                self = .string(v)
                return
            }
        }
        if var container = try? decoder.unkeyedContainer() {
            var arr: [JSON] = []
            arr.reserveCapacity(container.count ?? 0)
            while !container.isAtEnd {
                arr.append(try container.decode(JSON.self))
            }
            self = .array(arr)
            return
        }
        if let container = try? decoder.container(keyedBy: AnyKey.self) {
            var dict: OrderedDictionary<String, JSON> = [:]
            for key in container.allKeys {
                dict[key.stringValue] = try container.decode(JSON.self, forKey: key)
            }
            self = .object(dict)
            return
        }
        throw DecodingError.dataCorrupted(
            DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "JSON: value matched no known variant"
            ))
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .null:
            var c = encoder.singleValueContainer()
            try c.encodeNil()
        case .bool(let v):
            var c = encoder.singleValueContainer()
            try c.encode(v)
        case .integer(let v):
            var c = encoder.singleValueContainer()
            try c.encode(v)
        case .double(let v):
            var c = encoder.singleValueContainer()
            try c.encode(v)
        case .string(let v):
            var c = encoder.singleValueContainer()
            try c.encode(v)
        case .array(let arr):
            var c = encoder.unkeyedContainer()
            for v in arr {
                try c.encode(v)
            }
        case .object(let dict):
            var c = encoder.container(keyedBy: AnyKey.self)
            for (k, v) in dict {
                try c.encode(v, forKey: AnyKey(stringValue: k))
            }
        }
    }
}

// ── Direct serialization (preserves object key order) ─────────────

extension JSON {
    public enum SerializationError: Error, Equatable, Sendable {
        case parseFailure(String)
        case unsupportedNumber(String)
        case unsupportedRoot(String)
    }

    /// Decode JSON bytes into a `JSON` tree, preserving object key
    /// insertion order. Uses `JSONSerialization` for parsing, then
    /// walks the resulting tree with a key-order-preserving traversal.
    ///
    /// Foundation's `JSONSerialization` parses objects into
    /// `[String: Any]` — Dictionary, NOT insertion-ordered. To
    /// preserve order we re-parse the source bytes ourselves into an
    /// OrderedDictionary tree.
    public static func parse(_ string: String) throws -> JSON {
        guard let data = string.data(using: .utf8) else {
            throw SerializationError.parseFailure("input is not valid UTF-8")
        }
        return try parse(data)
    }

    /// Decode JSON bytes into a `JSON` tree, preserving object key
    /// insertion order.
    public static func parse(_ data: Data) throws -> JSON {
        var scanner = JSONScanner(data: data)
        scanner.skipWhitespace()
        let value = try scanner.parseValue()
        scanner.skipWhitespace()
        guard scanner.isAtEnd else {
            throw SerializationError.parseFailure(
                "trailing content at offset \(scanner.offset)"
            )
        }
        return value
    }

    /// Serialize this `JSON` tree to a compact UTF-8 string,
    /// preserving object key insertion order. Output matches
    /// `serde_json::to_string` byte-for-byte for the values we
    /// support.
    public func serialize() -> String {
        var out = String()
        writeTo(&out)
        return out
    }

    /// Pretty-print with `indent` spaces per level, matching
    /// Python's `json.dumps(obj, indent=2)` byte-for-byte:
    ///   - `": "` between key and value
    ///   - `,\n<indent>` between items
    ///   - empty `{}` / `[]` stay compact (no inner newline)
    ///   - no trailing newline on the outermost value
    ///
    /// Used for on-disk persistence (`agent.json`) so the bytes
    /// match what the Python kernel writes for the same record.
    public func serializePretty(indent: Int = 2) -> String {
        var out = String()
        writeToPretty(&out, level: 0, indent: indent)
        return out
    }

    private func writeTo(_ out: inout String) {
        switch self {
        case .null:
            out.append("null")
        case .bool(let v):
            out.append(v ? "true" : "false")
        case .integer(let v):
            out.append(String(v))
        case .double(let v):
            // Format matching serde_json: finite → minimal decimal,
            // NaN/Infinity → null (serde_json refuses them; we mirror).
            if v.isNaN || v.isInfinite {
                out.append("null")
            } else if v.rounded() == v, abs(v) < 1e16 {
                // Whole-number doubles serialize as "1.0", matching
                // serde_json's f64 path.
                out.append("\(v)")
            } else {
                out.append("\(v)")
            }
        case .string(let v):
            JSON.writeEscapedString(v, to: &out)
        case .array(let arr):
            out.append("[")
            var first = true
            for item in arr {
                if !first { out.append(",") }
                first = false
                item.writeTo(&out)
            }
            out.append("]")
        case .object(let dict):
            out.append("{")
            var first = true
            for (k, v) in dict {
                if !first { out.append(",") }
                first = false
                JSON.writeEscapedString(k, to: &out)
                out.append(":")
                v.writeTo(&out)
            }
            out.append("}")
        }
    }

    /// Pretty-print writer mirroring CPython's `json.encoder`
    /// `indent=N` behavior. Item separators are `,` (no trailing
    /// space — Python writes `,\n<indent>`), key/value separator
    /// is `": "`.
    private func writeToPretty(_ out: inout String, level: Int, indent: Int) {
        switch self {
        case .null, .bool, .integer, .double, .string:
            // Scalars: same bytes as compact mode.
            writeTo(&out)
        case .array(let arr):
            if arr.isEmpty {
                out.append("[]")
                return
            }
            out.append("[\n")
            let childPad = String(repeating: " ", count: (level + 1) * indent)
            let parentPad = String(repeating: " ", count: level * indent)
            var first = true
            for item in arr {
                if !first { out.append(",\n") }
                first = false
                out.append(childPad)
                item.writeToPretty(&out, level: level + 1, indent: indent)
            }
            out.append("\n")
            out.append(parentPad)
            out.append("]")
        case .object(let dict):
            if dict.isEmpty {
                out.append("{}")
                return
            }
            out.append("{\n")
            let childPad = String(repeating: " ", count: (level + 1) * indent)
            let parentPad = String(repeating: " ", count: level * indent)
            var first = true
            for (k, v) in dict {
                if !first { out.append(",\n") }
                first = false
                out.append(childPad)
                JSON.writeEscapedString(k, to: &out)
                out.append(": ")
                v.writeToPretty(&out, level: level + 1, indent: indent)
            }
            out.append("\n")
            out.append(parentPad)
            out.append("}")
        }
    }

    /// JSON string escaping matching RFC 8259 (and serde_json's
    /// default compact escaper).
    private static func writeEscapedString(_ s: String, to out: inout String) {
        out.append("\"")
        for scalar in s.unicodeScalars {
            switch scalar {
            case "\"": out.append("\\\"")
            case "\\": out.append("\\\\")
            case "\u{08}": out.append("\\b")
            case "\u{0C}": out.append("\\f")
            case "\n": out.append("\\n")
            case "\r": out.append("\\r")
            case "\t": out.append("\\t")
            default:
                if scalar.value < 0x20 {
                    out.append(String(format: "\\u%04x", scalar.value))
                } else {
                    out.append(String(scalar))
                }
            }
        }
        out.append("\"")
    }
}

// ── Internal: dynamic CodingKey ───────────────────────────────────

struct AnyKey: CodingKey {
    var stringValue: String
    var intValue: Int? { nil }
    init(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { nil }
}
