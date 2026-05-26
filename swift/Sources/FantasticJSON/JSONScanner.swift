// Order-preserving JSON parser.
//
// Foundation's `JSONSerialization` builds objects as `[String: Any]`
// (Dictionary), which is hash-randomized and does NOT preserve key
// insertion order. To produce `JSON.object` values whose
// `OrderedDictionary` reflects the source byte order, we walk the
// bytes ourselves.
//
// Scope: RFC 8259 JSON. Matches what serde_json accepts at its
// default settings. We do NOT support JSON5 / comments / trailing
// commas — same as serde_json's strict mode.

import Foundation
import OrderedCollections

struct JSONScanner {
    private let bytes: [UInt8]
    var offset: Int

    init(data: Data) {
        self.bytes = Array(data)
        self.offset = 0
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

    mutating func parseValue() throws -> JSON {
        skipWhitespace()
        guard offset < bytes.count else {
            throw JSON.SerializationError.parseFailure(
                "unexpected end of input at offset \(offset)")
        }
        let b = bytes[offset]
        switch b {
        case 0x7B:  // '{'
            return try parseObject()
        case 0x5B:  // '['
            return try parseArray()
        case 0x22:  // '"'
            return .string(try parseString())
        case 0x74:  // 't'
            try expectLiteral("true")
            return .bool(true)
        case 0x66:  // 'f'
            try expectLiteral("false")
            return .bool(false)
        case 0x6E:  // 'n'
            try expectLiteral("null")
            return .null
        default:
            if b == 0x2D || (b >= 0x30 && b <= 0x39) {  // '-' or digit
                return try parseNumber()
            }
            throw JSON.SerializationError.parseFailure(
                "unexpected byte 0x\(String(b, radix: 16)) at offset \(offset)")
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

    private mutating func parseObject() throws -> JSON {
        offset += 1  // consume '{'
        var dict: OrderedDictionary<String, JSON> = [:]
        skipWhitespace()
        if offset < bytes.count, bytes[offset] == 0x7D {
            offset += 1
            return .object(dict)
        }
        while true {
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
            let value = try parseValue()
            dict[key] = value
            skipWhitespace()
            guard offset < bytes.count else {
                throw JSON.SerializationError.parseFailure(
                    "unterminated object at offset \(offset)")
            }
            let b = bytes[offset]
            if b == 0x7D {
                offset += 1
                return .object(dict)
            } else if b == 0x2C {
                offset += 1
                continue
            } else {
                throw JSON.SerializationError.parseFailure(
                    "expected ',' or '}' at offset \(offset)")
            }
        }
    }

    private mutating func parseArray() throws -> JSON {
        offset += 1  // consume '['
        var arr: [JSON] = []
        skipWhitespace()
        if offset < bytes.count, bytes[offset] == 0x5D {
            offset += 1
            return .array(arr)
        }
        while true {
            let value = try parseValue()
            arr.append(value)
            skipWhitespace()
            guard offset < bytes.count else {
                throw JSON.SerializationError.parseFailure(
                    "unterminated array at offset \(offset)")
            }
            let b = bytes[offset]
            if b == 0x5D {
                offset += 1
                return .array(arr)
            } else if b == 0x2C {
                offset += 1
                continue
            } else {
                throw JSON.SerializationError.parseFailure(
                    "expected ',' or ']' at offset \(offset)")
            }
        }
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
            } else {
                // Copy bytes until next significant char as UTF-8.
                let start = offset
                while offset < bytes.count, bytes[offset] != 0x22,
                    bytes[offset] != 0x5C
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

    private mutating func parseNumber() throws -> JSON {
        let start = offset
        var hasFraction = false
        var hasExponent = false
        if bytes[offset] == 0x2D { offset += 1 }
        while offset < bytes.count {
            let b = bytes[offset]
            if b >= 0x30 && b <= 0x39 {
                offset += 1
            } else if b == 0x2E {
                hasFraction = true
                offset += 1
            } else if b == 0x65 || b == 0x45 {
                hasExponent = true
                offset += 1
                if offset < bytes.count, bytes[offset] == 0x2B || bytes[offset] == 0x2D {
                    offset += 1
                }
            } else {
                break
            }
        }
        let slice = bytes[start..<offset]
        guard let str = String(bytes: slice, encoding: .ascii) else {
            throw JSON.SerializationError.parseFailure(
                "bad number bytes at offset \(start)")
        }
        if hasFraction || hasExponent {
            guard let d = Double(str) else {
                throw JSON.SerializationError.unsupportedNumber(str)
            }
            return .double(d)
        }
        if let i = Int64(str) {
            return .integer(i)
        }
        // Fallback to double for integers outside Int64 range.
        guard let d = Double(str) else {
            throw JSON.SerializationError.unsupportedNumber(str)
        }
        return .double(d)
    }
}
