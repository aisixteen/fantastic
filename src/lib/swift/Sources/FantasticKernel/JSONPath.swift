// python `io_bridge._codec.set_path` — a value placed at a path of STEPS.
//
// The ONE spelling of "where a raw body sits in an envelope": a binary frame's
// `_binary_path` (a JSON array) and the kernel's mirror of a call that carries
// bytes (`Blob`, whose `parts` are these steps). A string step is an object key,
// taken literally — it may hold `.`, be empty or look like a number, so `"a.b"`
// is ONE key, never `a` → `b`; an integer step is a list index. Every step is
// checked against the value it walks into, exactly as python's `set_path` checks
// a path off the wire. Rust `fantastic_bundle::path`.

import FantasticJSON

/// One step of a path: an object key (literal) or a list index.
public enum PathStep: Sendable, Hashable, ExpressibleByStringLiteral,
    ExpressibleByIntegerLiteral
{
    case key(String)
    case index(Int)

    public init(stringLiteral value: String) { self = .key(value) }
    public init(integerLiteral value: Int) { self = .index(value) }

    /// The step as `_binary_path` spells it: a key as a JSON string, an index
    /// as a JSON integer.
    public var json: JSON {
        switch self {
        case .key(let k): return .string(k)
        case .index(let i): return .integer(Int64(i))
        }
    }
}

/// What python's `set_path` raises (`ValueError`), as one line — the `<why>` of
/// `bad _binary_path <path>: <why>`.
public struct PathError: Error, CustomStringConvertible, Sendable, Equatable {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// python `set_path(obj, path, value)` over a path as it came off the wire — any
/// JSON value. Returns the steps it walked.
///
/// `path` must be a non-empty array. At an object the step is a string key:
/// every key before the last must exist, the last is set (kept in place when
/// present, appended when absent). At a list the step is a non-negative integer
/// index inside it (no negative indices, no bools, no floats, no digit strings).
/// Anything else throws, naming the step.
@discardableResult
public func pySetPath(_ obj: inout JSON, wire path: JSON, _ value: JSON) throws(PathError)
    -> [PathStep]
{
    guard case .array(let steps) = path else { throw PathError("not a list of steps") }
    guard !steps.isEmpty else { throw PathError("an empty path names no slot") }
    return try assignPath(&obj, steps[...], value)
}

/// `pySetPath` over typed steps — a `Blob`'s `parts`. The same checks.
public func pySetPath(_ obj: inout JSON, parts: [PathStep], _ value: JSON) throws(PathError) {
    try pySetPath(&obj, wire: .array(parts.map(\.json)), value)
}

/// Take the value at `parts` out of `obj`: an object key is REMOVED (python
/// `del d[k]`), a list slot becomes `null` (removing it would shift every later
/// index). The same checks as `pySetPath`, except that the last key must exist.
public func pyDelPath(_ obj: inout JSON, parts: [PathStep]) throws(PathError) {
    guard !parts.isEmpty else { throw PathError("an empty path names no slot") }
    try removePath(&obj, parts[...])
}

/// The JSON name of a value that is not a container, for an error (python
/// `_kind`).
private func jsonKind(_ v: JSON) -> String {
    switch v {
    case .null: return "null"
    case .bool: return "boolean"
    case .integer, .double: return "number"
    default: return "string"
    }
}

/// A step at an object: a string key, or the error naming it.
private func objectKey(_ step: JSON) throws(PathError) -> String {
    guard case .string(let k) = step else {
        throw PathError("step \(pyRepr(step)) at an object is not a string key")
    }
    return k
}

/// A step at a list of `count`: a non-negative integer index inside it.
private func listIndex(_ step: JSON, _ count: Int) throws(PathError) -> Int {
    guard case .integer(let n) = step, n >= 0 else {
        throw PathError("step \(pyRepr(step)) at a list is not a non-negative integer index")
    }
    guard n < count else {
        throw PathError("index \(n) is past the end of a list of \(count)")
    }
    return Int(n)
}

private func assignPath(_ obj: inout JSON, _ steps: ArraySlice<JSON>, _ value: JSON)
    throws(PathError) -> [PathStep]
{
    let step = steps[steps.startIndex]
    let rest = steps.dropFirst()
    switch obj {
    case .object(var members):
        let key = try objectKey(step)
        if rest.isEmpty {
            members[key] = value
            obj = .object(members)
            return [.key(key)]
        }
        // a missing intermediate is an error — nothing is created on the way
        guard var child = members[key] else { throw PathError("no key \(pyRepr(key))") }
        let tail = try assignPath(&child, rest, value)
        members[key] = child
        obj = .object(members)
        return [.key(key)] + tail
    case .array(var items):
        let idx = try listIndex(step, items.count)
        if rest.isEmpty {
            items[idx] = value
            obj = .array(items)
            return [.index(idx)]
        }
        var child = items[idx]
        let tail = try assignPath(&child, rest, value)
        items[idx] = child
        obj = .array(items)
        return [.index(idx)] + tail
    default:
        throw PathError("step \(pyRepr(step)) goes into a \(jsonKind(obj)), not a container")
    }
}

private func removePath(_ obj: inout JSON, _ parts: ArraySlice<PathStep>) throws(PathError) {
    let step = parts[parts.startIndex].json
    let rest = parts.dropFirst()
    switch obj {
    case .object(var members):
        let key = try objectKey(step)
        guard var child = members[key] else { throw PathError("no key \(pyRepr(key))") }
        if rest.isEmpty {
            members.removeValue(forKey: key)
        } else {
            try removePath(&child, rest)
            members[key] = child
        }
        obj = .object(members)
    case .array(var items):
        let idx = try listIndex(step, items.count)
        if rest.isEmpty {
            items[idx] = .null
        } else {
            var child = items[idx]
            try removePath(&child, rest)
            items[idx] = child
        }
        obj = .array(items)
    default:
        throw PathError("step \(pyRepr(step)) goes into a \(jsonKind(obj)), not a container")
    }
}
