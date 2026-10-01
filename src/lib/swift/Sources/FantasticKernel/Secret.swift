// SECRET VALUES on a call — the caller marks them, the kernel never records them.
//
// Port of python `kernel/_secret.py` (canonical, owner 2026-10-01).
//
// A batch item may carry `secret: [path, …]` beside `target`/`payload`/`timeout`.
// A path is dotted and rooted at the item: `payload.<field…>` names a field of the
// payload, `result` the whole reply, `result.<field…>` a field of it. The TARGET is
// handed the real payload and the CALLER the real reply; every place the kernel
// RECORDS or SHOWS the call carries `<secret>` instead — the call and settle rows
// (and so the memory store), the `ledger` read-back and dedup, the state stream's
// `payload` and `summary`, the inbox and watcher mirrors, and any error text the
// call's outcome carries.
//
// Two limits, by design:
//   * masking is not isolation — the receiver holds the value; send secrets to a
//     bundle that keeps them, never to an AI agent (they would enter its prompt and
//     its memory);
//   * a call whose record holds placeholders cannot be answered FROM that record:
//     re-sending a named execution with secrets after it left RAM is refused, loud.
//
// Not secret-able: `payload.type` — the verb is how every row is read.
//
// RAW BYTES. Python holds a binary item's body (and a binary reply's chunk) as a
// `bytes` value under the top-level `bytes` key of the payload (the result).
// Swift carries those bytes beside the JSON (`Item.binary`, `Outcome.body`), so
// every path walk here sees them where python does: `payload.bytes` names the
// item's blob, `result.bytes` the reply's body. A blob's secret VALUE is what
// python's `str(bytes)` spells — its `b'…'` repr.

import FantasticJSON
import Foundation
import OrderedCollections

/// What one item asked to be kept: the payload/result paths (as key lists), the
/// paths as given (recorded on the call row — paths are not secret), and the
/// payload's secret VALUES (RAM only — scrubbed out of every recorded string).
public struct Secret: Sendable {
    /// What every record and mirror carries in a secret value's place.
    public static let placeholder = "<secret>"

    /// The paths AS GIVEN — the call row's `secret` field.
    public let paths: [JSON]
    let payloadPaths: [[String]]
    let resultPaths: [[String]]
    let values: [String]

    /// Validate an item's `secret` (python `check`). Absent (or JSON null) ⇒
    /// nothing secret. A path the payload does not have is refused — a typo would
    /// otherwise record the value it meant to hide.
    static func check(_ raw: JSON?, payload: JSON, blob: Data?) -> LaunchField<Secret?> {
        guard let raw, raw != .null else { return .ok(nil) }
        func bad(_ why: String) -> LaunchField<Secret?> {
            .refused(
                Refusal.make(
                    Refusal.badArg, "send: secret \(why)", extra: [("arg", .string("secret"))]))
        }
        guard case .array(let list) = raw, !list.isEmpty else {
            return bad("must be a non-empty list of paths, got \(pyRepr(raw))")
        }
        let carried = Carried(json: payload, blob: blob)
        var payloadPaths: [[String]] = []
        var resultPaths: [[String]] = []
        for p in list {
            guard case .string(let s) = p, !s.isEmpty else {
                return bad("path must be a non-empty string, got \(pyRepr(p))")
            }
            // python `p.split(".")` — on the code point, never a grapheme.
            let keys = s.unicodeScalars.split(separator: ".", omittingEmptySubsequences: false)
                .map { String(String.UnicodeScalarView($0)) }
            if keys.contains(where: { $0.isEmpty }) {
                return bad("path \(pyRepr(s)) has an empty step")
            }
            if keys[0] == "result" {
                resultPaths.append(Array(keys.dropFirst()))
            } else if keys[0] == "payload" && keys.count > 1 {
                let kp = Array(keys.dropFirst())
                if kp == ["type"] {
                    return bad("cannot hide payload.type — the verb is how every row is read")
                }
                if carried.get(kp) == nil {
                    return bad("path \(pyRepr(s)) names nothing in the payload")
                }
                payloadPaths.append(kp)
            } else {
                return bad("path \(pyRepr(s)) must start with 'payload.' or be 'result[.…]'")
            }
        }
        let values = payloadPaths.flatMap { kp in carried.get(kp).map(secretStrings) ?? [] }
        return .ok(
            Secret(
                paths: list, payloadPaths: payloadPaths, resultPaths: resultPaths, values: values))
    }

    /// The payload as recorded and shown: secret fields → placeholder, then every
    /// secret value scrubbed out of every string (python `shown_payload`). A
    /// hidden `payload.bytes` drops the blob — the shown payload is text.
    func shownPayload(_ payload: JSON, blob: Data?) -> ShownPayload {
        var out = Carried(json: payload, blob: blob)
        for kp in payloadPaths { out = out.put(kp) }
        return ShownPayload(json: Secret.scrub(out.json, values), blob: out.blob)
    }

