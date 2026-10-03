// Order-preserving JSON parser.
//
// Foundation's `JSONSerialization` builds objects as `[String: Any]`
// (Dictionary), which is hash-randomized and does NOT preserve key
// insertion order. To produce `JSON.object` values whose
// `OrderedDictionary` reflects the source byte order, we walk the
// bytes ourselves.
//
// Scope: RFC 8259 JSON, as python's `json.loads` reads it (strict=True):
// a raw control character inside a string and a number off the RFC grammar
// (`01`, `1.`, `.5`, `1.e5`) are refused. No JSON5 / comments / trailing
// commas.
//
// `strict` adds what python's wire decoder (`io_bridge.loads`, owner D13)
// refuses on top: an integer outside int64 and a number that overflows a
// double — values another runtime would hold differently. (`NaN`/`Infinity`
// and lone surrogates are refused in every mode: this scanner never parses
// them.)
//
// NESTING (IO-3): containers are walked with an explicit stack, never by
// recursion, so no input reaches the thread's stack — a recursive walk let ONE
// 100k-deep frame overflow an NIO thread (512 KiB) and kill the kernel before
// any gate. Depth is bounded where python refuses: see `maxDepth`.

import Foundation
import OrderedCollections

struct JSONScanner {
    /// Python's json scanner charges every `[`/`{` it enters against
    /// `sys.getrecursionlimit()` (1000) and refuses past it with RecursionError
    /// "maximum recursion depth exceeded while decoding a JSON array|object from
    /// a unicode string". `io_bridge.loads` spends 6 of those frames itself, so
    /// 994 nested containers parse and the 995th is refused — measured 2026-10-03
    /// (CPython 3.11, `loads` called with no caller frames above it). That is the
    /// deepest python ever takes: every caller's own frames lower it (measured
    /// 963 through web_ws, 966 through web_rest), and a number leaf by 2 more.
    static let maxDepth = 994

    private let bytes: [UInt8]
    var offset: Int
    private let strict: Bool

    init(data: Data, strict: Bool = false) {
        self.bytes = Array(data)
        self.offset = 0
        self.strict = strict
    }

    var isAtEnd: Bool { offset >= bytes.count }

    mutating func skipWhitespace() {
        while offset < bytes.count {
            let b = bytes[offset]
            if b == 0x20 || b == 0x09 || b == 0x0A || b == 0x0D {
                offset += 1
            } else {
                break
            }
        }
    }

    /// One container still open: its members so far and, for an object, the
    /// key whose value is being read.
    private struct Open {
        let isObject: Bool
        var items: [JSON] = []
        var members: OrderedDictionary<String, JSON> = [:]
        var key: String = ""
    }

