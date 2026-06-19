// JSON value type.
//
// Mirrors Rust's `serde_json::Value` (with the `preserve_order`
// feature) — object keys retain insertion order on round-trip, so
// `agent.json` bytes match across Rust↔Swift cross-runtime testing.
//
// The kernel's substrate dispatch is fundamentally dynamic: every
// verb's payload is arbitrary JSON, switched on the `type` field at
// runtime. Typed Codable structs are used at boundaries (AgentRecord,
// KernelState) but the in-flight payload between bundles is `JSON`.

import Foundation
import OrderedCollections

/// One JSON value. Object keys are insertion-ordered; integers and
/// doubles are kept distinct to round-trip whole-number JSON literals
/// (`1` vs `1.0`) the way `serde_json` does.
public indirect enum JSON: Sendable, Hashable {
    case null
    case bool(Bool)
    case integer(Int64)
    case double(Double)
    case string(String)
    case array([JSON])
    case object(OrderedDictionary<String, JSON>)
}

// ── ExpressibleBy* literals ───────────────────────────────────────

extension JSON: ExpressibleByNilLiteral {
    public init(nilLiteral: ()) { self = .null }
}

extension JSON: ExpressibleByBooleanLiteral {
    public init(booleanLiteral value: Bool) { self = .bool(value) }
}

extension JSON: ExpressibleByIntegerLiteral {
    public init(integerLiteral value: Int64) { self = .integer(value) }
}

extension JSON: ExpressibleByFloatLiteral {
    public init(floatLiteral value: Double) { self = .double(value) }
}

extension JSON: ExpressibleByStringLiteral {
    public init(stringLiteral value: String) { self = .string(value) }
}

extension JSON: ExpressibleByArrayLiteral {
    public init(arrayLiteral elements: JSON...) { self = .array(elements) }
}

extension JSON: ExpressibleByDictionaryLiteral {
    public init(dictionaryLiteral elements: (String, JSON)...) {
        var dict: OrderedDictionary<String, JSON> = [:]
        for (k, v) in elements {
            dict[k] = v
        }
        self = .object(dict)
    }
}

// ── Accessors ─────────────────────────────────────────────────────

extension JSON {
    public var isNull: Bool {
        if case .null = self { return true }
        return false
    }

    public var asBool: Bool? {
        if case let .bool(v) = self { return v }
        return nil
    }

    public var asInt: Int64? {
        if case let .integer(v) = self { return v }
        // Permit lossless promotion from a whole-number double, mirroring
        // serde_json::Value::as_i64.
        if case let .double(v) = self, v.rounded() == v,
            v >= Double(Int64.min), v <= Double(Int64.max)
        {
            return Int64(v)
        }
        return nil
    }

    public var asDouble: Double? {
        if case let .double(v) = self { return v }
        if case let .integer(v) = self { return Double(v) }
        return nil
    }

    public var asString: String? {
        if case let .string(v) = self { return v }
        return nil
    }

    public var asArray: [JSON]? {
        if case let .array(v) = self { return v }
        return nil
    }

    public var asObject: OrderedDictionary<String, JSON>? {
        if case let .object(v) = self { return v }
        return nil
    }
}

// ── Subscripts ────────────────────────────────────────────────────

extension JSON {
    /// Object key lookup. Returns `.null` on miss or non-object —
    /// matches `serde_json::Value`'s `Value::null()` fallthrough.
    public subscript(key: String) -> JSON {
        get {
            if case let .object(dict) = self, let v = dict[key] { return v }
            return .null
        }
        set {
            guard case var .object(dict) = self else { return }
            dict[key] = newValue
            self = .object(dict)
        }
    }

    /// Array index lookup. Returns `.null` on out-of-bounds or
    /// non-array.
    public subscript(index: Int) -> JSON {
        get {
            if case let .array(arr) = self, index >= 0, index < arr.count {
                return arr[index]
            }
            return .null
        }
        set {
            guard case var .array(arr) = self, index >= 0, index < arr.count else {
                return
            }
            arr[index] = newValue
            self = .array(arr)
        }
    }
}