    /// An outcome as recorded and shown: secret result fields → placeholder, and
    /// every secret value (the payload's and the result's own) scrubbed from its
    /// text — the status string included, as python scrubs the whole outcome dict
    /// (python `shown_outcome`).
    func shownOutcome(status: String, result: JSON, body: Data?) -> ShownOutcome {
        var res = Carried(json: result, blob: body)
        var vals = values
        for kp in resultPaths {
            if let v = res.get(kp) { vals += secretStrings(v) }
            res = res.put(kp)
        }
        let order = Secret.longestFirst(vals)
        return ShownOutcome(
            status: Secret.scrubText(status, order), result: Secret.scrub(res.json, vals),
            body: res.blob)
    }

    /// The answer to re-sending an execution whose record holds placeholders
    /// (python `replay_refused`). `key` is the caller's name or the int id.
    static func replayRefused(_ key: JSON) -> JSON {
        Refusal.make(
            Refusal.badArg,
            "send: execution \(pyRepr(key)) carried secrets — its record holds "
                + "\(placeholder) placeholders, not values, so it cannot be answered from "
                + "the record; send again under a new id",
            extra: [("arg", .string("id"))])
    }

    /// Every occurrence of a secret value inside any string of `x` → placeholder —
    /// an error text that quotes the key must not carry it into a row or a log
    /// (python `scrub`). Dict KEYS are left alone, as python's are.
    static func scrub(_ x: JSON, _ values: [String]) -> JSON {
        if values.isEmpty { return x }
        return scrubOrdered(x, longestFirst(values))
    }

    private static func scrubOrdered(_ x: JSON, _ order: [[Unicode.Scalar]]) -> JSON {
        switch x {
        case .string(let s): return .string(scrubText(s, order))
        case .object(let o):
            var out = OrderedDictionary<String, JSON>()
            for (k, v) in o { out[k] = scrubOrdered(v, order) }
            return .object(out)
        case .array(let a): return .array(a.map { scrubOrdered($0, order) })
        default: return x
        }
    }

    /// python `sorted(values, key=len, reverse=True)`: longest (in CODE POINTS)
    /// first, ties kept in their given order — python's reverse sort is stable.
    private static func longestFirst(_ values: [String]) -> [[Unicode.Scalar]] {
        values.map { Array($0.unicodeScalars) }.enumerated()
            .sorted {
                $0.element.count != $1.element.count
                    ? $0.element.count > $1.element.count : $0.offset < $1.offset
            }
            .map(\.element)
    }

    /// python `for v in …: if v in x: x = x.replace(v, PLACEHOLDER)` — each value
    /// in turn, every non-overlapping occurrence left to right, on code points.
    private static func scrubText(_ s: String, _ order: [[Unicode.Scalar]]) -> String {
        var hay = Array(s.unicodeScalars)
        let mark = Array(placeholder.unicodeScalars)
        for needle in order where !needle.isEmpty && needle.count <= hay.count {
            var out: [Unicode.Scalar] = []
            out.reserveCapacity(hay.count)
            var i = 0
            var hit = false
            while i + needle.count <= hay.count {
                if hay[i..<(i + needle.count)].elementsEqual(needle) {
                    out.append(contentsOf: mark)
                    i += needle.count
                    hit = true
                } else {
                    out.append(hay[i])
                    i += 1
                }
            }
            if hit {
                out.append(contentsOf: hay[i...])
                hay = out
            }
        }
        var view = String.UnicodeScalarView()
        view.append(contentsOf: hay)
        return String(view)
    }
}

/// A payload as recorded and shown. `blob` is the item's raw body when it is NOT
/// secret (a watcher still receives it, as python's mirror carries the bytes);
/// `nil` when there was none or `payload.bytes` hid it.
struct ShownPayload: Sendable {
    let json: JSON
    let blob: Data?

    /// The fanout element — python publishes the shown dict, bytes and all.
    var inboxItem: InboxItem {
        blob.map { Kernel.binaryItem(json, $0) } ?? .text(json)
    }
}

/// An outcome as recorded and shown — what `ledger` reads back (python
/// `shown_out`). `body` is the reply chunk when it is NOT secret.
struct ShownOutcome: Sendable {
    let status: String
    let result: JSON
    let body: Data?
}

// ─── the walk ────────────────────────────────────────────────────────────────

/// What a path names: a JSON value, the raw blob, or the whole carried value.
private enum Found {
    case json(JSON)
    case blob(Data)
    case whole(Carried)
}