    /// One JSON value. Nested containers are held on `open`, not on the call
    /// stack; a value completes, is stored in the container above it, and that
    /// container then reads `,` (next member) or its closer (it completes too).
    mutating func parseValue() throws -> JSON {
        var open: [Open] = []
        while true {
            skipWhitespace()
            guard offset < bytes.count else {
                throw JSON.SerializationError.parseFailure(
                    "unexpected end of input at offset \(offset)")
            }
            var value: JSON
            let b = bytes[offset]
            switch b {
            case 0x7B, 0x5B:  // '{', '['
                let isObject = b == 0x7B
                // python enters the container (and charges the depth) before
                // it looks inside, so an empty one counts.
                guard open.count < Self.maxDepth else {
                    throw JSON.SerializationError.parseFailure(
                        "maximum recursion depth exceeded while decoding a JSON "
                            + (isObject ? "object" : "array") + " from a unicode string")
                }
                offset += 1
                skipWhitespace()
                if offset < bytes.count, bytes[offset] == (isObject ? 0x7D : 0x5D) {
                    offset += 1
                    value = isObject ? .object([:]) : .array([])
                } else {
                    var container = Open(isObject: isObject)
                    if isObject { container.key = try parseKey() }
                    open.append(container)
                    continue
                }
            case 0x22:  // '"'
                value = .string(try parseString())
            case 0x74:  // 't'
                try expectLiteral("true")
                value = .bool(true)
            case 0x66:  // 'f'
                try expectLiteral("false")
                value = .bool(false)
            case 0x6E:  // 'n'
                try expectLiteral("null")
                value = .null
            default:
                guard b == 0x2D || (b >= 0x30 && b <= 0x39) else {  // '-' or digit
                    throw JSON.SerializationError.parseFailure(
                        "unexpected byte 0x\(String(b, radix: 16)) at offset \(offset)")
                }
                value = try parseNumber()
            }
            // `value` is complete: store it in the container above, which then
            // either reads its next member or closes — completing in turn.
            while !open.isEmpty {
                let top = open.count - 1
                let isObject = open[top].isObject
                if isObject {
                    let key = open[top].key
                    open[top].members[key] = value
                } else {
                    open[top].items.append(value)
                }
                skipWhitespace()
                guard offset < bytes.count else {
                    throw JSON.SerializationError.parseFailure(
                        "unterminated \(isObject ? "object" : "array") at offset \(offset)")
                }
                let c = bytes[offset]
                if c == (isObject ? 0x7D : 0x5D) {  // '}' / ']'
                    offset += 1
                    let done = open.removeLast()
                    value = isObject ? .object(done.members) : .array(done.items)
                } else if c == 0x2C {  // ','
                    offset += 1
                    if isObject { open[top].key = try parseKey() }
                    break
                } else {
                    throw JSON.SerializationError.parseFailure(
                        "expected ',' or '\(isObject ? "}" : "]")' at offset \(offset)")
                }
            }
            if open.isEmpty { return value }
        }
    }

    private mutating func expectLiteral(_ literal: String) throws {
        let lbytes = Array(literal.utf8)
        guard offset + lbytes.count <= bytes.count else {
            throw JSON.SerializationError.parseFailure(
                "truncated literal \(literal) at offset \(offset)")
        }
        for (i, b) in lbytes.enumerated() {
            guard bytes[offset + i] == b else {
                throw JSON.SerializationError.parseFailure(
                    "expected literal \(literal) at offset \(offset)")
            }
        }
        offset += lbytes.count
    }

    /// An object member's key and its `:` — after `{` or a member's `,`.
    private mutating func parseKey() throws -> String {
        skipWhitespace()
        guard offset < bytes.count, bytes[offset] == 0x22 else {
            throw JSON.SerializationError.parseFailure(
                "expected string key at offset \(offset)")
        }
        let key = try parseString()
        skipWhitespace()
        guard offset < bytes.count, bytes[offset] == 0x3A else {
            throw JSON.SerializationError.parseFailure(
                "expected ':' after key at offset \(offset)")
        }
        offset += 1
        return key
    }

    private mutating func parseString() throws -> String {
        precondition(bytes[offset] == 0x22)
        offset += 1
        var out = String()
        out.reserveCapacity(16)
        while offset < bytes.count {
            let b = bytes[offset]
            if b == 0x22 {
                offset += 1
                return out
            }
            if b == 0x5C {  // '\\'
                offset += 1
                guard offset < bytes.count else {
                    throw JSON.SerializationError.parseFailure(
                        "truncated escape at offset \(offset)")
                }
                let esc = bytes[offset]
                offset += 1
                switch esc {
                case 0x22: out.append("\"")
                case 0x5C: out.append("\\")
                case 0x2F: out.append("/")
                case 0x62: out.append("\u{08}")
                case 0x66: out.append("\u{0C}")
                case 0x6E: out.append("\n")
                case 0x72: out.append("\r")
                case 0x74: out.append("\t")
                case 0x75:  // \uXXXX
                    let scalar = try parseHex4()
                    if (0xD800...0xDBFF).contains(scalar) {
                        // High surrogate — expect a low surrogate.
                        guard offset + 1 < bytes.count,
                            bytes[offset] == 0x5C, bytes[offset + 1] == 0x75
                        else {
                            throw JSON.SerializationError.parseFailure(
                                "lone high surrogate at offset \(offset)")
                        }
                        offset += 2
                        let low = try parseHex4()
                        guard (0xDC00...0xDFFF).contains(low) else {
                            throw JSON.SerializationError.parseFailure(
                                "invalid low surrogate at offset \(offset)")
                        }
                        let combined =
                            0x10000 + ((scalar - 0xD800) << 10)
                            + (low - 0xDC00)
                        guard let s = Unicode.Scalar(combined) else {
                            throw JSON.SerializationError.parseFailure(
                                "invalid combined scalar at offset \(offset)")
                        }
                        out.unicodeScalars.append(s)
                    } else {
                        guard let s = Unicode.Scalar(scalar) else {
                            throw JSON.SerializationError.parseFailure(
                                "invalid unicode escape at offset \(offset)")
                        }
                        out.unicodeScalars.append(s)
                    }
                default:
                    throw JSON.SerializationError.parseFailure(
                        "unknown escape \\\(Character(UnicodeScalar(esc))) at offset \(offset)"
                    )
                }
            } else if b < 0x20 {
                // python `json.loads` (strict=True): "Invalid control character".
                throw JSON.SerializationError.parseFailure(
                    "invalid control character at offset \(offset)")
            } else {
                // Copy bytes until next significant char as UTF-8.
                let start = offset
                while offset < bytes.count, bytes[offset] != 0x22,
                    bytes[offset] != 0x5C, bytes[offset] >= 0x20
                {
                    offset += 1
                }
                let slice = Array(bytes[start..<offset])
                guard let s = String(bytes: slice, encoding: .utf8) else {
                    throw JSON.SerializationError.parseFailure(
                        "invalid UTF-8 in string at offset \(start)")
                }
                out.append(s)
            }
        }
        throw JSON.SerializationError.parseFailure(
            "unterminated string at offset \(offset)")
    }

    private mutating func parseHex4() throws -> UInt32 {
        guard offset + 4 <= bytes.count else {
            throw JSON.SerializationError.parseFailure(
                "truncated \\u escape at offset \(offset)")
        }
        var value: UInt32 = 0
        for _ in 0..<4 {
            let b = bytes[offset]
            offset += 1
            let digit: UInt32
            switch b {
            case 0x30...0x39: digit = UInt32(b - 0x30)
            case 0x41...0x46: digit = UInt32(b - 0x41 + 10)
            case 0x61...0x66: digit = UInt32(b - 0x61 + 10)
            default:
                throw JSON.SerializationError.parseFailure(
                    "bad hex digit at offset \(offset)")
            }
            value = (value << 4) | digit
        }
        return value
    }

    /// RFC 8259 / python `NUMBER_RE`: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?`.
    /// Anything that leaves the grammar — a leading zero before a digit, a
    /// bare `.` or `e` — is refused: python's scanner stops there and the next
    /// byte is never valid where it lands.
    private mutating func parseNumber() throws -> JSON {
        let start = offset
        func isDigit(_ i: Int) -> Bool { i < bytes.count && bytes[i] >= 0x30 && bytes[i] <= 0x39 }
        func bad() -> JSON.SerializationError {
            .parseFailure("bad number at offset \(start)")
        }
        if bytes[offset] == 0x2D { offset += 1 }
        guard isDigit(offset) else { throw bad() }
        if bytes[offset] == 0x30 {
            offset += 1
            if isDigit(offset) { throw bad() }
        } else {
            while isDigit(offset) { offset += 1 }
        }
        var isFloat = false
        if offset < bytes.count, bytes[offset] == 0x2E {
            offset += 1
            guard isDigit(offset) else { throw bad() }
            while isDigit(offset) { offset += 1 }
            isFloat = true
        }
        if offset < bytes.count, bytes[offset] == 0x65 || bytes[offset] == 0x45 {
            offset += 1
            if offset < bytes.count, bytes[offset] == 0x2B || bytes[offset] == 0x2D { offset += 1 }
            guard isDigit(offset) else { throw bad() }
            while isDigit(offset) { offset += 1 }
            isFloat = true
        }
        let str = String(decoding: bytes[start..<offset], as: UTF8.self)
        if isFloat {
            guard let d = Double(str) else {
                throw JSON.SerializationError.unsupportedNumber(str)
            }
            // io_bridge `_finite`: a number that overflows a double.
            if strict && !d.isFinite {
                throw JSON.SerializationError.parseFailure("number \(str) overflows a double")
            }
            return .double(d)
        }
        if let i = Int64(str) {
            return .integer(i)
        }
        // io_bridge `_int64`: an integer outside int64.
        if strict {
            throw JSON.SerializationError.parseFailure("integer \(str) is outside int64")
        }
        // Outside the wire, an integer past Int64 is held as a double.
        guard let d = Double(str) else {
            throw JSON.SerializationError.unsupportedNumber(str)
        }
        return .double(d)
    }
}