/// A value as python holds it: the JSON, plus — for a binary item or reply — the
/// raw bytes python keeps under its top-level `bytes` key.
private struct Carried {
    var json: JSON
    var blob: Data?

    /// The blob is at `bytes` only when the value is a dict, as python's is.
    private var hasBlob: Bool {
        if blob == nil { return false }
        if case .object = json { return true }
        return false
    }

    /// python `_get`: `nil` when the path names nothing. Empty keys = the whole.
    func get(_ keys: [String]) -> Found? {
        if keys.isEmpty { return .whole(self) }
        if hasBlob, keys[0] == "bytes" {
            // `_step(bytes, k)` — raw bytes have no fields.
            return keys.count == 1 ? .blob(blob!) : nil
        }
        var cur = json
        for k in keys {
            guard let next = stepInto(cur, k) else { return nil }
            cur = next
        }
        return .json(cur)
    }

    /// python `_put`: the value at `keys` → placeholder (a missing path leaves it
    /// as is); empty keys replace the whole value.
    func put(_ keys: [String]) -> Carried {
        if keys.isEmpty { return Carried(json: .string(Secret.placeholder), blob: nil) }
        guard get(keys) != nil else { return self }
        if hasBlob, keys[0] == "bytes", case .object(var o) = json {
            // in place when the header held the key, appended otherwise
            o["bytes"] = .string(Secret.placeholder)
            return Carried(json: .object(o), blob: nil)
        }
        return Carried(json: putInto(json, keys[...]), blob: blob)
    }
}

/// python `_step`: a dict key (present even when its value is null), or a list
/// index spelled in ASCII digits.
private func stepInto(_ cur: JSON, _ key: String) -> JSON? {
    switch cur {
    case .object(let o):
        return o[key]
    case .array(let a):
        guard !key.isEmpty, key.unicodeScalars.allSatisfy({ $0.value >= 0x30 && $0.value <= 0x39 }),
            let i = Int(key), i < a.count
        else { return nil }
        return a[i]
    default:
        return nil
    }
}

/// The JSON half of `_put`, the path already known to exist.
private func putInto(_ x: JSON, _ keys: ArraySlice<String>) -> JSON {
    guard let k = keys.first else { return .string(Secret.placeholder) }
    switch x {
    case .object(var o):
        guard let v = o[k] else { return x }
        o[k] = putInto(v, keys.dropFirst())
        return .object(o)
    case .array(var a):
        guard let i = Int(k), i < a.count else { return x }
        a[i] = putInto(a[i], keys.dropFirst())
        return .array(a)
    default:
        return x
    }
}

/// python `_strings`: every string a secret value holds — the value itself, or
/// its leaves; a number as `str(x)`; nothing for None or a bool.
private func secretStrings(_ f: Found) -> [String] {
    switch f {
    case .json(let j): return jsonStrings(j)
    case .blob(let d): return [pyBytesRepr(d)]
    case .whole(let c):
        guard let blob = c.blob, case .object(let o) = c.json else { return jsonStrings(c.json) }
        var out: [String] = []
        var placed = false
        for (k, v) in o {
            if k == "bytes" {
                out.append(pyBytesRepr(blob))
                placed = true
            } else {
                out += jsonStrings(v)
            }
        }
        if !placed { out.append(pyBytesRepr(blob)) }
        return out
    }
}

private func jsonStrings(_ x: JSON) -> [String] {
    switch x {
    case .string(let s): return s.isEmpty ? [] : [s]
    case .object(let o): return o.values.flatMap(jsonStrings)
    case .array(let a): return a.flatMap(jsonStrings)
    case .null, .bool: return []
    case .integer(let n): return [String(n)]
    case .double(let d): return [pyFloatRepr(d)]
    }
}

/// python `str(bytes)` / `repr(bytes)`: `b'…'`, single-quoted unless the bytes
/// hold a `'` and no `"`; the quote and `\` escaped; TAB/LF/CR as `\t` `\n` `\r`;
/// every other byte outside `0x20..<0x7f` as `\xhh`.
func pyBytesRepr(_ d: Data) -> String {
    let quote: UInt8 = (d.contains(0x27) && !d.contains(0x22)) ? 0x22 : 0x27
    var out = "b" + String(UnicodeScalar(quote))
    for b in d {
        switch b {
        case quote, 0x5C: out += "\\" + String(UnicodeScalar(b))
        case 0x09: out += "\\t"
        case 0x0A: out += "\\n"
        case 0x0D: out += "\\r"
        case 0x20..<0x7F: out += String(UnicodeScalar(b))
        default:
            let hex = String(b, radix: 16)
            out += "\\x" + (hex.count < 2 ? "0" + hex : hex)
        }
    }
    return out + String(UnicodeScalar(quote))
}
